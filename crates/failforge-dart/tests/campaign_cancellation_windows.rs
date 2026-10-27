#![cfg(windows)]
//! Installed helper + native Runner/Job + real private named-pipe controller.
use failforge_runner::{CommandSpec, CompletionCause, Runner};
use failforge_workspace::windows_fs::{PrivateSecurity, random_token};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ServerOptions;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-campaign-windows-{}", random_token().unwrap()));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(
            path.join("payload.json"),
            json!({"workspaceRoot":path,"packageRoot":".","limits":{"maxMutants":0}}).to_string(),
        )
        .unwrap();
        Self(path)
    }
    fn state(&self) -> Option<Value> {
        std::fs::read(self.0.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn cancellation_case(request: Option<u8>, buffered: bool) {
    let fixture = Fixture::new();
    let name = format!(
        r"\\.\pipe\failforge-campaign-test-{}{}",
        random_token().unwrap(),
        random_token().unwrap()
    );
    let mut security = PrivateSecurity::new().unwrap();
    // Current-account-only DACL installed at creation, one local connection.
    let mut pipe = unsafe {
        ServerOptions::new()
            .access_inbound(false)
            .access_outbound(true)
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create_with_security_attributes_raw(&name, security.as_ptr().cast())
            .unwrap()
    };
    let mut spec = CommandSpec {
        program: env!("CARGO_BIN_EXE_failforge-adapter-campaign").into(),
        args: vec![
            env!("CARGO_BIN_EXE_failforge-adapter-fixture").into(),
            fixture.0.clone().into_os_string(),
            fixture.0.join("payload.json").into_os_string(),
        ],
        cwd: fixture.0.clone(),
        env: Default::default(),
        timeout_policy: Default::default(),
        capture_policy: Default::default(),
    };
    spec.timeout_policy.timeout = Some(Duration::from_secs(15));
    spec.env
        .set
        .insert("FAILFORGE_ADAPTER_CANCEL_PIPE".into(), name.into());
    spec.env.set.insert(
        "FAILFORGE_CANCEL_MARKER".into(),
        fixture.0.join("state.json").into_os_string(),
    );
    spec.env
        .set
        .insert("FAILFORGE_CANCEL_MODE".into(), "discover".into());
    let runner = Runner::new();
    let mut handle = runner.start(spec).unwrap();
    tokio::time::timeout(Duration::from_secs(5), pipe.connect())
        .await
        .unwrap()
        .unwrap();
    if !buffered {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(state) = fixture.state().filter(|state| state["stage"] == "discover") {
                    assert_eq!(state["privateEnvironmentPresent"], false);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("adapter never reached quiet discovery");
    }
    if let Some(byte) = request {
        pipe.write_all(&[byte]).await.unwrap();
    }
    drop(pipe);
    let result = tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .unwrap()
        .unwrap();
    runner.shutdown().await.unwrap();
    assert_eq!(
        result.cause,
        CompletionCause::ProcessCompleted,
        "{result:?}"
    );
    assert!(!result.status.unwrap().success());
    assert!(result.cleanup.is_complete(), "{result:?}");
    let events: Vec<Value> = result
        .stdout
        .tail
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let cleanup = &events
        .iter()
        .find(|event| event["step"] == "cleanup")
        .expect("helper cleanup receipt")["payload"];
    assert_eq!(cleanup["complete"], true, "{cleanup}");
    assert_eq!(cleanup["adapterCloseSucceeded"], true, "{cleanup}");
    assert_eq!(cleanup["runnerShutdown"], true);
    assert_eq!(cleanup["activeRuns"], 0);
    assert_eq!(cleanup["cancelled"], true);
    assert_eq!(cleanup["campaignSucceeded"], false);
    assert_eq!(cleanup["writesPerformed"], false);
    assert_eq!(
        cleanup["cancellationReason"],
        match request {
            Some(b'I') => "SIGINT",
            Some(b'T') => "SIGTERM",
            _ => "controller_eof",
        }
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_request_is_not_lost_and_helper_drains_its_native_job() {
    cancellation_case(Some(b'I'), true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quiet_native_discovery_cancels_on_interrupt_terminate_and_controller_eof() {
    for request in [Some(b'I'), Some(b'T'), None] {
        cancellation_case(request, false).await;
    }
}
