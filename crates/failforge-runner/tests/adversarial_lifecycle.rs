//! Adversarial boundaries use a separate fixture control channel and a fresh
//! observer runtime; neither captured output nor arbitrary sleeps imply READY.

use failforge_runner::{CommandSpec, CompletionCause, EnvSpec, Runner, RunnerError, TimeoutPolicy};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct ControlHarness {
    directory: PathBuf,
    listener: UnixListener,
    control: Option<UnixStream>,
}

impl ControlHarness {
    fn new() -> Self {
        // Darwin sockaddr_un limits paths to 104 bytes. Use the existing short
        // /tmp surface rather than the macOS user's long temporary directory.
        let directory = PathBuf::from("/tmp").join(format!(
            "ff-adversarial-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&directory).unwrap();
        let listener = UnixListener::bind(directory.join("control.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            directory,
            listener,
            control: None,
        }
    }

    fn spec(&self) -> CommandSpec {
        let mut env = EnvSpec::default();
        env.set.insert(
            "FAILFORGE_CONTROL".into(),
            self.directory.join("control.sock").into(),
        );
        CommandSpec {
            program: env!("CARGO_BIN_EXE_failforge-process-fixture").into(),
            args: vec!["ignore".into()],
            cwd: self.directory.clone(),
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

    fn ready(&mut self) -> libc::pid_t {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stream = loop {
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture READY accept deadline");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("fixture READY accept failed: {error}"),
            }
        };
        stream.set_nonblocking(true).unwrap();
        // Register the exact connection before any assertion can unwind. Its
        // Drop closes the fixture's independent EOF stop channel.
        self.control = Some(stream.try_clone().unwrap());
        let mut line = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "fixture READY read deadline");
            let mut byte = [0];
            match stream.read(&mut byte) {
                Ok(1) => {}
                Ok(_) => panic!("fixture ended before READY"),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture READY read deadline");
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("fixture READY read failed: {error}"),
            }
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
            assert!(line.len() < 128, "fixture READY record too large");
            assert!(Instant::now() < deadline, "fixture READY read deadline");
        }
        let text = std::str::from_utf8(&line).unwrap();
        let fields: Vec<_> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0], "READY");
        let pid = fields[1].parse::<libc::pid_t>().unwrap();
        let group = fields[2].parse::<libc::pid_t>().unwrap();
        assert!(pid > 0);
        assert_eq!(pid, group);
        pid
    }

    fn assert_closed(&mut self) {
        let stream = self.control.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                Instant::now() < deadline,
                "fixture closure observation deadline"
            );
            let mut byte = [0];
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => panic!("unexpected fixture control data after READY"),
                Err(error) if error.kind() == io::ErrorKind::ConnectionReset => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture survived owner runtime destruction"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("fixture closure observation failed: {error}"),
            }
        }
    }
}

impl Drop for ControlHarness {
    fn drop(&mut self) {
        if let Some(mut stream) = self.control.take() {
            let _ = stream.write_all(b"exit 0\n");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn assert_eventually_reaped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // SAFETY: this non-consuming query addresses the direct fixture whose
        // status belongs to the independent fallback reaper. It cannot consume
        // that status and never sends a numeric PID or PGID signal.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: info is initialized native storage and WNOWAIT is read-only
        // with respect to status ownership. Stop permanently on ECHILD.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                return;
            }
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        }
        assert!(
            Instant::now() < deadline,
            "fallback reaper did not consume fixture status"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn destroying_the_actual_owner_runtime_publishes_partial_and_does_not_hang_wait() {
    for drive_owner in [false, true] {
        let mut harness = ControlHarness::new();
        let owner = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let runner = Runner::new();
        let mut handle = {
            let _entered = owner.enter();
            runner.start(harness.spec()).unwrap()
        };
        let pid = harness.ready();
        if drive_owner {
            owner.block_on(async {
                // Drive a real pending wait/timer before destruction as well
                // as covering destruction before the first block_on. READY
                // already proved the child is alive; this timeout drives the
                // runtime rather than serving as fixture readiness evidence.
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), handle.wait())
                        .await
                        .is_err()
                );
            });
        }
        // This deliberately crosses the documented runtime-abort boundary.
        // It verifies fallback/reporting behavior, not a normal cleanup promise.
        drop(owner);
        let observer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        observer.block_on(async {
            let error = tokio::time::timeout(Duration::from_secs(3), handle.wait())
                .await
                .expect("aborted owner left handle.wait pending")
                .unwrap_err();
            let partial = error
                .partial_result()
                .expect("runtime abort must publish a partial result");
            assert_eq!(partial.cause, CompletionCause::RunnerFailure);
            assert!(partial.status.is_none());
            assert!(!partial.cleanup.child_reaped);
            assert!(!partial.cleanup.is_complete());
            assert!(
                partial
                    .cleanup
                    .errors
                    .iter()
                    .any(|message| message.contains("supervisor stopped"))
            );
            assert_eq!(runner.active_runs(), 0);
            assert!(matches!(
                runner.shutdown().await,
                Err(RunnerError::ShutdownFailed { runs: 1 })
            ));
        });
        harness.assert_closed();
        assert_eventually_reaped(pid);
        // No numeric PID operation follows ECHILD; the control Drop is the
        // independent cleanup path if any preceding assertion fails.
    }
}
