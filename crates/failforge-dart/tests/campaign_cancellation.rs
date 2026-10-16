//! The installed helper cooperatively drains an actual quiet protocol process.
//! The finite Python fixture needs neither Dart nor network access. Its natural
//! 1.5-second response makes an ignored cancellation fail without orphaning it.
use serde_json::{Value, json};
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const CANCEL_ENV: &str = "FAILFORGE_ADAPTER_CANCEL_FD";
static NEXT: AtomicU64 = AtomicU64::new(0);

const ADAPTER: &str = r##"#!/usr/bin/env python3
import json, os, sys, time
from pathlib import Path
marker = Path(os.environ['FAILFORGE_CANCEL_MARKER'])
probe = int(os.environ['FAILFORGE_CANCEL_PROBE_FD'])
try:
    info = os.fstat(probe)
    inherited = [info.st_dev, info.st_ino] == json.loads(os.environ['FAILFORGE_CANCEL_PROBE_INODE'])
except OSError:
    inherited = False
state = dict(pid=os.getpid(), inheritedControl=inherited,
             privateEnvironmentPresent='FAILFORGE_ADAPTER_CANCEL_FD' in os.environ)
def report(stage):
    marker.write_text(json.dumps(dict(state, stage=stage)))
report('started')
for line in sys.stdin:
    request = json.loads(line)
    command = request['command']
    if command == 'handshake':
        payload = dict(operators=[])
    else:
        report('discover')
        if os.environ.get('FAILFORGE_CANCEL_MODE') != 'close':
            time.sleep(1.5)
        payload = dict(mutants=[], truncated=False, totalCandidates=0, returnedCandidates=0)
    print(json.dumps(dict(protocolVersion=1, requestId=request['requestId'],
                         kind=command, payload=payload)), flush=True)
if os.environ.get('FAILFORGE_CANCEL_MODE') == 'close':
    report('close')
    time.sleep(1.5)
"##;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff helper cancellation {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        fs::create_dir(path.join("lib")).unwrap();
        fs::write(path.join("adapter"), ADAPTER).unwrap();
        fs::set_permissions(path.join("adapter"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            path.join("payload.json"),
            json!({
                "workspaceRoot":path, "packageRoot":".", "limits":{"maxMutants":0}
            })
            .to_string(),
        )
        .unwrap();
        Self(path)
    }

    fn marker(&self) -> Option<Value> {
        fs::read(self.0.join("state.json"))
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pipe() -> (OwnedFd, File) {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors is writable storage for the two pipe descriptors.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: pipe succeeded and both fresh descriptors are uniquely adopted.
    let read = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
    // Only the read capability is deliberately inherited. In particular, the
    // helper must not keep its own writer alive and defeat controller EOF.
    // SAFETY: write remains owned and F_SETFD takes descriptor flags only.
    assert_eq!(
        unsafe { libc::fcntl(write.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );
    (read, File::from(write))
}

struct Owner {
    child: Child,
    control: Option<File>,
    events: Receiver<Value>,
}
impl Owner {
    fn start(fixture: &Fixture, before_start: Option<u8>, close: bool) -> Self {
        let (read, mut write) = pipe();
        let info = File::from(read.try_clone().unwrap()).metadata().unwrap();
        if let Some(byte) = before_start {
            write.write_all(&[byte]).unwrap();
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_failforge-adapter-campaign"))
            .arg(fixture.0.join("adapter"))
            .arg(&fixture.0)
            .arg(fixture.0.join("payload.json"))
            .env(CANCEL_ENV, read.as_raw_fd().to_string())
            .env("FAILFORGE_CANCEL_PROBE_FD", read.as_raw_fd().to_string())
            .env(
                "FAILFORGE_CANCEL_PROBE_INODE",
                json!([info.dev(), info.ino()]).to_string(),
            )
            .env("FAILFORGE_CANCEL_MARKER", fixture.0.join("state.json"))
            .env(
                "FAILFORGE_CANCEL_MODE",
                if close { "close" } else { "discover" },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(File::create(fixture.0.join("owner.stderr")).unwrap())
            .spawn()
            .unwrap();
        drop(read);
        let stdout = child.stdout.take().unwrap();
        let (send, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value = serde_json::from_str(&line).expect("helper emitted invalid JSON");
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            control: Some(write),
            events,
        }
    }

    fn wait_quiet(&mut self, fixture: &Fixture, stage: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fixture
                .marker()
                .is_some_and(|state| state["stage"] == stage)
            {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "helper exited before quiet request"
            );
            assert!(
                Instant::now() < deadline,
                "adapter never entered quiet stage"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn cleanup(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let event = self
                .events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("helper did not publish cleanup");
            if event["step"] == "cleanup" {
                while self.child.try_wait().unwrap().is_none() {
                    assert!(
                        Instant::now() < deadline,
                        "helper did not exit after cleanup"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert!(!self.child.wait().unwrap().success());
                return event["payload"].clone();
            }
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if let Some(control) = &mut self.control {
            let _ = control.write_all(b"I");
        }
        self.control.take();
        // Never kill a cleanup owner. The finite adapter ensures this wait is
        // bounded even when the cancellation mechanism is broken in a RED run.
        let _ = self.child.wait();
    }
}

fn assert_cleanup(value: &Value, reason: &str) {
    assert_eq!(value["complete"], true);
    assert_eq!(value["adapterCloseSucceeded"], true, "{value}");
    assert_eq!(value["runnerShutdown"], true);
    assert_eq!(value["activeRuns"], 0);
    assert_eq!(value["campaignSucceeded"], false);
    assert_eq!(value["cancelled"], true);
    assert_eq!(value["cancellationReason"], reason);
    assert_eq!(value["writesPerformed"], false);
}

#[test]
fn buffered_cancellation_before_helper_start_is_not_lost() {
    let fixture = Fixture::new();
    let mut owner = Owner::start(&fixture, Some(b'I'), false);
    let started = Instant::now();
    assert_cleanup(&owner.cleanup(), "SIGINT");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn control_sigint_sigterm_and_eof_cancel_quiet_discovery_and_isolate_capability() {
    for request in [Some(b'I'), Some(b'T'), None] {
        let fixture = Fixture::new();
        let mut owner = Owner::start(&fixture, None, false);
        owner.wait_quiet(&fixture, "discover");
        let state = fixture.marker().unwrap();
        assert_eq!(state["privateEnvironmentPresent"], false);
        assert_eq!(state["inheritedControl"], false);
        let started = Instant::now();
        if let Some(byte) = request {
            owner.control.as_mut().unwrap().write_all(&[byte]).unwrap();
        } else {
            owner.control.take();
        }
        let cleanup = owner.cleanup();
        assert_cleanup(
            &cleanup,
            match request {
                Some(b'I') => "SIGINT",
                Some(b'T') => "SIGTERM",
                _ => "controller_eof",
            },
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let pid = state["pid"].as_i64().unwrap() as libc::pid_t;
        // SAFETY: kill with signal zero only queries whether the recorded
        // adapter PID still exists; no process receives a signal.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}

#[test]
fn actual_owner_sigint_and_sigterm_cancel_after_handlers_are_ready() {
    for (signal, reason) in [(libc::SIGINT, "SIGINT"), (libc::SIGTERM, "SIGTERM")] {
        let fixture = Fixture::new();
        let mut owner = Owner::start(&fixture, None, false);
        owner.wait_quiet(&fixture, "discover");
        let started = Instant::now();
        // The quiet adapter request proves handlers were installed before its
        // spawn. Only this exact helper owner receives the CLI signal.
        // SAFETY: child owns this live, unreaped helper PID.
        assert_eq!(
            unsafe { libc::kill(owner.child.id() as libc::pid_t, signal) },
            0
        );
        assert_cleanup(&owner.cleanup(), reason);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

#[test]
fn cancellation_during_adapter_close_retains_actual_cleanup_evidence() {
    let fixture = Fixture::new();
    let mut owner = Owner::start(&fixture, None, true);
    owner.wait_quiet(&fixture, "close");
    let started = Instant::now();
    owner.control.as_mut().unwrap().write_all(b"T").unwrap();
    assert_cleanup(&owner.cleanup(), "SIGTERM");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn malformed_control_request_is_infrastructure_failure_and_still_cleans() {
    let fixture = Fixture::new();
    let mut owner = Owner::start(&fixture, None, false);
    owner.wait_quiet(&fixture, "discover");
    owner.control.as_mut().unwrap().write_all(b"X").unwrap();
    let cleanup = owner.cleanup();
    assert_eq!(cleanup["complete"], true);
    assert_eq!(cleanup["adapterCloseSucceeded"], true);
    assert_eq!(cleanup["runnerShutdown"], true);
    assert_eq!(cleanup["campaignSucceeded"], false);
    assert_eq!(cleanup["cancelled"], false);
    assert_eq!(cleanup["cancellationReason"], Value::Null);
    assert!(
        fs::read_to_string(fixture.0.join("owner.stderr"))
            .unwrap()
            .contains("invalid campaign cancellation request")
    );
}
