//! Actual leased CLI owners and nested runs; no Dart/Flutter tooling is used.
#![cfg(unix)]

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use failforge_runner::{WORKSPACE_LEASE_ENV, WORKSPACE_LEASE_MAGIC};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct LeaseFixture {
    directory: PathBuf,
    file: File,
}

impl LeaseFixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "failforge cli lease {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        DirBuilder::new().mode(0o700).create(&directory).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join("lease"))
            .unwrap();
        file.write_all(WORKSPACE_LEASE_MAGIC).unwrap();
        // SAFETY: this private live fixture fd is the only requested flock.
        assert_eq!(unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }, 0);
        Self { directory, file }
    }

    fn run(&self, inherited: bool, options: &[&str], child: &[&str]) -> (Output, PathBuf) {
        let capture = self
            .directory
            .join(format!("capture-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut command = Command::new(env!("CARGO_BIN_EXE_failforge"));
        command
            .args(["run", "--timeout", "5s", "--capture-dir"])
            .arg(&capture)
            .args(options)
            .arg("--")
            .args(child)
            .stdin(Stdio::null())
            .current_dir(&self.directory);
        if inherited {
            let descriptor = self.file.as_raw_fd();
            command.env(WORKSPACE_LEASE_ENV, descriptor.to_string());
            // SAFETY: the fork-child callback only changes fd flags. It never
            // mutates this test process's environment or inheritable fd state.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(descriptor, libc::F_GETFD);
                    if flags < 0
                        || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            command.env_remove(WORKSPACE_LEASE_ENV);
        }
        (command.output().unwrap(), capture)
    }
}

impl Drop for LeaseFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn leased_owner_removes_private_environment_even_when_explicitly_set() {
    let fixture = LeaseFixture::new();
    let (output, capture) = fixture.run(
        true,
        &["--env", "FAILFORGE_WORKSPACE_LEASE_FD=subject-override"],
        &[
            env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
            "env",
            WORKSPACE_LEASE_ENV,
        ],
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(fs::read(capture.join("stdout.bin")).unwrap(), b"<unset>\n");
}

#[test]
fn leased_owner_can_run_a_nested_failforge_command() {
    let fixture = LeaseFixture::new();
    let inner_capture = fixture.directory.join("inner-capture");
    let (output, capture) = fixture.run(
        true,
        &[],
        &[
            env!("CARGO_BIN_EXE_failforge"),
            "run",
            "--timeout",
            "2s",
            "--capture-dir",
            inner_capture.to_str().unwrap(),
            "--",
            env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
            "exit",
            "0",
        ],
    );
    let child_errors = fs::read(capture.join("stderr.bin")).unwrap();
    assert!(output.status.success(), "{:?}", child_errors);
    assert!(inner_capture.join("stdout.bin").is_file());
    assert!(inner_capture.join("stderr.bin").is_file());
    assert!(
        String::from_utf8(child_errors)
            .unwrap()
            .contains("status=exit:0")
    );
}

#[test]
fn ordinary_nonleased_run_preserves_explicit_same_name_environment() {
    let fixture = LeaseFixture::new();
    let (output, capture) = fixture.run(
        false,
        &["--env", "FAILFORGE_WORKSPACE_LEASE_FD=user-value"],
        &[
            env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
            "env",
            WORKSPACE_LEASE_ENV,
        ],
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        fs::read(capture.join("stdout.bin")).unwrap(),
        b"757365722d76616c7565\n"
    );
}
