//! Replace the CLI with the campaign controller; native commands keep one owner.
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::args::MutateArgs;

#[derive(Debug, PartialEq, Eq)]
enum LaunchError {
    InvocationDirectory,
    Installation,
    Controller,
    Runner,
}

impl LaunchError {
    fn diagnostic(&self) -> &'static str {
        match self {
            Self::InvocationDirectory => "failforge: invocation directory must be absolute",
            Self::Installation => {
                "failforge: campaign installation is unavailable; use --installation PATH"
            }
            Self::Controller => "failforge: campaign controller is missing from the installation",
            Self::Runner => "failforge: cannot locate the native Runner executable",
        }
    }
}

fn prepare(
    arguments: &MutateArgs,
    invocation_cwd: &Path,
    default_installation: &Path,
    runner: &Path,
) -> Result<Command, LaunchError> {
    if !invocation_cwd.is_absolute() {
        return Err(LaunchError::InvocationDirectory);
    }
    let requested = arguments
        .installation
        .as_deref()
        .unwrap_or(default_installation);
    let installation = invocation_cwd
        .join(requested)
        .canonicalize()
        .map_err(|_| LaunchError::Installation)?;
    if !installation.is_dir() {
        return Err(LaunchError::Installation);
    }
    let entry = installation.join("python/failforge_campaign/cli.py");
    if !entry.is_file() {
        return Err(LaunchError::Controller);
    }
    let runner = runner.canonicalize().map_err(|_| LaunchError::Runner)?;
    if !runner.is_file() {
        return Err(LaunchError::Runner);
    }
    let mut command = Command::new(&arguments.python);
    command
        .arg("-E")
        .arg(entry)
        .args(arguments.engine_arguments())
        .arg("--installation")
        .arg(installation)
        .arg("--runner")
        .arg(runner)
        .current_dir(invocation_cwd);
    Ok(command)
}

pub(crate) fn execute(arguments: MutateArgs, invocation_cwd: &Path) -> ExitCode {
    let default_installation = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let runner = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => {
            let _ = writeln!(std::io::stderr(), "{}", LaunchError::Runner.diagnostic());
            return ExitCode::from(125);
        }
    };
    let mut command = match prepare(&arguments, invocation_cwd, &default_installation, &runner) {
        Ok(command) => command,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "{}", error.diagnostic());
            return ExitCode::from(125);
        }
    };
    let installed = arguments
        .installation
        .as_deref()
        .unwrap_or(&default_installation);
    let config = (|| {
        let context = failforge_config::Context::new(invocation_cwd, installed)?;
        let values = arguments
            .configuration_values()
            .map_err(failforge_config::ConfigError::new)?;
        failforge_config::resolve_config(&values, &context)
    })();
    let config = match config {
        Ok(value) => value,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            return ExitCode::from(2);
        }
    };
    let handoff = match configuration_handoff(&config.to_transport_value()) {
        Ok(file) => file,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: cannot transfer normalized configuration"
            );
            return ExitCode::from(125);
        }
    };
    command
        .arg("--effective-config-fd")
        .arg(handoff.as_raw_fd().to_string());
    // No Tokio runtime, Runner, signal handler or additional supervisor exists
    // on this path. Python replaces this PID and owns campaign cancellation.
    let _error = command.exec();
    let _ = writeln!(
        std::io::stderr(),
        "failforge: cannot execute the Python campaign controller; check --python PROGRAM"
    );
    ExitCode::from(125)
}

/// Keep normalized data out of argv/environment limits and leave no named file.
/// The remaining Python execution controller closes this descriptor on read.
fn configuration_handoff(value: &serde_json::Value) -> std::io::Result<File> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut file = None;
    for _ in 0..128 {
        let name = std::env::temp_dir().join(format!(
            "failforge-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&name)
        {
            Ok(opened) => {
                if let Err(error) = std::fs::remove_file(&name) {
                    drop(opened);
                    return Err(error);
                }
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let mut file = file.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "configuration handoff unavailable",
        )
    })?;
    serde_json::to_writer(&mut file, value)?;
    file.seek(SeekFrom::Start(0))?;
    if file.as_raw_fd() < 3 {
        // SAFETY: duplicate our live descriptor above the standard-fd range;
        // File takes ownership only after a successful duplication.
        let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate < 0 {
            return Err(std::io::Error::last_os_error());
        }
        file = unsafe { File::from_raw_fd(duplicate) };
    }
    let descriptor = file.as_raw_fd();
    // SAFETY: this file owns a live descriptor; F_GETFD/F_SETFD access only its
    // flags. Only this private handoff is made inheritable across controller exec.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
