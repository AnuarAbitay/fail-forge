//! Native owner for the read-only Dart analyzer namespace plan.
use std::collections::BTreeMap;
use std::ffi::OsStr;
#[cfg(all(test, unix))]
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use failforge_dart::namespace::NamespaceError;
use failforge_runner::{
    CancelReason, CapturePolicy, CommandResult, CommandSpec, CompletionCause, EnvSpec, Runner,
    TimeoutPolicy, WorkspaceLease,
};
use failforge_workspace::{Cancellation, hash_file, parse_json, source_hashes, verify_source};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::dart_namespace::invalid;

const MAX_PLAN: u64 = 8 * 1024 * 1024;
const MAX_FILES: usize = 2000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanProject {
    pub root: PathBuf,
    pub name: String,
    pub flutter: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PlanRuntime {
    pub dart_sdk: PathBuf,
    pub flutter_sdk: Option<PathBuf>,
    pub adapter_root: PathBuf,
}

#[cfg(unix)]
#[path = "namespace_planner_report_unix.rs"]
mod report;
#[cfg(windows)]
#[path = "namespace_planner_report_windows.rs"]
mod report;
#[cfg(windows)]
pub(crate) use report::protected_overlap;
pub(crate) use report::{ReportDirectory, create_report_directory};

fn report_directory(
    project: &PlanProject,
    directory: &Path,
    runtime: &PlanRuntime,
) -> Result<ReportDirectory, NamespaceError> {
    let protected = protected_inputs(project, runtime)?;
    create_report_directory(directory, &protected)
}

pub(crate) fn protected_inputs(
    project: &PlanProject,
    runtime: &PlanRuntime,
) -> Result<Vec<PathBuf>, NamespaceError> {
    let mut protected = vec![
        project.root.canonicalize()?,
        runtime.adapter_root.canonicalize()?,
        runtime.dart_sdk.canonicalize()?,
    ];
    if let Some(flutter) = &runtime.flutter_sdk {
        protected.push(flutter.canonicalize()?);
    }
    protected.extend(
        failforge_workspace::dependency_roots(
            &project.root.join(".dart_tool/package_config.json"),
        )?
        .into_values(),
    );
    Ok(protected)
}

fn command(
    project: &PlanProject,
    directory: &ReportDirectory,
    runtime: &PlanRuntime,
    files: &[String],
) -> Result<(CommandSpec, Value), NamespaceError> {
    let tool = runtime
        .adapter_root
        .join("bin/failforge_dart_namespace.dart");
    if !tool.is_file() {
        return Err(invalid(
            "missing Dart namespace planner in the selected adapter",
        ));
    }
    let mut payload = json!({"workspaceRoot":project.root,"packageRoot":".","dartSdkPath":runtime.dart_sdk,
        "packageName":project.name,"alias":"failforge_subject","files":files});
    if project.flutter {
        if let Some(flutter) = &runtime.flutter_sdk {
            payload["flutterSdkPath"] = json!(flutter);
        }
    }
    directory.write_json("payload.json", &payload)?;
    let canonical = directory.canonical_path()?;
    let program = runtime.dart_sdk.join(failforge_config::platform::DART_BIN);
    let args = vec![
        format!(
            "--packages={}",
            runtime
                .adapter_root
                .join(".dart_tool/package_config.json")
                .display()
        ),
        tool.to_string_lossy().into_owned(),
        canonical
            .join("payload.json")
            .to_string_lossy()
            .into_owned(),
    ];
    let receipt_command = json!(
        std::iter::once(program.to_string_lossy().into_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
    );
    let mut env = EnvSpec::default();
    for key in [
        "FAILFORGE_WORKSPACE_LEASE_FD",
        "FAILFORGE_ADAPTER_CANCEL_FD",
        "FAILFORGE_ADAPTER_CANCEL_PIPE",
        "FAILFORGE_CLI_CANCEL_PIPE",
        "FAILFORGE_RUNNER_READY_FD",
        "FAILFORGE_TEST_EVIDENCE_FD",
        "FAILFORGE_TEST_CONTEXT_FD",
        "FAILFORGE_WORKSPACE_REQUEST_FD",
        "FAILFORGE_WORKSPACE_HANDOFF_FD",
        "FAILFORGE_WORKSPACE_CAPS_FDS",
        "FAILFORGE_REPORT_FDS",
    ] {
        env.remove.insert(key.into());
    }
    Ok((
        CommandSpec {
            program: program.into_os_string(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: runtime.adapter_root.clone(),
            env,
            timeout_policy: TimeoutPolicy {
                timeout: Some(Duration::from_secs(60)),
                ..Default::default()
            },
            capture_policy: CapturePolicy {
                capture_dir: Some(canonical.join("capture")),
                ..Default::default()
            },
        },
        receipt_command,
    ))
}

async fn run(spec: CommandSpec, cancel: &Cancellation) -> Result<CommandResult, NamespaceError> {
    cancel.check()?;
    let runner = Runner::new();
    let mut handle = match runner.start(spec) {
        Ok(handle) => handle,
        Err(_) => {
            let _ = runner.shutdown().await;
            return Err(invalid("namespace analyzer could not start"));
        }
    };
    let result = tokio::select! {
        biased;
        result = handle.wait() => result,
        signum = cancel.cancelled() => {
            handle.cancel(if signum == libc::SIGTERM { CancelReason::SigTerm } else { CancelReason::CtrlC });
            handle.wait().await
        }
    };
    let shutdown = runner.shutdown().await;
    if shutdown.is_err() {
        return Err(invalid("namespace analyzer native cleanup failed"));
    }
    result
        .map(|value| (*value).clone())
        .map_err(|_| invalid("namespace analyzer native owner failed"))
}

fn runner_receipt(result: &CommandResult) -> Value {
    // Format from the typed parent-owned result. This never parses child text.
    let text = crate::summary(result);
    let fields: BTreeMap<_, _> = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|field| field.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    json!(fields)
}

fn validate_plan(
    raw: &[u8],
    project: &PlanProject,
    config_hash: &str,
) -> Result<Value, NamespaceError> {
    let plan = parse_json(raw)?;
    if plan.as_object().is_none()
        || plan["version"].as_u64() != Some(1)
        || plan["kind"] != "dart_namespace_plan"
        || plan["packageName"] != project.name
        || plan["alias"] != "failforge_subject"
        || plan["packageConfigHash"] != config_hash
    {
        return Err(invalid("namespace plan identifies different inputs"));
    }
    Ok(plan)
}

fn completed_plan(
    result: &CommandResult,
    project: &PlanProject,
    cancel: &Cancellation,
) -> Result<(Value, Vec<u8>), NamespaceError> {
    // Integrity takes precedence over cancellation and before a partial JSON
    // response could be treated as a source-bound plan.
    if !result.cleanup.is_complete()
        || result.stdout.issue.is_some()
        || result.stderr.issue.is_some()
    {
        return Err(invalid("namespace analyzer cleanup or capture failed"));
    }
    if result.stdout.bytes_read > MAX_PLAN {
        return Err(invalid("namespace plan exceeds its byte limit"));
    }
    let stdout = crate::test_evidence::read_capture_limited(&result.stdout, true, MAX_PLAN)
        .map_err(|_| invalid("namespace analyzer raw capture is incomplete"))?;
    crate::test_evidence::read_capture_limited(&result.stderr, false, 256 * 1024 * 1024)
        .map_err(|_| invalid("namespace analyzer raw capture is incomplete"))?;
    if let CompletionCause::Cancelled(reason) = result.cause {
        let signal = match reason {
            CancelReason::CtrlC => Some(libc::SIGINT),
            CancelReason::SigTerm => Some(libc::SIGTERM),
            _ => None,
        };
        if signal == cancel.signum() && signal.is_some() {
            cancel.check()?;
        }
    }
    if result.cause != CompletionCause::ProcessCompleted
        || !result.status.is_some_and(|status| status.success())
    {
        return Err(invalid(
            "namespace analyzer failed; see namespace-plan capture and result.json",
        ));
    }
    cancel.check()?;
    let config_hash = hash_file(
        &project.root.join(".dart_tool/package_config.json"),
        &Cancellation::default(),
    )?;
    let plan = validate_plan(&stdout.bytes, project, &config_hash)?;
    Ok((plan, stdout.bytes))
}

pub(crate) async fn prepare(
    project: &PlanProject,
    directory: &Path,
    runtime: &PlanRuntime,
    lease: Option<&WorkspaceLease>,
    cancel: &Cancellation,
) -> Result<Value, NamespaceError> {
    cancel.check()?;
    if !runtime
        .adapter_root
        .join("bin/failforge_dart_namespace.dart")
        .is_file()
    {
        return Err(invalid(
            "missing Dart namespace planner in the selected adapter",
        ));
    }
    let before = source_hashes(&project.root, cancel)?;
    let files = before
        .keys()
        .filter(|name| {
            name.starts_with("test")
                && name
                    .extension()
                    .is_some_and(|extension| extension == "dart")
        })
        .filter_map(|name| {
            name.components()
                .map(|part| part.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
        })
        .map(|parts| parts.join("/"))
        .collect::<Vec<_>>();
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(invalid(
            "namespace planning requires 1..2000 author Dart files",
        ));
    }
    let report = report_directory(project, directory, runtime)?;
    let mut receipt = json!({"exitCode":125,"complete":false,"sourceInputsUnchanged":false});
    let mut published_plan = None;
    let started = Instant::now();
    let outcome = async {
        let (mut spec, argv) = command(project, &report, runtime, &files)?;
        receipt["command"] = argv;
        if let Some(lease) = lease {
            lease.protect_child(&mut spec);
        }
        report.verify()?;
        let result = run(spec, cancel).await;
        // Always verify with an independent token, including owner startup and
        // cancellation errors, so failed cleanup cannot conceal changed inputs.
        verify_source(&project.root, &before, &Cancellation::default())?;
        report.verify()?;
        receipt["sourceInputsUnchanged"] = true.into();
        let result = result?;
        receipt["exitCode"] = crate::result_exit_code(&result).into();
        receipt["runner"] = runner_receipt(&result);
        report.write_private("runner.stdout", b"")?;
        report.write_private("runner.stderr", crate::summary(&result).as_bytes())?;
        let (plan, raw) = completed_plan(&result, project, cancel)?;
        cancel.check()?;
        published_plan = Some(report.write_private("plan.json", &raw)?);
        receipt["complete"] = true.into();
        receipt["planSha256"] = format!("{:x}", Sha256::digest(&raw)).into();
        receipt["files"] = files.len().into();
        receipt["planFile"] = json!(report.canonical_path()?.join("plan.json"));
        Ok::<_, NamespaceError>(plan)
    }
    .await;
    receipt["wallSeconds"] = started.elapsed().as_secs_f64().into();
    if let Err(error) = &outcome {
        receipt["error"] = error.to_string().into();
    }
    if let Err(error) = report.write_json("result.json", &receipt) {
        if let Some(file) = published_plan {
            let _ = report.remove_owned(OsStr::new("plan.json"), &file);
        }
        return Err(error);
    }
    outcome.map(|plan| json!({"plan":plan,"receipt":receipt}))
}

#[cfg(all(test, unix))]
#[path = "namespace_planner_tests.rs"]
mod tests;
