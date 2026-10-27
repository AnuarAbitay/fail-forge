//! A private pipe cancels the native owner even before it installs handlers.
//! The owner retains its lease and drains its Runner when the controller dies.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use failforge_dart::MAX_LINE_BYTES;
use serde_json::Value;
use tokio::io::{Interest, unix::AsyncFd};

use super::transcript::{Transcript, with_diagnostics};
use super::workspaces::{Recovery, Workspace};
use super::{Cancellation, FamilyApplicability, Runtime, failure};
use crate::ProfileError;

const CANCEL_ENV: &str = "FAILFORGE_ADAPTER_CANCEL_FD";

struct NativeOwner {
    child: Option<Child>,
    control: Option<File>,
    recovery: Option<Recovery>,
}

type ReaperState = Arc<Mutex<Option<(Child, Option<Recovery>)>>>;

impl Drop for NativeOwner {
    fn drop(&mut self) {
        self.control.take(); // EOF tells the native helper to drain its Runner.
        let Some(child) = self.child.take() else {
            return;
        };
        let recovery = self.recovery.take();
        let state = Arc::new(Mutex::new(Some((child, recovery))));
        let retained = state.clone();
        let reap = |state: ReaperState| {
            let value = state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some((mut child, recovery)) = value {
                if child.wait().is_ok() {
                    if let Some(recovery) = recovery {
                        let _ = recovery.recover();
                    }
                }
            }
        };
        if std::thread::Builder::new()
            .name("profile-owner-reaper".into())
            .spawn(move || reap(retained))
            .is_err()
        {
            // Thread exhaustion must not abandon a child. This exceptional
            // fallback blocks only until the helper has drained on pipe EOF.
            reap(state);
        }
    }
}

fn pipe() -> io::Result<(File, File)> {
    let mut raw = [-1; 2];
    // SAFETY: raw has space for the two fresh returned descriptors.
    if unsafe { libc::pipe(raw.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe returned two unique, live descriptors.
    let pair = unsafe { (File::from_raw_fd(raw[0]), File::from_raw_fd(raw[1])) };
    fn private(file: File) -> io::Result<File> {
        // Duplicate away from standard streams; keep all parent descriptors
        // CLOEXEC. Only the two intended capabilities are inherited in pre_exec.
        // SAFETY: fcntl borrows file and returns a new uniquely owned descriptor.
        let copied = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: copied is the fresh successful descriptor above.
        Ok(unsafe { File::from_raw_fd(copied) })
    }
    Ok((private(pair.0)?, private(pair.1)?))
}

fn nonblocking(file: &File) -> io::Result<()> {
    // SAFETY: fcntl operations use only a live integer descriptor/flags.
    let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if status < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, status | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn request_cancel(writer: &mut Option<File>, signal: i32) -> Result<(), ProfileError> {
    if let Some(mut pipe) = writer.take() {
        let byte = if signal == libc::SIGTERM { b'T' } else { b'I' };
        match pipe.write_all(&[byte]) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => (),
            Err(error) => return Err(error.into()),
        }
        // Closing the sole controller writer also makes controller death an
        // observable EOF, never a PID-based guess at cancellation readiness.
    }
    Ok(())
}

pub(super) async fn discover(
    runtime: &Runtime,
    package: &Path,
    workspace: &Workspace,
    payload: &Value,
    family: &str,
    requested: &[String],
    cancel: &Cancellation,
) -> Result<FamilyApplicability, ProfileError> {
    cancel.check()?;
    let source = payload["rules"]["includePaths"]
        .as_array()
        .and_then(|values| values.first())
        .and_then(Value::as_str)
        .ok_or_else(|| failure("analysis request has no selected file"))?;
    let output = workspace.path.join(format!("discovery-{family}"));
    fs::create_dir(&output)?;
    let payload_path = output.join("payload.json");
    fs::write(
        &payload_path,
        serde_json::to_vec(payload).map_err(|_| failure("cannot serialize analysis request"))?,
    )?;
    let errors = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(output.join("adapter.stderr"))?;
    let (control_read, control_write) = pipe()?;
    nonblocking(&control_write)?;
    let (stdout_read, stdout_write) = pipe()?;
    nonblocking(&stdout_read)?;
    let stdout = AsyncFd::with_interest(stdout_read, Interest::READABLE)?;
    let control_fd = control_read.as_raw_fd();
    let lease_fd = workspace.lease_fd();
    let mut command = Command::new(&runtime.helper);
    command
        .args([
            runtime.dart_sdk.join("bin/dart"),
            runtime.adapter.clone(),
            payload_path,
        ])
        .env_clear()
        .envs(runtime.environment())
        .env(CANCEL_ENV, control_fd.to_string())
        .env(failforge_runner::WORKSPACE_LEASE_ENV, lease_fd.to_string())
        .stdin(Stdio::null())
        .stdout(stdout_write)
        .stderr(errors);
    // SAFETY: the callback executes after fork with exclusively async-signal-safe
    // integer syscalls; descriptors remain alive until spawn has completed.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            for fd in [control_fd, lease_fd] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let recovery = workspace.recovery()?;
    let child = command.spawn()?;
    let mut owner = NativeOwner {
        child: Some(child),
        control: Some(control_write),
        recovery: Some(recovery),
    };
    drop(command); // Drop the parent's duplicate stdout writer before reading EOF.
    drop(control_read);
    let mut transcript = Transcript::new(package, family, requested, source);
    let mut error = None;
    let mut line = Vec::new();
    let mut buffer = [0; 32 * 1024];
    let mut eof = false;
    let mut exit = None;
    let mut signalled = false;
    while !eof || exit.is_none() {
        tokio::select! {
            signal = cancel.cancelled(), if !signalled => {
                signalled = true;
                if let Err(problem) = request_cancel(&mut owner.control, signal) { error.get_or_insert(problem); }
            },
            ready = stdout.readable(), if !eof => {
                match ready {
                    Err(problem) => { error.get_or_insert(problem.into()); eof = true; }
                    Ok(mut guard) => match guard.try_io(|ready| {
                        let mut file = ready.get_ref();
                        file.read(&mut buffer)
                    }) {
                        Err(_) => (),
                        Ok(Err(problem)) if problem.kind() == io::ErrorKind::Interrupted => (),
                        Ok(Err(problem)) => { error.get_or_insert(problem.into()); eof = true; },
                        Ok(Ok(0)) => { eof = true; },
                        Ok(Ok(count)) => {
                            if error.is_none() {
                                for byte in &buffer[..count] {
                                    if *byte == b'\n' {
                                        if let Err(problem) = transcript.accept(&line) { error = Some(problem); break; }
                                        line.clear();
                                    } else if line.len() >= MAX_LINE_BYTES {
                                        error = Some(failure("adapter campaign record exceeds eight MiB"));
                                        break;
                                    } else { line.push(*byte); }
                                }
                            }
                        },
                    },
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(10)), if exit.is_none() => {
                match owner.child.as_mut().expect("live owner").try_wait() {
                    Ok(status) => exit = status,
                    Err(problem) => { error.get_or_insert(problem.into()); break; }
                }
            },
        }
        if error.is_some() && owner.control.is_some() {
            let _ = request_cancel(&mut owner.control, libc::SIGINT);
        }
    }
    if exit.is_some() {
        owner.child.take();
    } // A successful try_wait reaped it.
    owner.control.take();
    if error.is_none() && !line.is_empty() {
        error = Some(failure(
            "adapter campaign ended with an unterminated record",
        ));
    }
    if let Some(error) = error {
        return Err(with_diagnostics(error, &output, family));
    }
    let result = transcript.finish(exit.expect("loop waits for owner exit"), cancel.signum());
    result.map_err(|error| with_diagnostics(error, &output, family))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "failforge-profile-transcript-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn diagnostics_are_bounded_and_never_follow_aliases_or_block_on_fifos() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let errors = fixture.0.join("adapter.stderr");
        fs::write(&errors, vec![b'x'; 5000]).unwrap();
        let message = with_diagnostics(failure("failed"), &fixture.0, "classic").to_string();
        assert!(message.ends_with("[truncated]"));
        assert!(message.len() < 4300);
        fs::write(
            &errors,
            "adapter failed \u{202e}txt.exe\u{202c}\u{001b}[31m",
        )
        .unwrap();
        let message = with_diagnostics(failure("failed"), &fixture.0, "classic").to_string();
        assert!(!message.contains(['\u{202e}', '\u{202c}', '\u{001b}']));
        fs::remove_file(&errors).unwrap();
        let secret = fixture.0.join("secret");
        fs::write(&secret, "do not follow").unwrap();
        symlink(&secret, &errors).unwrap();
        assert!(
            !with_diagnostics(failure("failed"), &fixture.0, "classic")
                .to_string()
                .contains("do not follow")
        );
        fs::remove_file(&errors).unwrap();
        let path = std::ffi::CString::new(errors.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: path is a live terminated string naming a fresh test fixture.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(
            !with_diagnostics(failure("failed"), &fixture.0, "classic")
                .to_string()
                .contains("stderr:")
        );
    }

    #[tokio::test]
    async fn dropping_discovery_reaps_native_owner_and_recovers_real_workspace() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let helper = fixture.0.join("helper");
        let ready = fixture.0.join("ready");
        // A deterministic fake owner blocks on exactly the inherited private
        // cancellation descriptor. It exits only when dropping the future
        // closes the controller writer; no sleeps or author code are involved.
        let ready_text = ready.to_str().unwrap().replace('\'', "'\"'\"'");
        fs::write(&helper,format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > '{ready_text}'\neval \"cat <&$FAILFORGE_ADAPTER_CANCEL_FD\" >/dev/null\n")).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = Runtime {
            helper,
            dart_sdk: fixture.0.clone(),
            flutter_sdk: None,
            adapter: fixture.0.clone(),
            installation: fixture.0.clone(),
        };
        let workspace = Workspace::new(std::slice::from_ref(&fixture.0)).unwrap();
        let campaign = workspace.path.parent().unwrap().to_owned();
        let cancel = Cancellation::default();
        let payload = json!({"rules":{"includePaths":["lib/a.dart"]}});
        let mut operation = Box::pin(discover(
            &runtime,
            &fixture.0,
            &workspace,
            &payload,
            "classic",
            &[],
            &cancel,
        ));
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut operation => panic!("fake owner ended before EOF: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {
                        if let Some(pid) = ready_pid(&ready).unwrap() {
                            break pid;
                        }
                    },
                }
            }
        })
        .await
        .unwrap();
        drop(operation);
        drop(workspace);
        tokio::time::timeout(Duration::from_secs(5), async {
            while campaign.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("detached cleanup must remove its leased campaign");
        let mut status = 0;
        // SAFETY: pid is the captured test child, status is writable. ECHILD
        // proves the guard waited it; returning pid would expose a zombie.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    // Shell redirection creates the file before printf writes the PID. Require
    // the terminating newline so neither an empty file nor a partial PID can
    // announce readiness or identify the wrong child.
    fn ready_pid(path: &Path) -> io::Result<Option<libc::pid_t>> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let Some(record) = raw.strip_suffix('\n') else {
            return Ok(None);
        };
        let pid = record
            .parse::<libc::pid_t>()
            .ok()
            .filter(|pid| *pid > 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid ready PID"))?;
        Ok(Some(pid))
    }

    #[test]
    fn discovery_readiness_waits_for_the_complete_pid_record() {
        let fixture = Fixture::new();
        let ready = fixture.0.join("ready");
        assert_eq!(ready_pid(&ready).unwrap(), None);
        for incomplete in ["", "12", "1234"] {
            fs::write(&ready, incomplete).unwrap();
            assert_eq!(ready_pid(&ready).unwrap(), None);
        }
        fs::write(&ready, "12345\n").unwrap();
        assert_eq!(ready_pid(&ready).unwrap(), Some(12345));
        for invalid in ["\n", "0\n", "-1\n", "invalid\n", "12\n34\n"] {
            fs::write(&ready, invalid).unwrap();
            assert_eq!(
                ready_pid(&ready).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}
