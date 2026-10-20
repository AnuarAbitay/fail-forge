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
    assert!(result.cleanup.is_complete(), "cleanup: {result:?}");
    // Closing one session does not close Runner admission or its global owner.
    let mut second = AdapterClient::start(runner.clone(), spec("normal"), options()).unwrap();
    second.handshake().await.unwrap();
    close_bounded(second).await.unwrap();
    shutdown(&runner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_version_identity_size_and_eof_fail_closed() {
    for mode in [
        "wrong-id",
        "wrong-version",
        "malformed",
        "oversize",
        "eof",
        "duplicate-nested",
    ] {
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
                "duplicate-nested" => matches!(error, ClientError::InvalidMetadata),
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
        let closed = tokio::time::timeout(Duration::from_secs(5), client.close_with_result())
            .await
            .unwrap();
        let result = closed.result.unwrap();
        assert!(result.cleanup.child_reaped);
        assert!(result.stdout.eof && result.stderr.eof);
        shutdown(&runner).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdout_after_last_response_is_rejected_at_close_after_cleanup() {
    let runner = Arc::new(Runner::new());
    let mut client =
        AdapterClient::start(runner.clone(), spec("extra-on-close"), options()).unwrap();
    client.handshake().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), client.close_with_result())
        .await
        .unwrap();
    assert!(matches!(
        closed.outcome,
        Err(ClientError::UnsolicitedResponse)
    ));
    let result = closed.result.unwrap();
    assert!(result.cleanup.is_complete());
    assert!(result.stdout.eof && result.stderr.eof);
    assert!(result.stdout.sha256.is_some() && result.stderr.sha256.is_some());
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

    async fn response_observed(&mut self) {
        use tokio::io::AsyncReadExt;
        let mut line = Vec::new();
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
                assert_eq!(line, b"RESPONSE\n");
                return;
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
    let closed = tokio::time::timeout(Duration::from_secs(5), client.close_with_result())
        .await
        .unwrap();
    let result = closed.result.unwrap();
    assert!(result.cleanup.child_reaped);
    assert!(result.stdout.eof && result.stderr.eof);
    shutdown(&runner).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_deadline_retains_capture_receipt_after_forced_owned_cleanup() {
    let runner = Arc::new(Runner::new());
    let mut specification = spec("hang-on-close");
    specification.timeout_policy.grace_period = Duration::from_millis(50);
    specification.timeout_policy.forced_wait = Duration::from_millis(100);
    specification.timeout_policy.output_drain = Duration::from_millis(50);
    let mut client = AdapterClient::start(
        runner.clone(),
        specification,
        ClientOptions {
            request_timeout: Duration::from_secs(1),
            ..options()
        },
    )
    .unwrap();
    client.handshake().await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), client.close_with_result())
        .await
        .unwrap();
    shutdown(&runner).await;
    assert!(matches!(closed.outcome, Err(ClientError::TimedOut)));
    let result = closed.result.unwrap();
    assert!(result.cleanup.child_reaped);
    assert!(matches!(
        result.cleanup.group,
        failforge_runner::GroupState::NoLiveMembers { .. }
    ));
    // A cooperative exit can race the forced signal. Preserve any recorded
    // signal error instead of inventing successful cleanup for this timeout.
    if !result.cleanup.errors.is_empty() {
        assert!(!result.cleanup.is_complete());
    }
    assert!(result.stdout.eof && result.stderr.eof);
    assert!(result.stdout.sha256.is_some() && result.stderr.sha256.is_some());
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

async fn buffered_response_after_interruption(case: &str) -> failforge_dart::ClientClose {
    let runner = Arc::new(Runner::new());
    let mut control = Control::new();
    let release = control.path.with_extension("release");
    let mut specification = spec("buffered-valid-response");
    specification.env.set.insert(
        "FAILFORGE_ADAPTER_CONTROL".into(),
        control.path.clone().into_os_string(),
    );
    specification.env.set.insert(
        "FAILFORGE_ADAPTER_RESPONSE_RELEASE".into(),
        release.clone().into_os_string(),
    );
    specification
        .env
        .set
        .insert("FAILFORGE_ADAPTER_RESPONSE_CASE".into(), case.into());
    let mut client = AdapterClient::start(runner.clone(), specification, options()).unwrap();
    let mut pending = Box::pin(client.handshake());
    tokio::select! {
        result = &mut pending => panic!("blocked fixture unexpectedly replied: {result:?}"),
        _ = control.request_observed() => {}
    }
    // Reproduce a normal response racing the owner's cancellation: the request
    // future has stopped consuming stdout, but the adapter has read that request.
    drop(pending);
    std::fs::write(&release, b"release").unwrap();
    control.response_observed().await;
    std::fs::remove_file(&release).unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(5), client.close_with_result())
        .await
        .unwrap();
    shutdown(&runner).await;
    let result = closed
        .result
        .as_ref()
        .expect("native cleanup receipt retained");
    assert!(result.cleanup.is_complete(), "{result:?}");
    assert!(result.stdout.eof && result.stderr.eof);
    closed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_pending_response_buffered_after_request_interruption_is_not_unsolicited() {
    let closed = buffered_response_after_interruption("valid").await;
    assert!(
        closed.outcome.is_ok(),
        "valid reply to the interrupted request was rejected: {:?}",
        closed.outcome
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_response_is_still_strictly_validated_after_native_cleanup() {
    for case in [
        "wrong-id",
        "wrong-version",
        "wrong-kind",
        "remote",
        "malformed",
        "duplicate",
        "oversize",
        "partial",
        "second-response",
        "extra",
    ] {
        let error = buffered_response_after_interruption(case)
            .await
            .outcome
            .unwrap_err();
        assert!(
            match case {
                "wrong-id" => matches!(
                    error,
                    ClientError::Protocol(ProtocolError::RequestIdMismatch)
                ),
                "wrong-version" =>
                    matches!(error, ClientError::Protocol(ProtocolError::VersionMismatch)),
                "wrong-kind" => matches!(
                    error,
                    ClientError::Protocol(ProtocolError::ResponseKindMismatch)
                ),
                "remote" => matches!(error, ClientError::Remote(_)),
                "malformed" => matches!(error, ClientError::Json(_)),
                "duplicate" => matches!(error, ClientError::InvalidMetadata),
                "oversize" => matches!(error, ClientError::LineTooLarge),
                "partial" => matches!(error, ClientError::UnexpectedEof),
                "second-response" | "extra" => matches!(error, ClientError::UnsolicitedResponse),
                _ => unreachable!(),
            },
            "{case}: {error}"
        );
    }
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
