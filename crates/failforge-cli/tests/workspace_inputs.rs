//! Private filesystem owner: validation, cancellation and inherited lease lifetime.
#![cfg(unix)]

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use failforge_runner::{WORKSPACE_LEASE_ENV, WORKSPACE_LEASE_MAGIC};
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);
const REQUEST: &str = "FAILFORGE_WORKSPACE_REQUEST_FD";

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff-workspace-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        Self {
            root: root.canonicalize().unwrap(),
            child: None,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_failforge"));
        command
            .arg("workspace-inputs")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .env_remove(REQUEST)
            .env_remove(WORKSPACE_LEASE_ENV)
            .env_remove("FAILFORGE_RUNNER_READY_FD")
            .stdout(File::create(self.root.join("stdout")).unwrap())
            .stderr(File::create(self.root.join("stderr")).unwrap());
        command
    }

    fn request(&self, command: &mut Command, bytes: &[u8], unlinked: bool) -> File {
        let path = self.root.join("request");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        if unlinked {
            fs::remove_file(path).unwrap();
        }
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        inherit(command, REQUEST, &file);
        file
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "native workspace owner did not terminate"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        Output {
            status,
            stdout: fs::read(self.root.join("stdout")).unwrap(),
            stderr: fs::read(self.root.join("stderr")).unwrap(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn inherit(command: &mut Command, name: &str, file: &File) {
    let descriptor = file.as_raw_fd();
    command.env(name, descriptor.to_string());
    // SAFETY: only async-signal-safe fcntl calls on a retained live descriptor.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn pipe() -> (File, File) {
    let mut descriptors = [-1; 2];
    // SAFETY: pipe initializes exactly two descriptors in this local array.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: a successful pipe call created these two uniquely owned handles.
    let files = unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    };
    for descriptor in descriptors {
        // SAFETY: only set close-on-exec on our live, newly created descriptors.
        assert_eq!(
            unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    files
}

fn wait_readable(file: &File) {
    let mut poll = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll receives one valid local entry and a bounded timeout.
    assert_eq!(
        unsafe { libc::poll(&mut poll, 1, 5_000) },
        1,
        "missing readiness/EOF"
    );
}

#[test]
fn invalid_request_fails_before_readiness_and_copy_creation() {
    for mode in ["missing", "named", "duplicate", "relative", "uncommitted"] {
        let mut fixture = Fixture::new();
        let destination = fixture.root.join("copy");
        let mut command = fixture.command();
        let (mut reader, writer) = pipe();
        inherit(&mut command, "FAILFORGE_RUNNER_READY_FD", &writer);
        let value = json!({"operation":"copy-project", "source":fixture.root.join("source"),
            "destination":if mode == "relative" { PathBuf::from("copy") } else { destination.clone() }});
        let mut bytes = if mode == "duplicate" {
            b"{\"operation\":\"inspect\",\"operation\":\"copy-project\"}\n".to_vec()
        } else {
            format!("{value}\n").into_bytes()
        };
        if mode == "uncommitted" {
            bytes.pop();
        }
        let _request =
            (mode != "missing").then(|| fixture.request(&mut command, &bytes, mode != "named"));
        fixture.child = Some(command.spawn().unwrap());
        drop(writer);
        let output = fixture.finish();
        assert_eq!(
            output.status.code(),
            Some(125),
            "{mode}: {:?}",
            output.stderr
        );
        assert!(output.stdout.is_empty(), "{mode}");
        assert!(!destination.exists(), "{mode}");
        wait_readable(&reader);
        let mut acknowledgement = Vec::new();
        reader.read_to_end(&mut acknowledgement).unwrap();
        assert!(
            acknowledgement.is_empty(),
            "invalid request acknowledged: {mode}"
        );
    }
}

#[test]
fn signals_cancel_ready_owner_and_lease_survives_controller_descriptor_close() {
    for signum in [libc::SIGINT, libc::SIGTERM] {
        let mut fixture = Fixture::new();
        let source = fixture.root.join("source");
        fs::create_dir(&source).unwrap();
        // A sparse file keeps the pending operation from completing before the
        // readiness-triggered signal. Readiness precedes dispatch, so this
        // checks the signal/owner contract, not an exact interruption offset.
        File::create(source.join("large.bin"))
            .unwrap()
            .set_len(8 * 1024 * 1024 * 1024)
            .unwrap();
        let lease_path = fixture.root.join("lease");
        let mut lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&lease_path)
            .unwrap();
        lease.write_all(WORKSPACE_LEASE_MAGIC).unwrap();
        // SAFETY: acquire a flock on our own live fixture file.
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX) }, 0);
        let mut command = fixture.command();
        inherit(&mut command, WORKSPACE_LEASE_ENV, &lease);
        let request = format!("{}\n", json!({"operation":"source-hashes", "root":source}));
        let _request = fixture.request(&mut command, request.as_bytes(), true);
        let (mut reader, writer) = pipe();
        inherit(&mut command, "FAILFORGE_RUNNER_READY_FD", &writer);
        fixture.child = Some(command.spawn().unwrap());
        drop(writer);
        wait_readable(&reader);
        let mut acknowledgement = [0];
        reader.read_exact(&mut acknowledgement).unwrap();
        assert_eq!(acknowledgement, *b"R");
        drop(lease);
        let probe = File::open(lease_path).unwrap();
        // SAFETY: nonblocking lock check on an independently opened live fd.
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // SAFETY: signal targets only this test's retained, unreaped child.
        assert_eq!(
            unsafe { libc::kill(fixture.child.as_ref().unwrap().id() as i32, signum) },
            0
        );
        let output = fixture.finish();
        assert_eq!(
            output.status.code(),
            Some(128 + signum),
            "{:?}",
            output.stderr
        );
        assert!(
            output.stdout.is_empty(),
            "cancelled operation published a binding"
        );
        // SAFETY: after owner exit the inherited lease must be fully released.
        assert_eq!(
            unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }
}
