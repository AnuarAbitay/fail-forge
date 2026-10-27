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

const PRIVATE_ENV: [&str; 11] = [
    "FAILFORGE_WORKSPACE_LEASE_FD",
    "FAILFORGE_ADAPTER_CANCEL_FD",
    "FAILFORGE_RUNNER_READY_FD",
    "FAILFORGE_TEST_EVIDENCE_FD",
    "FAILFORGE_TEST_CONTEXT_FD",
    "FAILFORGE_WORKSPACE_REQUEST_FD",
    "FAILFORGE_WORKSPACE_HANDOFF_FD",
    "FAILFORGE_WORKSPACE_CAPS_FDS",
    "FAILFORGE_REPORT_FDS",
    "FAILFORGE_CLI_CANCEL_PIPE",
    "FAILFORGE_ADAPTER_CANCEL_PIPE",
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
    pub exit_code: crate::ProcessCode,
    pub native_ok: bool,
    pub result: Option<CommandResult>,
    pub evidence: Option<AttemptEvidence>,
    /// Rejection before a process owner or any test evidence exists.
    pub preflight_rejection: Option<&'static str>,
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

fn dart_path_text(path: &Path) -> Result<String, NamespaceError> {
    path_text(path)?;
    Ok(failforge_workspace::dart_vm_argument(path)?
        .to_string_lossy()
        .into_owned())
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
        #[cfg(unix)]
        let prefix = vec![path_text(
            &sdk.join(failforge_config::platform::FLUTTER_BIN),
        )?];
        #[cfg(windows)]
        let prefix = vec![
            path_text(&config.dart_sdk.join(failforge_config::platform::DART_BIN))?,
            "--disable-dart-dev".into(),
            dart_path_text(&sdk.join("bin/cache/flutter_tools.snapshot"))?,
        ];
        prefix
            .into_iter()
            .chain(["--suppress-analytics", "test", "--no-pub", "--machine"].map(str::to_owned))
            .collect()
    } else {
        let test = project
            .packages
            .get("test")
            .ok_or_else(|| invalid("test execution requires the prepared test package"))?;
        vec![
            path_text(&config.dart_sdk.join(failforge_config::platform::DART_BIN))?,
            format!(
                "--packages={}",
                dart_path_text(&copied.join(".dart_tool/package_config.json"))?
            ),
            dart_path_text(&test.join("bin/test.dart"))?,
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
    let env = EnvSpec {
        remove: PRIVATE_ENV.into_iter().map(Into::into).collect(),
        ..EnvSpec::default()
    };
    #[cfg(windows)]
    let env = {
        let mut env = env;
        if project.flutter {
            // Flutter derives frontend-server SDK arguments from this value;
            // its URI conversion rejects verbatim paths with an appended '/'.
            env.set.insert(
                "FLUTTER_ROOT".into(),
                failforge_runner::windows_process_path(
                    config
                        .flutter_sdk
                        .as_ref()
                        .expect("Flutter SDK validated above"),
                )
                .into(),
            );
        }
        env
    };
    let spec = CommandSpec {
        program: child[0].clone().into(),
        args: child.into_iter().skip(1).map(Into::into).collect(),
        cwd: copied.to_owned(),
        env,
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
        preflight_rejection: None,
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
    let configuration = {
        let copied = copied.to_owned();
        let alias = context.alias.clone();
        tokio::task::spawn_blocking(move || {
            failforge_dart::validate_test_configuration(&copied, &alias)
        })
        .await
        .map_err(|_| invalid("test configuration validation failed"))?
    };
    // A cancellation received during the blocking read must win before the
    // child starts, including when validation reports an invalid configuration.
    cancel.check()?;
    if let Err(error) = configuration {
        return Ok(ExecutedAttempt {
            command,
            exit_code: 125,
            native_ok: false,
            result: None,
            evidence: None,
            preflight_rejection: Some("invalid_package_configuration"),
            diagnostics: format!("failforge: invalid test package configuration: {error}\n"),
        });
    }
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

#[cfg(all(test, unix))]
#[path = "attempt_execution_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    #[test]
    fn flutter_launches_native_dart_snapshot_with_literal_arguments() {
        let root = PathBuf::from(r"\\?\C:\native fixtures\#topic%");
        let config = ExecutionConfig {
            runner: root.join("failforge.exe"),
            dart_sdk: root.join("flutter/bin/cache/dart-sdk"),
            flutter_sdk: Some(root.join("flutter")),
            command_timeout: "30s".into(),
            test_timeout: "1s".into(),
        };
        let project = PreparedProject {
            root: root.join("subject"),
            name: "subject".into(),
            packages: BTreeMap::new(),
            graph: serde_json::Value::Null,
            flutter: true,
            framework_collision: false,
        };
        let (spec, _, reporter) =
            specification(&config, &project, &root.join("copy"), &root.join("capture")).unwrap();
        assert_eq!(reporter, TestReporter::Flutter);
        assert_eq!(
            spec.program,
            config
                .dart_sdk
                .join(failforge_config::platform::DART_BIN)
                .as_os_str()
        );
        assert_eq!(spec.args[0], "--disable-dart-dev");
        assert_eq!(
            spec.args[1],
            "file:///C:/native%20fixtures/%23topic%25/flutter/bin/cache/flutter_tools.snapshot"
        );
        assert_eq!(spec.args[2], "--suppress-analytics");
        assert!(
            spec.args
                .windows(3)
                .any(|args| args == ["test", "--no-pub", "--machine"])
        );
        assert_eq!(
            spec.env.set.get(std::ffi::OsStr::new("FLUTTER_ROOT")),
            Some(&root.join("flutter").into_os_string())
        );
        for name in PRIVATE_ENV {
            assert!(spec.env.remove.contains(std::ffi::OsStr::new(name)));
        }
    }

    #[test]
    fn flutter_root_uses_verified_runtime_spelling_while_config_stays_canonical() {
        let root = std::env::temp_dir().join(format!(
            "ff-flutter-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let sdk = root.join("flutter SDK #%");
        std::fs::create_dir(&sdk).unwrap();
        let sdk = sdk.canonicalize().unwrap();
        assert!(matches!(
            sdk.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
        ));
        let wide: Vec<_> = sdk.as_os_str().encode_wide().collect();
        let ordinary = std::ffi::OsString::from_wide(&wide[4..]);
        let config = ExecutionConfig {
            runner: root.join("failforge.exe"),
            dart_sdk: sdk.join("bin/cache/dart-sdk"),
            flutter_sdk: Some(sdk.clone()),
            command_timeout: "60s".into(),
            test_timeout: "10s".into(),
        };
        let project = PreparedProject {
            root: root.join("subject"),
            name: "subject".into(),
            packages: BTreeMap::new(),
            graph: serde_json::Value::Null,
            flutter: true,
            framework_collision: false,
        };
        let copied = root.join("copy");
        let (spec, _, reporter) =
            specification(&config, &project, &copied, &root.join("capture")).unwrap();
        assert_eq!(reporter, TestReporter::Flutter);
        assert_eq!(
            spec.env.set.get(std::ffi::OsStr::new("FLUTTER_ROOT")),
            Some(&ordinary)
        );
        assert_eq!(std::fs::canonicalize(&ordinary).unwrap(), sdk);
        assert_eq!(config.flutter_sdk.as_ref(), Some(&sdk));
        assert_eq!(config.dart_sdk, sdk.join("bin/cache/dart-sdk"));
        assert_eq!(spec.cwd, copied);
        assert_eq!(spec.timeout_policy.timeout, Some(Duration::from_secs(60)));
        assert_eq!(
            spec.program,
            config.dart_sdk.join("bin/dart.exe").as_os_str()
        );
    }

    #[test]
    fn dart_script_and_packages_are_uris_while_program_and_cwd_stay_native() {
        let root = PathBuf::from(r"\\?\C:\native fixtures\#topic%");
        let config = ExecutionConfig {
            runner: root.join("failforge.exe"),
            dart_sdk: root.join("dart-sdk"),
            flutter_sdk: None,
            command_timeout: "30s".into(),
            test_timeout: "1s".into(),
        };
        let project = PreparedProject {
            root: root.join("subject"),
            name: "subject".into(),
            packages: BTreeMap::from([("test".into(), root.join("test package"))]),
            graph: serde_json::Value::Null,
            flutter: false,
            framework_collision: false,
        };
        let copied = root.join("copy");
        let (spec, _, reporter) =
            specification(&config, &project, &copied, &root.join("capture")).unwrap();
        assert_eq!(reporter, TestReporter::Dart);
        assert_eq!(spec.program, root.join("dart-sdk/bin/dart.exe").as_os_str());
        assert_eq!(spec.cwd, copied);
        assert_eq!(
            spec.args[0],
            "--packages=file:///C:/native%20fixtures/%23topic%25/copy/.dart_tool/package_config.json"
        );
        assert_eq!(
            spec.args[1],
            "file:///C:/native%20fixtures/%23topic%25/test%20package/bin/test.dart"
        );
    }
}
