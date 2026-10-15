//! Replace the CLI with the campaign controller; native commands keep one owner.
use std::io::Write;
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
    // No Tokio runtime, Runner, signal handler or additional supervisor exists
    // on this path. Python replaces this PID and owns campaign cancellation.
    let _error = command.exec();
    let _ = writeln!(
        std::io::stderr(),
        "failforge: cannot execute the Python campaign controller; check --python PROGRAM"
    );
    ExitCode::from(125)
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
