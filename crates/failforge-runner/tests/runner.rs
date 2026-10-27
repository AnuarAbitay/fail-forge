#![cfg(unix)]

//! Linux/macOS process tests. READY sockets are independent of captured stdio.
//! Guards release the exact fixture connections on assertion failure; no stale
//! numeric PGID is used by the harness. Every wait has an external deadline.
use failforge_runner::*;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Harness {
    dir: PathBuf,
    listener: UnixListener,
    controls: Vec<(u32, u32, UnixStream)>,
    escaped: Option<u32>,
}
impl Harness {
    fn new() -> Self {
        let dir = PathBuf::from("/tmp").join(format!(
            "ff-runner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            dir,
            listener,
            controls: Vec::new(),
            escaped: None,
        }
    }
    fn spec(&self, mode: &str, args: &[OsString]) -> CommandSpec {
        let mut env = EnvSpec::default();
        env.set.insert(
            "FAILFORGE_CONTROL".into(),
            self.dir.join("control.sock").into_os_string(),
        );
        CommandSpec {
            program: env!("CARGO_BIN_EXE_failforge-process-fixture").into(),
            args: std::iter::once(mode.into())
                .chain(args.iter().cloned())
                .collect(),
            cwd: self.dir.clone(),
            env,
            timeout_policy: TimeoutPolicy {
                grace_period: Duration::from_millis(30),
                forced_wait: Duration::from_secs(1),
                output_drain: Duration::from_millis(300),
                ..Default::default()
            },
            capture_policy: Default::default(),
        }
    }
    async fn ready(&mut self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.controls.len() < count {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(true).unwrap();
                    let mut line = Vec::new();
                    loop {
                        let mut byte = [0];
                        match stream.read(&mut byte) {
                            Ok(1) => {
                                line.push(byte[0]);
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Ok(_) => panic!("control closed before READY"),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(error) => panic!("control read: {error}"),
                        }
                        assert!(Instant::now() < deadline, "READY deadline");
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    let line = String::from_utf8(line).unwrap();
                    let parts: Vec<_> = line.split_whitespace().collect();
                    assert_eq!(parts[0], "READY");
                    let pid = parts[1].parse().unwrap();
                    if parts.get(3) == Some(&"ESCAPED") {
                        self.escaped = Some(pid);
                    }
                    self.controls.push((pid, parts[2].parse().unwrap(), stream));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "accept READY deadline");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(error) => panic!("control accept: {error}"),
            }
        }
    }
    fn release_leader(&mut self) {
        let leader = self
            .controls
            .iter()
            .find(|(pid, pgid, _)| pid == pgid)
            .unwrap()
            .0;
        self.controls
            .iter_mut()
            .find(|(pid, _, _)| *pid == leader)
            .unwrap()
            .2
            .write_all(b"leave\n")
            .unwrap();
    }
    async fn assert_controls_closed(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        for (_, _, stream) in &mut self.controls {
            loop {
                let mut byte = [0];
                match stream.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                    Err(error) => panic!("control close: {error}"),
                }
                assert!(Instant::now() < deadline, "fixture is still live");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        for (_, _, stream) in &mut self.controls {
            let _ = stream.write_all(b"exit 0\n");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
async fn wait(handle: &mut RunHandle) -> RunOutcome {
    tokio::time::timeout(Duration::from_secs(5), handle.wait())
        .await
        .expect("external run deadline")
}
fn expected_bytes(count: usize) -> Vec<u8> {
    let pattern = [0, 255, b'a', 254, b'\n'];
    (0..count).map(|index| pattern[index % 5]).collect()
}
fn strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

#[test]
fn adversarial_cancelled_shutdown_keeps_both_runtime_owners_and_original_reason() {
    let mut first_fixture = Harness::new();
    let mut second_fixture = Harness::new();
    let owner = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    };
    let first_runtime = owner();
    let second_runtime = owner();
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runner = Runner::new();
    let mut first_spec = first_fixture.spec("ignore", &[]);
    first_spec.timeout_policy.grace_period = Duration::from_secs(2);
    let mut second_spec = second_fixture.spec("ignore", &[]);
    second_spec.timeout_policy.grace_period = Duration::from_secs(2);
    let mut first = {
        let _entered = first_runtime.enter();
        runner.start(first_spec).unwrap()
    };
    let mut second = {
        let _entered = second_runtime.enter();
        runner.start(second_spec).unwrap()
    };
    caller.block_on(async {
        tokio::join!(first_fixture.ready(1), second_fixture.ready(1));
        assert_ne!(first_fixture.controls[0].0, second_fixture.controls[0].0);
        assert_eq!(runner.active_runs(), 2);
        // Poll shutdown, then cancel only its waiting future. Both supervisor
        // tasks must remain owned by their original, still-running runtimes.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), runner.shutdown())
                .await
                .is_err()
        );
        assert!(matches!(
            runner.start(first_fixture.spec("exit", &strings(&["0"]))),
            Err(RunnerError::ShuttingDown)
        ));
        assert!(!first.cancel(CancelReason::User));
        assert!(!second.cancel(CancelReason::CtrlC));
        assert!(!first.force_cancel(CancelReason::User));
        assert!(!second.force_cancel(CancelReason::CtrlC));
        let (shutdown, first_outcome, second_outcome) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), runner.shutdown()),
            wait(&mut first),
            wait(&mut second)
        );
        shutdown.unwrap().unwrap();
        let first_result = first_outcome.unwrap();
        let second_result = second_outcome.unwrap();
        for result in [&first_result, &second_result] {
            assert_eq!(
                result.cause,
                CompletionCause::Cancelled(CancelReason::Shutdown)
            );
            assert_eq!(result.status.unwrap().signal(), Some(libc::SIGKILL));
            assert!(result.cleanup.is_complete());
        }
        assert_eq!(runner.active_runs(), 0);
        tokio::join!(
            first_fixture.assert_controls_closed(),
            second_fixture.assert_controls_closed()
        );
        assert!(std::sync::Arc::ptr_eq(
            &first_result,
            &wait(&mut first).await.unwrap()
        ));
        assert!(std::sync::Arc::ptr_eq(
            &second_result,
            &wait(&mut second).await.unwrap()
        ));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adversarial_unobserved_real_quota_failure_survives_handle_drop() {
    let mut fixture = Harness::new();
    let runner = Runner::new();
    let infinite = usize::MAX.to_string();
    let mut spec = fixture.spec("bytes", &strings(&[&infinite, &infinite]));
    spec.capture_policy.capture_dir = Some(fixture.dir.join("capture"));
    spec.capture_policy.read_chunk_bytes = 4096;
    spec.capture_policy.file_quota_bytes_per_stream = 1024;
    spec.capture_policy.tail_bytes_per_stream = 17;
    let handle = runner.start(spec).unwrap();
    fixture.ready(1).await;
    // No handle.wait is polled: active=0 proves supervisor retirement without
    // acknowledging the retained failure through the public wait API.
    let deadline = Instant::now() + Duration::from_secs(5);
    while runner.active_runs() != 0 {
        assert!(
            Instant::now() < deadline,
            "quota cleanup retirement deadline"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    drop(handle);
    for _ in 0..2 {
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), runner.shutdown())
                .await
                .unwrap(),
            Err(RunnerError::ShutdownFailed { runs: 1 })
        ));
    }
    fixture.assert_controls_closed().await;
    let files = ["stdout.bin", "stderr.bin"].map(|name| {
        let bytes = std::fs::read(fixture.dir.join("capture").join(name)).unwrap();
        assert!(bytes.len() <= 1024);
        assert_eq!(bytes, expected_bytes(bytes.len()));
        bytes
    });
    assert!(files.iter().any(|bytes| bytes.len() == 1024));
    assert_eq!(runner.active_runs(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adversarial_real_reader_capacity_panic_returns_partial_and_reaps_child() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("exit", &strings(&["41"]));
    // This exceeds Vec's representable capacity. It panics before allocation,
    // so the probe never attempts an enormous physical-memory allocation.
    spec.capture_policy.read_chunk_bytes = usize::MAX;
    let mut handle = runner.start(spec).unwrap();
    let error = wait(&mut handle).await.unwrap_err();
    let result = error
        .partial_result()
        .expect("reader panic must be partial");
    assert!(result.status.is_some(), "native status must be retained");
    assert!(result.cleanup.child_reaped);
    assert!(matches!(
        result.cleanup.group,
        GroupState::NoLiveMembers { .. }
    ));
    assert!(!result.cleanup.output_finished);
    for summary in [&result.stdout, &result.stderr] {
        assert!(matches!(summary.issue, Some(CaptureIssue::ReadError(_))));
        assert_eq!(summary.bytes_read, 0);
        assert!(!summary.eof);
        assert!(summary.writer_finished);
    }
    let repeated = wait(&mut handle).await.unwrap_err();
    assert!(std::sync::Arc::ptr_eq(&error, &repeated));
    // Missing EOF is explicit capture failure; all process/writer resources
    // are nevertheless released, so observing the failure permits shutdown.
    tokio::time::timeout(Duration::from_secs(3), runner.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_reader_panic_with_file_capture_releases_writers_and_allows_shutdown() {
    let fixture = Harness::new();
    let runner = Runner::new();
    let capture = fixture.dir.join("capture");
    let mut spec = fixture.spec("exit", &strings(&["41"]));
    spec.capture_policy.capture_dir = Some(capture.clone());
    spec.capture_policy.read_chunk_bytes = usize::MAX;
    spec.timeout_policy.output_drain = Duration::from_secs(2);
    let mut handle = runner.start(spec).unwrap();
    let error = wait(&mut handle).await.unwrap_err();
    let result = error.partial_result().unwrap();
    assert!(result.status.is_some());
    assert!(result.cleanup.child_reaped);
    assert!(matches!(
        result.cleanup.group,
        GroupState::NoLiveMembers { .. }
    ));
    for summary in [&result.stdout, &result.stderr] {
        assert!(matches!(summary.issue, Some(CaptureIssue::ReadError(_))));
        assert!(
            summary.writer_finished,
            "idle writer must have received EOF"
        );
        assert!(!summary.eof, "a panicked reader cannot certify input EOF");
        assert_eq!(summary.file_bytes_written, 0);
    }
    assert!(
        std::fs::read(capture.join("stdout.bin"))
            .unwrap()
            .is_empty()
    );
    assert!(
        std::fs::read(capture.join("stderr.bin"))
            .unwrap()
            .is_empty()
    );
    tokio::time::timeout(Duration::from_secs(3), runner.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_status_and_reuse_after_spawn_errors() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut missing = harness.spec("exit", &strings(&["0"]));
    missing.program = "/failforge-missing-executable".into();
    assert!(matches!(
        runner.start(missing),
        Err(RunnerError::Preparation { .. })
    ));
    let mut badcwd = harness.spec("exit", &strings(&["0"]));
    badcwd.cwd = harness.dir.join("missing");
    assert!(runner.start(badcwd).is_err());
    for code in [0, 1, 2, 37, 124, 125, 130, 143, 255] {
        let mut handle = runner
            .start(harness.spec("exit", &strings(&[&code.to_string()])))
            .unwrap();
        let result = wait(&mut handle).await.unwrap();
        assert_eq!(result.cause, CompletionCause::ProcessCompleted);
        assert_eq!(result.status.unwrap().code(), Some(code));
        assert!(result.cleanup.is_complete());
        let repeated = wait(&mut handle).await.unwrap();
        assert!(std::sync::Arc::ptr_eq(&result, &repeated));
    }
    runner.shutdown().await.unwrap();
    assert_eq!(runner.active_runs(), 0);
    assert!(matches!(
        runner.start(harness.spec("exit", &strings(&["0"]))),
        Err(RunnerError::ShuttingDown)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arguments_environment_cwd_and_null_stdin_are_native() {
    use std::os::unix::ffi::OsStringExt;
    let harness = Harness::new();
    let runner = Runner::new();
    let args = vec![
        "a b".into(),
        "$HOME;echo *".into(),
        OsString::new(),
        OsString::from_vec(vec![255]),
    ];
    let mut handle = runner.start(harness.spec("argv", &args)).unwrap();
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(
        result.stdout.tail,
        b"612062\n24484f4d453b6563686f202a\n\nff\n"
    );
    let mut spec = harness.spec("env", &strings(&["MODE"]));
    spec.env
        .set
        .insert("MODE".into(), OsString::from_vec(vec![255, b'=']));
    let mut handle = runner.start(spec).unwrap();
    assert_eq!(wait(&mut handle).await.unwrap().stdout.tail, b"ff3d\n");
    let mut spec = harness.spec("env", &strings(&["PATH"]));
    spec.env.remove.insert("PATH".into());
    let mut handle = runner.start(spec).unwrap();
    assert_eq!(wait(&mut handle).await.unwrap().stdout.tail, b"<unset>\n");
    let mut spec = harness.spec("env", &strings(&["PATH"]));
    spec.env.clear = true;
    let mut handle = runner.start(spec).unwrap();
    assert_eq!(wait(&mut handle).await.unwrap().stdout.tail, b"<unset>\n");
    let before = std::env::current_dir().unwrap();
    let mut handle = runner.start(harness.spec("cwd", &[])).unwrap();
    let result = wait(&mut handle).await.unwrap();
    assert!(!result.stdout.tail.is_empty());
    assert_eq!(std::env::current_dir().unwrap(), before);
    let mut handle = runner.start(harness.spec("stdin-eof", &[])).unwrap();
    assert_eq!(
        wait(&mut handle).await.unwrap().status.unwrap().code(),
        Some(0)
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_binary_output_is_bounded_and_saved_exactly() {
    let harness = Harness::new();
    let runner = Runner::new();
    let count = 4 * 1024 * 1024 + 3;
    let mut spec = harness.spec("bytes", &strings(&[&count.to_string(), &count.to_string()]));
    spec.capture_policy.tail_bytes_per_stream = 17;
    spec.capture_policy.read_chunk_bytes = 4096;
    spec.capture_policy.capture_dir = Some(harness.dir.join("capture"));
    let mut handle = runner.start(spec).unwrap();
    let result = wait(&mut handle).await.unwrap();
    let expected = expected_bytes(count);
    for summary in [&result.stdout, &result.stderr] {
        assert_eq!(summary.bytes_read, count as u64);
        assert_eq!(summary.tail, expected[expected.len() - 17..]);
        assert!(summary.tail_truncated);
        assert_eq!(
            std::fs::read(summary.file_path.as_ref().unwrap()).unwrap(),
            expected
        );
        assert_eq!(summary.file_bytes_written, count as u64);
    }
    assert!(result.cleanup.is_complete());
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_output_preserves_all_256_byte_values_in_files_and_tails() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let stdout: Vec<_> = (u8::MIN..=u8::MAX).collect();
    let stderr: Vec<_> = stdout.iter().rev().copied().collect();
    let args: Vec<OsString> = [&stdout, &stderr]
        .into_iter()
        .map(|bytes| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                .into()
        })
        .collect();
    let mut spec = harness.spec("raw", &args);
    spec.capture_policy.read_chunk_bytes = 7;
    spec.capture_policy.tail_bytes_per_stream = 256;
    spec.capture_policy.capture_dir = Some(harness.dir.join("capture"));
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(0));
    for (summary, expected) in [(&result.stdout, stdout), (&result.stderr, stderr)] {
        assert_eq!(summary.bytes_read, 256);
        assert_eq!(summary.file_bytes_written, 256);
        assert_eq!(summary.tail, expected);
        assert!(!summary.tail_truncated);
        assert!(summary.eof && summary.writer_finished);
        assert!(summary.issue.is_none());
        assert_eq!(summary.stream_dropped_bytes, 0);
        assert_eq!(summary.stream_unconfirmed_bytes, 0);
        assert_eq!(
            std::fs::read(summary.file_path.as_ref().unwrap()).unwrap(),
            expected
        );
    }
    assert!(result.cleanup.is_complete());
    harness.assert_controls_closed().await;
    tokio::time::timeout(Duration::from_secs(5), runner.shutdown())
        .await
        .expect("external shutdown deadline")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn newline_free_raw_streams_finish_and_truncate_tails_by_bytes() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let stdout = b"\0\xffab\r\x80\xfe\x7f\0cd".to_vec();
    let stderr = b"\xfe\xff\0xyz\x81\r\0".to_vec();
    assert!(!stdout.contains(&b'\n') && !stderr.contains(&b'\n'));
    let mut spec = harness.spec(
        "raw",
        &strings(&["00ff61620d80fe7f006364", "feff0078797a810d00"]),
    );
    spec.capture_policy.read_chunk_bytes = 2;
    spec.capture_policy.tail_bytes_per_stream = 4;
    spec.capture_policy.capture_dir = Some(harness.dir.join("capture"));
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(0));
    for (summary, expected) in [(&result.stdout, stdout), (&result.stderr, stderr)] {
        assert_eq!(summary.bytes_read, expected.len() as u64);
        assert_eq!(summary.file_bytes_written, expected.len() as u64);
        assert_eq!(summary.tail, expected[expected.len() - 4..]);
        assert!(summary.tail_truncated);
        assert!(summary.eof && summary.writer_finished);
        assert!(summary.issue.is_none());
        assert_eq!(
            std::fs::read(summary.file_path.as_ref().unwrap()).unwrap(),
            expected
        );
    }
    assert!(result.cleanup.is_complete());
    harness.assert_controls_closed().await;
    tokio::time::timeout(Duration::from_secs(5), runner.shutdown())
        .await
        .expect("external shutdown deadline")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_raw_streams_have_empty_files_tails_and_zero_counters() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("raw", &strings(&["", ""]));
    spec.capture_policy.capture_dir = Some(harness.dir.join("capture"));
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(0));
    for summary in [&result.stdout, &result.stderr] {
        assert_eq!(summary.bytes_read, 0);
        assert_eq!(summary.file_bytes_written, 0);
        assert!(summary.tail.is_empty());
        assert!(!summary.tail_truncated);
        assert!(summary.eof && summary.writer_finished);
        assert!(summary.issue.is_none());
        assert!(
            std::fs::read(summary.file_path.as_ref().unwrap())
                .unwrap()
                .is_empty()
        );
    }
    assert!(result.cleanup.is_complete());
    harness.assert_controls_closed().await;
    tokio::time::timeout(Duration::from_secs(5), runner.shutdown())
        .await
        .expect("external shutdown deadline")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_forces_ignored_signal_and_preserves_real_status() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("ignore", &[]);
    spec.timeout_policy.timeout = Some(Duration::from_millis(200));
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::TimedOut);
    assert_eq!(result.status.unwrap().signal(), Some(libc::SIGKILL));
    assert!(
        result
            .cleanup
            .signals
            .iter()
            .any(|item| item.signal == libc::SIGTERM)
    );
    assert!(
        result
            .cleanup
            .signals
            .iter()
            .any(|item| item.signal == libc::SIGKILL)
    );
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_can_coexist_with_zero_exit_status() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("graceful", &[]);
    spec.timeout_policy.timeout = Some(Duration::from_millis(200));
    spec.timeout_policy.grace_period = Duration::from_millis(200);
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::TimedOut);
    assert_eq!(result.status.unwrap().code(), Some(0));
    assert_eq!(result.cleanup.signals.len(), 1);
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_wait_does_not_cancel_supervisor_and_can_be_repeated() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut handle = runner.start(harness.spec("wait", &[])).unwrap();
    harness.ready(1).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(10), handle.wait())
            .await
            .is_err()
    );
    assert!(handle.cancel(CancelReason::User));
    assert!(!handle.cancel(CancelReason::CtrlC));
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert!(result.cleanup.is_complete());
    let repeated = wait(&mut handle).await.unwrap();
    assert!(std::sync::Arc::ptr_eq(&result, &repeated));
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_request_shortens_grace_without_overwriting_cause() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("ignore", &[]);
    spec.timeout_policy.grace_period = Duration::from_secs(3);
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    assert!(handle.cancel(CancelReason::User));
    handle.force_cancel(CancelReason::CtrlC);
    let start = Instant::now();
    let result = wait(&mut handle).await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert_eq!(result.status.unwrap().signal(), Some(libc::SIGKILL));
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_handle_and_runner_shutdown_reap_children() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let handle = runner.start(harness.spec("ignore", &[])).unwrap();
    harness.ready(1).await;
    drop(handle);
    tokio::time::timeout(Duration::from_secs(5), runner.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runner.active_runs(), 0);
    harness.assert_controls_closed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_exit_does_not_hide_descendants_with_or_without_pipes() {
    for mode in ["tree-pipe", "tree-closed"] {
        let mut harness = Harness::new();
        let runner = Runner::new();
        let mut handle = runner.start(harness.spec(mode, &[])).unwrap();
        harness.ready(2).await;
        harness.release_leader();
        let result = wait(&mut handle).await.unwrap();
        assert_eq!(result.cause, CompletionCause::ProcessCompleted);
        assert_eq!(result.status.unwrap().code(), Some(0));
        assert!(result.cleanup.is_complete());
        assert!(
            result
                .cleanup
                .signals
                .iter()
                .any(|item| item.signal == libc::SIGKILL)
        );
        harness.assert_controls_closed().await;
        runner.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn descendant_created_by_running_parent_is_cleaned_up_on_cancel() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut handle = runner.start(harness.spec("tree-wait", &[])).unwrap();
    harness.ready(2).await;
    handle.cancel(CancelReason::User);
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert!(result.cleanup.is_complete());
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_failure_returns_partial_output_and_allows_a_later_run() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("bytes", &strings(&["1048576", "1048576"]));
    spec.capture_policy.capture_dir = Some(harness.dir.join("capture"));
    spec.capture_policy.file_quota_bytes_per_stream = 1024;
    let mut handle = runner.start(spec).unwrap();
    let error = wait(&mut handle).await.unwrap_err();
    let result = error.partial_result().unwrap();
    assert!(result.stdout.issue.is_some() || result.stderr.issue.is_some());
    assert!(result.cleanup.child_reaped);
    assert!(result.stdout.file_bytes_written <= 1024 && result.stderr.file_bytes_written <= 1024);
    let mut handle = runner
        .start(harness.spec("exit", &strings(&["0"])))
        .unwrap();
    assert_eq!(
        wait(&mut handle).await.unwrap().status.unwrap().code(),
        Some(0)
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_runs_keep_outcomes_and_capture_independent_through_cancel_and_shutdown() {
    let mut cancelled_process = Harness::new();
    let mut shutdown_process = Harness::new();
    let mut successful_process = Harness::new();
    let mut quota_process = Harness::new();
    let runner = Runner::new();

    // READY confirms that both signal-ignoring children are actually running.
    // Each owns a separate control connection for cleanup even on assertion failure.
    let mut cancelled = runner.start(cancelled_process.spec("ignore", &[])).unwrap();
    let mut shutdown = runner.start(shutdown_process.spec("ignore", &[])).unwrap();
    tokio::join!(cancelled_process.ready(1), shutdown_process.ready(1));
    assert_ne!(
        cancelled_process.controls[0].0,
        shutdown_process.controls[0].0
    );
    assert_ne!(
        cancelled_process.controls[0].1,
        shutdown_process.controls[0].1
    );
    assert_eq!(runner.active_runs(), 2);

    let mut success_spec =
        successful_process.spec("raw", &strings(&["5355434345535300ff", "4f574e2d455252fe"]));
    success_spec.capture_policy.capture_dir = Some(successful_process.dir.join("capture"));
    success_spec.capture_policy.tail_bytes_per_stream = 64;
    let mut successful = runner.start(success_spec).unwrap();

    // Infinite output makes quota exhaustion stop a still-running command.
    // The watchdog socket independently stops it if the backend/test fails.
    let amount = usize::MAX.to_string();
    let mut quota_spec = quota_process.spec("bytes", &strings(&[&amount, &amount]));
    quota_spec.capture_policy.capture_dir = Some(quota_process.dir.join("capture"));
    quota_spec.capture_policy.file_quota_bytes_per_stream = 1024;
    quota_spec.capture_policy.read_chunk_bytes = 4096;
    quota_spec.capture_policy.tail_bytes_per_stream = 17;
    let mut quota = runner.start(quota_spec).unwrap();
    tokio::join!(successful_process.ready(1), quota_process.ready(1));

    let (success_outcome, quota_outcome) = tokio::join!(wait(&mut successful), wait(&mut quota));
    let success_result = success_outcome.unwrap();
    let quota_error = quota_outcome.unwrap_err();
    let quota_result = quota_error.partial_result().unwrap();

    assert_eq!(success_result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(success_result.status.unwrap().code(), Some(0));
    assert!(success_result.cleanup.is_complete());
    assert!(success_result.cleanup.signals.is_empty());
    for (summary, expected) in [
        (&success_result.stdout, b"SUCCESS\0\xff".as_slice()),
        (&success_result.stderr, b"OWN-ERR\xfe".as_slice()),
    ] {
        assert_eq!(summary.bytes_read, expected.len() as u64);
        assert_eq!(summary.tail, expected);
        assert!(!summary.tail_truncated);
        assert_eq!(summary.file_bytes_written, expected.len() as u64);
        assert!(summary.issue.is_none());
        let path = summary.file_path.as_ref().unwrap();
        assert!(path.starts_with(successful_process.dir.join("capture")));
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }

    // wait acknowledges the partial error before shutdown checks failure history.
    assert_eq!(quota_result.cause, CompletionCause::RunnerFailure);
    assert!(quota_result.status.is_some());
    assert!(quota_result.cleanup.child_reaped);
    assert!(matches!(
        quota_result.cleanup.group,
        GroupState::NoLiveMembers { .. }
    ));
    assert!(
        matches!(
            quota_result.stdout.issue,
            Some(CaptureIssue::FileQuotaExceeded)
        ) || matches!(
            quota_result.stderr.issue,
            Some(CaptureIssue::FileQuotaExceeded)
        )
    );
    for summary in [&quota_result.stdout, &quota_result.stderr] {
        assert!(summary.eof && summary.writer_finished);
        assert!(summary.file_bytes_written <= 1024);
        assert!(summary.tail.len() <= 17);
        let offset = summary.bytes_read - summary.tail.len() as u64;
        let tail: Vec<_> = (0..summary.tail.len())
            .map(|index| [0, 255, b'a', 254, b'\n'][((offset + index as u64) % 5) as usize])
            .collect();
        assert_eq!(summary.tail, tail);
        let path = summary.file_path.as_ref().unwrap();
        assert!(path.starts_with(quota_process.dir.join("capture")));
        let captured = std::fs::read(path).unwrap();
        assert_eq!(captured.len() as u64, summary.file_bytes_written);
        assert_eq!(captured, expected_bytes(captured.len()));
    }

    // These are negative assertions on retained wait futures, not sleep-based
    // readiness. A success/quota failure must not finish either held child.
    let (cancelled_pending, shutdown_pending) = tokio::join!(
        tokio::time::timeout(Duration::from_millis(20), cancelled.wait()),
        tokio::time::timeout(Duration::from_millis(20), shutdown.wait()),
    );
    assert!(cancelled_pending.is_err() && shutdown_pending.is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        while runner.active_runs() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed runs must leave the registry");

    assert!(cancelled.cancel(CancelReason::User));
    let cancelled_result = wait(&mut cancelled).await.unwrap();
    assert_eq!(
        cancelled_result.cause,
        CompletionCause::Cancelled(CancelReason::User)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), shutdown.wait())
            .await
            .is_err()
    );
    cancelled_process.assert_controls_closed().await;

    // One run ends through explicit cancellation; shutdown owns the other.
    let (shutdown_outcome, last_outcome) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), runner.shutdown()),
        wait(&mut shutdown),
    );
    shutdown_outcome
        .expect("external shutdown deadline")
        .unwrap();
    let shutdown_result = last_outcome.unwrap();
    assert_eq!(
        shutdown_result.cause,
        CompletionCause::Cancelled(CancelReason::Shutdown)
    );
    for result in [cancelled_result.as_ref(), shutdown_result.as_ref()] {
        assert_eq!(result.status.unwrap().signal(), Some(libc::SIGKILL));
        assert!(result.cleanup.is_complete());
        for summary in [&result.stdout, &result.stderr] {
            assert_eq!(summary.bytes_read, 0);
            assert!(summary.tail.is_empty());
            assert!(summary.file_path.is_none());
            assert!(summary.issue.is_none());
        }
    }
    assert_eq!(runner.active_runs(), 0);
    tokio::join!(
        shutdown_process.assert_controls_closed(),
        successful_process.assert_controls_closed(),
        quota_process.assert_controls_closed(),
    );

    assert!(std::sync::Arc::ptr_eq(
        &success_result,
        &wait(&mut successful).await.unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &quota_error,
        &wait(&mut quota).await.unwrap_err()
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_stream_consumer_drops_display_without_stalling_capture() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("bytes", &strings(&["1048576", "1048576"]));
    spec.capture_policy.mode = OutputMode::StreamAndCapture;
    spec.capture_policy.stream_queue_bytes = 16;
    spec.capture_policy.read_chunk_bytes = 4096;
    let mut handle = runner.start(spec).unwrap();
    let _receiver = handle.take_stream().unwrap();
    assert!(handle.take_stream().is_none());
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.stdout.bytes_read, 1048576);
    assert_eq!(result.stderr.bytes_read, 1048576);
    assert!(result.stdout.stream_dropped_bytes + result.stderr.stream_dropped_bytes > 0);
    assert!(result.cleanup.is_complete());
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_search_uses_effective_environment_and_cwd_without_shell_fallback() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let harness = Harness::new();
    let runner = Runner::new();
    symlink(
        env!("CARGO_BIN_EXE_failforge-process-fixture"),
        harness.dir.join("tool"),
    )
    .unwrap();
    for path in ["", "."] {
        let mut spec = harness.spec("exit", &strings(&["37"]));
        spec.program = "tool".into();
        spec.env.clear = true;
        spec.env.set.insert("PATH".into(), path.into());
        let mut handle = runner.start(spec).unwrap();
        assert_eq!(
            wait(&mut handle).await.unwrap().status.unwrap().code(),
            Some(37)
        );
    }
    let script = harness.dir.join("plain-script");
    std::fs::write(&script, b"echo IMPLICIT_SHELL_IS_FORBIDDEN\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = harness.spec("exit", &[]);
    spec.program = script.into_os_string();
    spec.args = vec![];
    assert!(matches!(
        runner.start(spec),
        Err(RunnerError::Preparation {
            os_error: Some(libc::ENOEXEC),
            ..
        })
    ));
    let script = harness.dir.join("launcher");
    std::fs::write(&script, b"#!/bin/sh\nexit 37\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = harness.spec("exit", &[]);
    spec.program = "./launcher".into();
    spec.args = vec![];
    let mut handle = runner.start(spec).unwrap();
    assert_eq!(
        wait(&mut handle).await.unwrap().status.unwrap().code(),
        Some(37)
    );
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_configuration_and_unrepresentable_budget_fail_before_spawn() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("exit", &strings(&["0"]));
    spec.program = OsString::new();
    assert!(matches!(
        runner.start(spec),
        Err(RunnerError::InvalidSpec(SpecError::EmptyProgram))
    ));
    let mut spec = harness.spec("exit", &strings(&["0"]));
    spec.timeout_policy.timeout = Some(Duration::MAX);
    assert!(matches!(
        runner.start(spec),
        Err(RunnerError::TimeBudgetOutOfRange)
    ));
    assert_eq!(runner.active_runs(), 0);
    runner.shutdown().await.unwrap();
    runner.shutdown().await.unwrap();
}

#[test]
fn start_requires_an_existing_runtime() {
    let harness = Harness::new();
    let runner = Runner::default();
    assert!(matches!(
        runner.start(harness.spec("exit", &strings(&["0"]))),
        Err(RunnerError::NoRuntime)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn path_search_retries_kernel_permission_errors_and_preserves_eacces() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let harness = Harness::new();
    let runner = Runner::new();
    let denied = harness.dir.join("denied");
    let allowed = harness.dir.join("allowed");
    std::fs::create_dir(&denied).unwrap();
    std::fs::create_dir(&allowed).unwrap();
    std::fs::write(denied.join("tool"), b"unexecutable").unwrap();
    std::fs::set_permissions(denied.join("tool"), std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(
        env!("CARGO_BIN_EXE_failforge-process-fixture"),
        allowed.join("tool"),
    )
    .unwrap();
    let mut spec = harness.spec("exit", &strings(&["37"]));
    spec.program = "tool".into();
    spec.env.clear = true;
    spec.env.set.insert(
        "PATH".into(),
        std::env::join_paths([&denied, &allowed]).unwrap(),
    );
    let mut handle = runner.start(spec).unwrap();
    assert_eq!(
        wait(&mut handle).await.unwrap().status.unwrap().code(),
        Some(37)
    );
    let mut spec = harness.spec("exit", &strings(&["0"]));
    spec.program = "tool".into();
    spec.env.clear = true;
    spec.env.set.insert(
        "PATH".into(),
        std::env::join_paths([denied, harness.dir.join("absent")]).unwrap(),
    );
    assert!(matches!(
        runner.start(spec),
        Err(RunnerError::Preparation {
            os_error: Some(libc::EACCES),
            ..
        })
    ));
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn executable_and_cwd_paths_preserve_non_utf8_bytes() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    let harness = Harness::new();
    let runner = Runner::new();
    let cwd = harness.dir.join(OsString::from_vec(vec![b'c', 255]));
    if let Err(error) = std::fs::create_dir(&cwd) {
        // APFS rejects ill-formed UTF-8 file names before Runner can execute.
        // That native rejection must still become a typed preparation error.
        assert_eq!(error.raw_os_error(), Some(libc::EILSEQ));
        let mut spec = harness.spec("cwd", &[]);
        spec.cwd = cwd;
        let native = std::fs::canonicalize(&spec.cwd).unwrap_err();
        match runner.start(spec) {
            Err(RunnerError::Preparation { kind, os_error, .. }) => {
                assert_eq!(kind, native.kind());
                assert_eq!(os_error, native.raw_os_error());
            }
            _ => panic!("native cwd rejection must remain a preparation failure"),
        }
        assert_eq!(runner.active_runs(), 0);
        runner.shutdown().await.unwrap();
        return;
    }
    let name = OsString::from_vec(vec![b't', 254]);
    symlink(
        env!("CARGO_BIN_EXE_failforge-process-fixture"),
        cwd.join(&name),
    )
    .unwrap();
    let mut spec = harness.spec("cwd", &[]);
    spec.cwd = cwd.clone();
    spec.program = PathBuf::from(".").join(name).into_os_string();
    let mut handle = runner.start(spec).unwrap();
    let result = wait(&mut handle).await.unwrap();
    let canonical = std::fs::canonicalize(cwd).unwrap();
    let mut expected = String::new();
    for byte in canonical.as_os_str().as_encoded_bytes() {
        use std::fmt::Write;
        write!(expected, "{byte:02x}").unwrap();
    }
    expected.push('\n');
    assert_eq!(result.stdout.tail, expected.as_bytes());
    assert!(result.cleanup.is_complete());
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuous_output_does_not_starve_timeout_or_grow_retained_tails() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let count = usize::MAX.to_string();
    let mut spec = harness.spec("bytes", &strings(&[&count, &count]));
    spec.timeout_policy.timeout = Some(Duration::from_millis(150));
    spec.capture_policy.tail_bytes_per_stream = 17;
    let mut handle = runner.start(spec).unwrap();
    harness.ready(1).await;
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::TimedOut);
    assert!(result.cleanup.is_complete());
    for summary in [&result.stdout, &result.stderr] {
        assert!(summary.bytes_read > 17);
        assert_eq!(summary.tail.len(), 17);
        assert!(summary.tail_truncated);
    }
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn byte_watchdog_stops_undrained_pipes_independently_and_allows_natural_exit() {
    use std::os::fd::AsRawFd;
    use std::process::{Child, Command, Stdio};

    // No Runner/ManagedChild is involved: the socket is a fixture-owned stop
    // mechanism. This guard signals only its still-owned, unreaped direct PID.
    struct NativeChild(Child);
    impl Drop for NativeChild {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let deadline = Instant::now() + Duration::from_secs(2);
                while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
    fn pending(fd: &impl AsRawFd) -> i32 {
        let mut bytes = 0;
        // SAFETY: the Child-owned pipe descriptor stays open, and bytes is
        // writable native int storage for this read-only FIONREAD query.
        assert_eq!(
            unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &mut bytes) },
            0
        );
        bytes
    }
    async fn finished(child: &mut NativeChild) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "native fixture stop deadline");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    for command_stop in [true, false] {
        let mut harness = Harness::new();
        let count = usize::MAX.to_string();
        let mut child = NativeChild(
            Command::new(env!("CARGO_BIN_EXE_failforge-process-fixture"))
                .args(["bytes", &count, &count])
                .env("FAILFORGE_CONTROL", harness.dir.join("control.sock"))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        harness.ready(1).await;
        assert_eq!(harness.controls[0].0, child.0.id());

        // Readiness comes from the separate socket. Neither pipe is drained;
        // wait for output in both and unchanged occupancy across polls, rather
        // than an arbitrary sleep before attempting independent cleanup.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut previous = None;
        loop {
            assert!(child.0.try_wait().unwrap().is_none());
            let occupancy = (
                pending(child.0.stdout.as_ref().unwrap()),
                pending(child.0.stderr.as_ref().unwrap()),
            );
            if occupancy.0 >= 4096 && occupancy.1 >= 4096 && previous == Some(occupancy) {
                break;
            }
            previous = Some(occupancy);
            assert!(
                Instant::now() < deadline,
                "undrained fixture pipes never filled"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if command_stop {
            harness.controls[0].2.write_all(b"exit 0\n").unwrap();
        } else {
            drop(harness.controls.remove(0));
        }
        assert_eq!(finished(&mut child).await.code(), Some(0));
        harness.assert_controls_closed().await;
    }

    // CLI benchmarks create a listener but do not accept bytes-mode READY.
    // The watcher must not be joined or hold up finite output's natural exit.
    let harness = Harness::new();
    let mut child = NativeChild(
        Command::new(env!("CARGO_BIN_EXE_failforge-process-fixture"))
            .args(["bytes", "37", "37"])
            .env("FAILFORGE_CONTROL", harness.dir.join("control.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert_eq!(finished(&mut child).await.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn display_failure_after_reap_preserves_status_and_never_signals_retired_group() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("bytes", &strings(&["5", "0"]));
    spec.capture_policy.mode = OutputMode::StreamAndCapture;
    spec.timeout_policy.output_drain = Duration::from_secs(2);
    let mut handle = runner.start(spec).unwrap();
    let mut receiver = handle.take_stream().unwrap();
    let chunk = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    // The queue closes from OutputSession::finish, which runs after leader reap.
    assert!(
        tokio::time::timeout(Duration::from_secs(3), receiver.recv())
            .await
            .unwrap()
            .is_none()
    );
    chunk.report_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
    drop(chunk);
    let error = wait(&mut handle).await.unwrap_err();
    let result = error.partial_result().unwrap();
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(0));
    assert!(result.cleanup.child_reaped);
    assert!(result.cleanup.signals.is_empty());
    assert!(matches!(
        result.stdout.issue,
        Some(CaptureIssue::StreamError(_))
    ));
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn batched_stream_failure_is_attributed_to_its_chunk_in_the_public_result() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("raw", &strings(&["6f7574", "657272"]));
    spec.capture_policy.mode = OutputMode::StreamAndCapture;
    spec.timeout_policy.output_drain = Duration::from_secs(2);
    let mut handle = runner.start(spec).unwrap();
    let mut receiver = handle.take_stream().unwrap();
    let mut chunks = tokio::time::timeout(Duration::from_secs(3), async {
        let mut chunks = Vec::new();
        while let Some(chunk) = receiver.recv().await {
            chunks.push(chunk);
            assert!(chunks.len() <= 2, "three-byte streams each fit one chunk");
        }
        chunks
    })
    .await
    .unwrap();
    assert_eq!(chunks.len(), 2);
    let stdout = chunks
        .iter()
        .find(|chunk| chunk.stream == OutputStream::Stdout)
        .unwrap();
    assert_eq!(stdout.bytes(), b"out");
    stdout.report_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
    let stderr = chunks
        .iter_mut()
        .find(|chunk| chunk.stream == OutputStream::Stderr)
        .unwrap();
    assert_eq!(stderr.bytes(), b"err");
    stderr.mark_displayed(stderr.bytes().len());
    drop(chunks);
    let error = wait(&mut handle).await.unwrap_err();
    let result = error.partial_result().unwrap();
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(0));
    assert!(result.cleanup.child_reaped);
    assert!(result.cleanup.signals.is_empty());
    assert!(matches!(
        result.stdout.issue,
        Some(CaptureIssue::StreamError(_))
    ));
    assert!(result.stderr.issue.is_none());
    assert_eq!(result.stdout.tail, b"out");
    assert_eq!(result.stderr.tail, b"err");
    assert_eq!(result.stdout.stream_dropped_bytes, 3);
    assert_eq!(result.stderr.stream_dropped_bytes, 0);
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unacknowledged_display_is_reported_without_changing_os_completion() {
    let harness = Harness::new();
    let runner = Runner::new();
    let mut spec = harness.spec("bytes", &strings(&["5", "0"]));
    spec.capture_policy.mode = OutputMode::StreamAndCapture;
    spec.timeout_policy.output_drain = Duration::from_millis(50);
    let mut handle = runner.start(spec).unwrap();
    let mut receiver = handle.take_stream().unwrap();
    let chunk = tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.stdout.stream_unconfirmed_bytes, 5);
    assert_eq!(result.status.unwrap().code(), Some(0));
    assert!(result.cleanup.signals.is_empty());
    assert!(result.cleanup.is_complete());
    drop(chunk);
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escaped_descendant_is_an_explicit_boundary_and_has_independent_cleanup() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut handle = runner.start(harness.spec("tree-escape", &[])).unwrap();
    harness.ready(2).await;
    // The separate control role identifies the escape without a numeric PID
    // signal or assuming READY arrival order.
    assert_eq!(
        harness
            .controls
            .iter()
            .map(|(_, pgid, _)| pgid)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
    let escaped = harness.escaped.unwrap();
    harness
        .controls
        .iter_mut()
        .find(|(pid, _, _)| *pid != escaped)
        .unwrap()
        .2
        .write_all(b"leave\n")
        .unwrap();
    let error = wait(&mut handle).await.unwrap_err();
    let result = error.partial_result().unwrap();
    assert!(result.cleanup.child_reaped);
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert!(matches!(
        result.stdout.issue,
        Some(CaptureIssue::DrainExpired)
    ));
    assert!(matches!(
        result.cleanup.group,
        GroupState::NoLiveMembers { .. }
    ));
    harness
        .controls
        .iter_mut()
        .find(|(pid, _, _)| *pid == escaped)
        .unwrap()
        .2
        .write_all(b"release\n")
        .unwrap();
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_runner_requests_shutdown_without_aborting_handle_wait() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut handle = runner.start(harness.spec("wait", &[])).unwrap();
    harness.ready(1).await;
    drop(runner);
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(
        result.cause,
        CompletionCause::Cancelled(CancelReason::Shutdown)
    );
    assert!(result.cleanup.is_complete());
    harness.assert_controls_closed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leader_leaving_its_original_group_is_forced_without_signalling_controller_group() {
    let mut harness = Harness::new();
    let runner = Runner::new();
    let mut handle = runner.start(harness.spec("move-group", &[])).unwrap();
    harness.ready(1).await;
    assert_ne!(harness.controls[0].0, harness.controls[0].1);
    handle.cancel(CancelReason::User);
    let result = wait(&mut handle).await.unwrap();
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert_eq!(result.status.unwrap().signal(), Some(libc::SIGKILL));
    assert!(result.cleanup.is_complete());
    harness.assert_controls_closed().await;
    runner.shutdown().await.unwrap();
}
