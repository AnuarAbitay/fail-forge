//! Public tool interruption retains a receipt and drains its exact child.
#![cfg(unix)]

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

fn executable(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff-tools-public-signal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let release = root.join("target/release");
        for name in ["failforge", "failforge-process-fixture"] {
            executable(&release.join(name), "#!/bin/sh\nexit 0\n");
        }
        executable(
            &release.join("examples/microbench"),
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$FF_TOOLS_CHILD_MARKER\"\nexec /bin/sleep 10\n",
        );
        Self { root, child: None }
    }

    fn spawn(&mut self) {
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_failforge-tools"))
                .args([
                    "benchmark",
                    "--iterations",
                    "1",
                    "--warmup",
                    "0",
                    "--bytes",
                    "0",
                ])
                .arg("--target-dir")
                .arg(self.root.join("target"))
                .arg("--output")
                .arg(self.root.join("result"))
                .env("FF_TOOLS_CHILD_MARKER", self.root.join("child.pid"))
                .stdin(Stdio::null())
                .stdout(File::create(self.root.join("stdout")).unwrap())
                .stderr(File::create(self.root.join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
    }

    fn wait_child_started(&mut self) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = fs::read_to_string(self.root.join("child.pid"))
                .ok()
                .and_then(|text| text.parse().ok())
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "tool did not start library fixture"
            );
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(self.root.join("stderr")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            if child.try_wait().ok().flatten().is_none() {
                // SAFETY: the retained child has not been reaped.
                unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
                let deadline = Instant::now() + Duration::from_secs(2);
                while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn public_sigint_and_sigterm_drain_child_and_retain_interrupted_receipt() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut fixture = Fixture::new();
        fixture.spawn();
        let native_pid = fixture.wait_child_started();
        let tool = fixture.child.as_mut().unwrap();
        // SAFETY: tool is our retained, live direct child.
        assert_eq!(unsafe { libc::kill(tool.id() as i32, signal) }, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = tool.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "interrupted tool did not drain");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            status.code(),
            Some(128 + signal),
            "{}",
            fs::read_to_string(fixture.root.join("stderr")).unwrap()
        );
        let receipt: Value = serde_json::from_slice(
            &fs::read(fixture.root.join("result/library/command.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["ownerPid"], native_pid);
        assert_eq!(receipt["exitCode"], -signal);
        assert_eq!(receipt["cleanupUnconfirmed"], false);
        assert!(receipt["error"].as_str().unwrap().contains("interrupted"));
        assert!(!fixture.root.join("result/summary.json").exists());
        // SAFETY: signal zero only checks existence of the recorded fixture.
        assert_eq!(unsafe { libc::kill(native_pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
}
