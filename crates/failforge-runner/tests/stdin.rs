#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use failforge_runner::{CommandSpec, CompletionCause, Runner, TimeoutPolicy};

fn spec() -> CommandSpec {
    CommandSpec {
        program: "/bin/cat".into(),
        args: vec![],
        cwd: std::env::current_dir().unwrap(),
        env: Default::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        },
        capture_policy: Default::default(),
    }
}

#[tokio::test]
async fn transferred_input_preserves_native_bytes_and_eof() {
    let runner = Runner::new();
    let (mut parent, child) = UnixStream::pair().unwrap();
    let mut handle = runner
        .start_with_stdin(spec(), File::from(OwnedFd::from(child)))
        .unwrap();
    let bytes = b"first\n\0\xffsecond\r\n";
    parent.write_all(bytes).unwrap();
    parent.shutdown(std::net::Shutdown::Write).unwrap();
    let result = handle.wait().await.unwrap();
    assert_eq!(result.stdout.tail, bytes);
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert!(result.status.unwrap().success());
    assert!(result.cleanup.is_complete());
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_spec_closes_transferred_input_without_starting_a_child() {
    let runner = Runner::new();
    let (mut parent, child) = UnixStream::pair().unwrap();
    parent
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut invalid = spec();
    invalid.program = "invalid\0".into();
    assert!(
        runner
            .start_with_stdin(invalid, File::from(OwnedFd::from(child)))
            .is_err()
    );
    assert_eq!(parent.read(&mut [0]).unwrap(), 0);
    assert_eq!(runner.active_runs(), 0);
    runner.shutdown().await.unwrap();
}
