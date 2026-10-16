//! Replace the CLI with the profile generator without starting execution owners.
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::args::ProfileGenerateArgs;

#[derive(Debug, PartialEq, Eq)]
enum LaunchError {
    LanguageSelection,
    InvocationDirectory,
    Installation,
    Generator,
    Runner,
}

impl LaunchError {
    fn diagnostic(&self) -> &'static str {
        match self {
            Self::LanguageSelection => {
                "failforge: --language dart is required exactly when --family language_specific is selected"
            }
            Self::InvocationDirectory => "failforge: invocation directory must be absolute",
            Self::Installation => {
                "failforge: profile installation is unavailable; use --installation PATH"
            }
            Self::Generator => "failforge: profile generator is missing from the installation",
            Self::Runner => {
                "failforge: cannot locate the native Runner executable for profile analysis"
            }
        }
    }
}

fn prepare(
    arguments: &ProfileGenerateArgs,
    invocation_cwd: &Path,
    default_installation: &Path,
    runner: Option<&Path>,
) -> Result<Command, LaunchError> {
    if !arguments.has_valid_language_selection() {
        return Err(LaunchError::LanguageSelection);
    }
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
    let entry = installation.join("python/failforge_campaign/profile_generate.py");
    if !entry.is_file() {
        return Err(LaunchError::Generator);
    }
    let mut command = Command::new(&arguments.python);
    command
        .arg("-E")
        .arg(entry)
        .args(arguments.engine_arguments())
        .arg("--installation")
        .arg(installation)
        .current_dir(invocation_cwd);
    if !arguments.template {
        let runner = runner
            .filter(|path| path.is_absolute())
            .ok_or(LaunchError::Runner)?;
        let runner = runner.canonicalize().map_err(|_| LaunchError::Runner)?;
        if !runner.is_file() {
            return Err(LaunchError::Runner);
        }
        command.arg("--runner").arg(runner);
    }
    Ok(command)
}

pub(crate) fn execute(arguments: ProfileGenerateArgs, invocation_cwd: &Path) -> ExitCode {
    let default_installation = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let runner = if arguments.template {
        None
    } else {
        match std::env::current_exe() {
            Ok(path) => Some(path),
            Err(_) => {
                let _ = writeln!(std::io::stderr(), "{}", LaunchError::Runner.diagnostic());
                return ExitCode::from(125);
            }
        }
    };
    let mut command = match prepare(
        &arguments,
        invocation_cwd,
        &default_installation,
        runner.as_deref(),
    ) {
        Ok(command) => command,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "{}", error.diagnostic());
            return ExitCode::from(if error == LaunchError::LanguageSelection {
                2
            } else {
                125
            });
        }
    };
    // Native dispatch starts neither a Runner nor a Tokio runtime. Python owns
    // analysis execution; exec preserves its status and signal ownership.
    let _error = command.exec();
    let _ = writeln!(
        std::io::stderr(),
        "failforge: cannot execute the Python profile generator; check --python PROGRAM"
    );
    ExitCode::from(125)
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
