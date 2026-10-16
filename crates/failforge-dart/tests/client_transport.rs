use failforge_dart::{AdapterClient, AdapterCommand, ClientError, ClientOptions, ProtocolError};
use failforge_runner::{CancelReason, CommandSpec, CompletionCause, Runner};
use serde_json::json;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

fn spec(mode: &str) -> CommandSpec {
    CommandSpec {
        program: env!("CARGO_BIN_EXE_failforge-adapter-fixture").into(),
        args: vec![mode.into()],
        cwd: std::env::current_dir().unwrap(),
        env: Default::default(),
        timeout_policy: Default::default(),
        capture_policy: Default::default(),
    }
}
fn options() -> ClientOptions {
    ClientOptions {
        request_timeout: Duration::from_secs(2),
        max_line_bytes: 512,
    }
}
async fn close_bounded(
    client: AdapterClient,
) -> Result<Arc<failforge_runner::CommandResult>, ClientError> {
    tokio::time::timeout(Duration::from_secs(5), client.close())
        .await
        .expect("external client-close deadline")
}
async fn shutdown(runner: &Runner) {
    tokio::time::timeout(Duration::from_secs(5), runner.shutdown())
        .await
        .expect("external runner-shutdown deadline")
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_requests_remote_errors_and_shared_runner_admission_are_independent() {
    let runner = Arc::new(Runner::new());
    let mut client = AdapterClient::start(runner.clone(), spec("normal"), options()).unwrap();
    assert_eq!(client.handshake().await.unwrap()["sequence"], 1);
    assert!(matches!(
        client
            .request(AdapterCommand::Discover, json!({"error":true}))
            .await,
        Err(ClientError::Remote(_))
    ));
    assert_eq!(client.validate(json!({})).await.unwrap()["sequence"], 3);
    let result = close_bounded(client).await.unwrap();
    assert!(result.status.unwrap().success());
    assert!(result.cleanup.is_complete());
    // Closing one session does not close Runner admission or its global owner.
    let mut second = AdapterClient::start(runner.clone(), spec("normal"), options()).unwrap();
    second.handshake().await.unwrap();
    close_bounded(second).await.unwrap();
    shutdown(&runner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_version_identity_size_and_eof_fail_closed() {
    for mode in ["wrong-id", "wrong-version", "malformed", "oversize", "eof"] {
        let runner = Arc::new(Runner::new());
        let mut client = AdapterClient::start(runner.clone(), spec(mode), options()).unwrap();
        let error = client.handshake().await.unwrap_err();
        assert!(
            match mode {
                "wrong-id" => matches!(
                    error,
                    ClientError::Protocol(ProtocolError::RequestIdMismatch)
                ),
                "wrong-version" =>
                    matches!(error, ClientError::Protocol(ProtocolError::VersionMismatch)),
                "malformed" => matches!(error, ClientError::Json(_)),
                "oversize" => matches!(error, ClientError::LineTooLarge),
                "eof" => matches!(error, ClientError::UnexpectedEof),
                _ => false,
            },
            "mode={mode}: {error}"
        );
        assert!(matches!(
            client.handshake().await,
            Err(ClientError::InterruptedRequest)
        ));
        let _ = close_bounded(client).await;
        shutdown(&runner).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_after_last_response_is_rejected_at_close_after_cleanup() {
    let runner = Arc::new(Runner::new());
    let mut client =
        AdapterClient::start(runner.clone(), spec("extra-on-close"), options()).unwrap();
    client.handshake().await.unwrap();
    assert!(matches!(
        close_bounded(client).await,
        Err(ClientError::UnsolicitedResponse)
    ));
    shutdown(&runner).await;
}

static NEXT_CONTROL: AtomicU64 = AtomicU64::new(0);
struct Control {
    path: PathBuf,
    listener: tokio::net::UnixListener,
    stream: Option<tokio::net::UnixStream>,
}
impl Control {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/tmp/ff-adapter-{}-{}.sock",
            std::process::id(),
            NEXT_CONTROL.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        Self {
            path,
            listener,
            stream: None,
        }
    }
    async fn request_observed(&mut self) {
        use tokio::io::AsyncReadExt;
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), self.listener.accept())
            .await
            .unwrap()
            .unwrap();
        self.stream = Some(stream);
        let mut line = Vec::new();
        let mut lines = 0;
        loop {
            let byte = tokio::time::timeout(
                Duration::from_secs(2),
                self.stream.as_mut().unwrap().read_u8(),
            )
            .await
            .unwrap()
            .unwrap();
            line.push(byte);
            assert!(line.len() < 128);
            if byte == b'\n' {
                lines += 1;
                if lines == 1 {
                    assert!(line.starts_with(b"READY "));
                } else {
                    assert!(line.starts_with(b"REQUEST rust-1"));
                    break;
                }
                line.clear();
            }
        }
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.stream.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_request_future_poisoning_does_not_cancel_another_session() {
    let runner = Arc::new(Runner::new());
    let mut control = Control::new();
    let mut waiting = spec("hang");
    waiting.env.set.insert(
        "FAILFORGE_ADAPTER_CONTROL".into(),
        control.path.clone().into_os_string(),
    );
    let mut client = AdapterClient::start(runner.clone(), waiting, options()).unwrap();
    let mut other = AdapterClient::start(runner.clone(), spec("normal"), options()).unwrap();
    other.handshake().await.unwrap();
    let mut pending = Box::pin(client.handshake());
    tokio::select! {result=&mut pending=>panic!("hang fixture unexpectedly responded: {result:?}"),_=control.request_observed()=>{}}
    drop(pending);
    assert!(matches!(
        client.handshake().await,
        Err(ClientError::InterruptedRequest)
    ));
    let _ = close_bounded(client).await;
    assert_eq!(other.discover(json!({})).await.unwrap()["sequence"], 2);
    close_bounded(other).await.unwrap();
    shutdown(&runner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_deadline_stops_its_owned_process() {
    let runner = Arc::new(Runner::new());
    let mut client = AdapterClient::start(
        runner.clone(),
        spec("hang"),
        ClientOptions {
            request_timeout: Duration::from_millis(30),
            ..options()
        },
    )
    .unwrap();
    assert!(matches!(
        client.handshake().await,
        Err(ClientError::TimedOut)
    ));
    let _ = close_bounded(client).await;
    shutdown(&runner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_request_close_keeps_actual_cleanup_evidence() {
    let runner = Arc::new(Runner::new());
    let mut control = Control::new();
    let mut waiting = spec("hang-until-signal");
    waiting.env.set.insert(
        "FAILFORGE_ADAPTER_CONTROL".into(),
        control.path.clone().into_os_string(),
    );
    let mut client = AdapterClient::start(runner.clone(), waiting, options()).unwrap();
    let mut pending = Box::pin(client.handshake());
    tokio::select! {
        result = &mut pending => panic!("quiet adapter unexpectedly responded: {result:?}"),
        _ = control.request_observed() => {}
    }
    drop(pending);
    let closed = close_bounded(client).await;
    shutdown(&runner).await;
    assert!(
        closed.is_ok(),
        "interrupted close lost cleanup evidence: {closed:?}"
    );
    let result = closed.unwrap();
    assert!(result.cleanup.is_complete());
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert_eq!(result.status.unwrap().signal(), Some(libc::SIGTERM));
    assert!(
        result
            .cleanup
            .signals
            .iter()
            .all(|record| record.signal != libc::SIGKILL)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_noncooperative_adapter_escalates_after_its_grace_period() {
    let runner = Arc::new(Runner::new());
    let mut control = Control::new();
    let mut waiting = spec("hang-ignore-term");
    waiting.timeout_policy.grace_period = Duration::from_millis(150);
    waiting.env.set.insert(
        "FAILFORGE_ADAPTER_CONTROL".into(),
        control.path.clone().into_os_string(),
    );
    let mut client = AdapterClient::start(runner.clone(), waiting, options()).unwrap();
    let mut pending = Box::pin(client.handshake());
    tokio::select! {
        result = &mut pending => panic!("noncooperative adapter responded: {result:?}"),
        _ = control.request_observed() => {}
    }
    drop(pending);
    let started = std::time::Instant::now();
    let closed = close_bounded(client).await;
    shutdown(&runner).await;
    let result = closed.unwrap();
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert_eq!(result.status.unwrap().signal(), Some(libc::SIGKILL));
    assert!(result.cleanup.is_complete());
    let forced = result
        .cleanup
        .signals
        .iter()
        .find(|record| record.signal == libc::SIGKILL)
        .unwrap();
    assert!(forced.after_spawn >= Duration::from_millis(150));
    assert!(started.elapsed() < Duration::from_secs(2));
}
