use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand, error::ErrorKind};
use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, OutputMode, TimeoutPolicy};

#[derive(Parser)]
#[command(
    name = "failforge",
    version,
    about = "Run external commands with bounded capture and cleanup"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Run a program directly; child arguments must follow --.
    Run(RunArgs),
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Execution limit: an integer followed by ms, s, or m.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    timeout: Option<Duration>,
    /// Working directory of the child, relative to this invocation.
    #[arg(long, value_name = "PATH")]
    cwd: Option<PathBuf>,
    /// Display child output as well as capturing it.
    #[arg(long)]
    stream: bool,
    /// Save raw output in a directory relative to this invocation.
    #[arg(long, value_name = "PATH")]
    capture_dir: Option<PathBuf>,
    /// Override a child environment variable; the last assignment wins.
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<OsString>,
    /// Remove a child environment variable.
    #[arg(long, value_name = "KEY")]
    env_remove: Vec<OsString>,
    /// Start with an empty child environment.
    #[arg(long)]
    clear_env: bool,
    /// Wait after the graceful signal before forcing termination.
    #[arg(long, value_name = "DURATION", default_value = "2s", value_parser = parse_duration)]
    grace_period: Duration,
    #[arg(last = true, required = true, num_args = 1.., value_name = "PROGRAM [ARGS...]")]
    command: Vec<OsString>,
}

impl RunArgs {
    pub(crate) fn into_spec(self, invocation_cwd: &Path) -> Result<CommandSpec, clap::Error> {
        if !invocation_cwd.is_absolute() {
            return Err(config_error("the invocation directory must be absolute"));
        }
        let mut command = self.command.into_iter();
        let program = command
            .next()
            .ok_or_else(|| config_error("a program is required after --"))?;
        let mut env = EnvSpec {
            clear: self.clear_env,
            ..EnvSpec::default()
        };
        for assignment in self.env {
            let (key, value) = split_env_assignment(assignment)?;
            env.set.insert(key, value);
        }
        env.remove.extend(self.env_remove);

        let cwd = self
            .cwd
            .map(|path| invocation_cwd.join(path))
            .unwrap_or_else(|| invocation_cwd.to_path_buf());
        let capture_dir = self.capture_dir.map(|path| invocation_cwd.join(path));
        let spec = CommandSpec {
            program,
            args: command.collect(),
            cwd,
            env,
            timeout_policy: TimeoutPolicy {
                timeout: self.timeout,
                grace_period: self.grace_period,
                ..TimeoutPolicy::default()
            },
            capture_policy: CapturePolicy {
                mode: if self.stream {
                    OutputMode::StreamAndCapture
                } else {
                    OutputMode::Capture
                },
                capture_dir,
                ..CapturePolicy::default()
            },
        };
        spec.validate()
            .map_err(|error| config_error(error.to_string()))?;
        Ok(spec)
    }
}

fn config_error(message: impl Into<String>) -> clap::Error {
    Cli::command().error(ErrorKind::ValueValidation, message.into())
}

fn split_env_assignment(assignment: OsString) -> Result<(OsString, OsString), clap::Error> {
    let bytes = assignment.as_os_str().as_bytes();
    let separator = bytes
        .iter()
        .position(|&byte| byte == b'=')
        .ok_or_else(|| config_error("--env requires KEY=VALUE"))?;
    Ok((
        OsString::from_vec(bytes[..separator].to_vec()),
        OsString::from_vec(bytes[separator + 1..].to_vec()),
    ))
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    let (number, unit) = if let Some(number) = text.strip_suffix("ms") {
        (number, "ms")
    } else if let Some(number) = text.strip_suffix('s') {
        (number, "s")
    } else if let Some(number) = text.strip_suffix('m') {
        (number, "m")
    } else {
        return Err("duration must be an integer followed by ms, s, or m".into());
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("duration must contain an unsigned decimal integer".into());
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| "duration is too large".to_owned())?;
    match unit {
        "ms" => Ok(Duration::from_millis(amount)),
        "s" => Ok(Duration::from_secs(amount)),
        "m" => amount
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| "duration is too large".to_owned()),
        _ => unreachable!("unit selected from the three recognized suffixes"),
    }
}

fn parse_timeout(text: &str) -> Result<Duration, String> {
    let duration = parse_duration(text)?;
    if duration.is_zero() {
        return Err("timeout must be greater than zero".into());
    }
    Ok(duration)
}

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;
