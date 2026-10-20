//! Execute one prepared test command inside the existing native attempt owner.
//!
//! The caller retains filesystem capabilities and verifies inputs before
//! accepting this evidence. There is no recursive CLI invocation or private
//! evidence-descriptor round trip between execution and classification.
use std::path::{Path, PathBuf};
use std::time::Duration;

use failforge_dart::{SelectionContext, namespace::NamespaceError};
use failforge_runner::{
    CancelReason, CapturePolicy, CommandResult, CommandSpec, EnvSpec, RunOutcome, Runner,
    RunnerError, TimeoutPolicy,
};
use failforge_workspace::{Cancellation, PreparedProject};
use serde::Deserialize;

use crate::args::TestReporter;
use crate::test_evidence::AttemptEvidence;

const PRIVATE_ENV: [&str; 8] = [
    "FAILFORGE_WORKSPACE_LEASE_FD",
    "FAILFORGE_ADAPTER_CANCEL_FD",
    "FAILFORGE_RUNNER_READY_FD",
    "FAILFORGE_TEST_EVIDENCE_FD",
    "FAILFORGE_TEST_CONTEXT_FD",
    "FAILFORGE_WORKSPACE_REQUEST_FD",
    "FAILFORGE_WORKSPACE_HANDOFF_FD",
    "FAILFORGE_WORKSPACE_CAPS_FDS",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExecutionConfig {
    pub runner: PathBuf,
    pub dart_sdk: PathBuf,
    pub flutter_sdk: Option<PathBuf>,
    pub command_timeout: String,
    pub test_timeout: String,
}

#[derive(Debug)]
pub(crate) struct ExecutedAttempt {
    /// Compatibility report argv. The actual child is its suffix after `--`;
    /// the virtual `failforge run` prefix does not start another process.
    pub command: Vec<String>,
    pub exit_code: u8,
    pub native_ok: bool,
    pub result: Option<CommandResult>,
    pub evidence: Option<AttemptEvidence>,
    pub diagnostics: String,
}

fn invalid(message: &str) -> NamespaceError {
    NamespaceError::Invalid(message.into())
}

fn path_text(path: &Path) -> Result<String, NamespaceError> {
    path.to_str()
        .filter(|text| path.is_absolute() && !text.contains('\0'))
        .map(str::to_owned)
        .ok_or_else(|| invalid("test execution paths must be absolute Unicode paths"))
}

fn specification(
    config: &ExecutionConfig,
    project: &PreparedProject,
    copied: &Path,
    capture: &Path,
) -> Result<(CommandSpec, Vec<String>, TestReporter), NamespaceError> {
    use failforge_config::duration::milliseconds;
    let timeout = milliseconds(&config.command_timeout.clone().into(), "runner timeout")
        .map_err(|_| invalid("invalid test execution timeout"))?;
    milliseconds(&config.test_timeout.clone().into(), "test timeout")
        .map_err(|_| invalid("invalid test execution timeout"))?;
    let reporter = if project.flutter {
        TestReporter::Flutter
    } else {
        TestReporter::Dart
    };
    let mut command = vec![
        path_text(&config.runner)?,
        "run".into(),
        "--timeout".into(),
        config.command_timeout.clone(),
        "--cwd".into(),
        path_text(copied)?,
        "--capture-dir".into(),
        path_text(capture)?,
        "--test-reporter".into(),
        reporter.as_str().into(),
        "--".into(),
    ];
    let mut child = if project.flutter {
        let sdk = config
            .flutter_sdk
            .as_ref()
            .ok_or_else(|| invalid("Flutter test execution requires its SDK"))?;
        vec![
            path_text(&sdk.join("bin/flutter"))?,
            "--suppress-analytics".into(),
            "test".into(),
            "--no-pub".into(),
            "--machine".into(),
        ]
    } else {
        let test = project
            .packages
            .get("test")
            .ok_or_else(|| invalid("test execution requires the prepared test package"))?;
        vec![
            path_text(&config.dart_sdk.join("bin/dart"))?,
            format!(
                "--packages={}",
                path_text(&copied.join(".dart_tool/package_config.json"))?
            ),
            path_text(&test.join("bin/test.dart"))?,
            "--reporter".into(),
            "json".into(),
            "--platform".into(),
            "vm".into(),
        ]
    };
    child.extend([
        "--concurrency".into(),
        "1".into(),
        "--timeout".into(),
        config.test_timeout.clone(),
    ]);
    command.extend(child.iter().cloned());
    let spec = CommandSpec {
        program: child[0].clone().into(),
        args: child.into_iter().skip(1).map(Into::into).collect(),
        cwd: copied.to_owned(),
        env: EnvSpec {
            remove: PRIVATE_ENV.into_iter().map(Into::into).collect(),
            ..EnvSpec::default()
        },
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::from_millis(timeout)),
            ..TimeoutPolicy::default()
        },
        capture_policy: CapturePolicy {
            capture_dir: Some(capture.to_owned()),
            ..CapturePolicy::default()
        },
    };
    spec.validate()
        .map_err(|_| invalid("invalid test execution command"))?;
    Ok((spec, command, reporter))
}

fn completed(
    command: Vec<String>,
    outcome: RunOutcome,
    shutdown: Result<(), RunnerError>,
) -> ExecutedAttempt {
    let native_ok = outcome.is_ok() && shutdown.is_ok();
    let (result, mut exit_code, mut diagnostics) = match outcome {
        Ok(result) => (
            Some((*result).clone()),
            crate::result_exit_code(&result),
            crate::summary(&result),
        ),
        Err(error) => {
            let result = error.partial_result().cloned();
            let mut diagnostics = format!("failforge: {error}\n");
            if let Some(result) = &result {
                diagnostics.push_str(&crate::summary(result));
            }
            (result, 125, diagnostics)
        }
    };
    if let Err(error) = shutdown {
        exit_code = 125;
        diagnostics.push_str(&format!("failforge: {error}\n"));
    }
    ExecutedAttempt {
        command,
        exit_code,
        native_ok,
        result,
        evidence: None,
        diagnostics,
    }
}

/// Execute and drain before reading captures or inspecting suite source files.
/// The outer owner applies source-integrity and late-cancellation precedence,
/// then removes the workspace. It must keep its runtime and lease alive until
/// this future returns, including the joined blocking evidence analysis.
pub(crate) async fn execute(
    config: &ExecutionConfig,
    project: &PreparedProject,
    copied: &Path,
    capture: &Path,
    context: &SelectionContext,
    cancel: &Cancellation,
) -> Result<ExecutedAttempt, NamespaceError> {
    failforge_dart::validate_selection_context(context)
        .map_err(|_| invalid("invalid test selection context"))?;
    let (spec, command, reporter) = specification(config, project, copied, capture)?;
    cancel.check()?;
    let runner = Runner::new();
    let mut handle = match runner.start(spec) {
        Ok(handle) => handle,
        Err(error) => {
            return Ok(completed(
                command,
                Err(error.into()),
                runner.shutdown().await,
            ));
        }
    };
    let outcome = tokio::select! {
        biased;
        result = handle.wait() => result,
        signum = cancel.cancelled() => {
            handle.cancel(if signum == libc::SIGTERM { CancelReason::SigTerm } else { CancelReason::CtrlC });
            handle.wait().await
        }
    };
    let mut attempt = completed(command, outcome, runner.shutdown().await);
    if let Some(result) = attempt.result.clone() {
        let context = context.clone();
        let copied = copied.to_owned();
        let native_ok = attempt.native_ok;
        match tokio::task::spawn_blocking(move || {
            crate::test_evidence::analyze(&result, native_ok, &copied, reporter, &context)
        })
        .await
        {
            Ok(Ok(evidence)) => attempt.evidence = Some(evidence),
            _ => {
                attempt.exit_code = 125;
                attempt.native_ok = false;
                attempt
                    .diagnostics
                    .push_str("failforge: cannot produce native test evidence\n");
            }
        }
    }
    Ok(attempt)
}

#[cfg(test)]
#[path = "attempt_execution_tests.rs"]
mod tests;
