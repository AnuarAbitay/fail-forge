//! Native owner for the read-only Dart analyzer namespace plan.
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
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

fn same_directory(left: &Metadata, right: &Metadata) -> bool {
    left.is_dir() && right.is_dir() && left.dev() == right.dev() && left.ino() == right.ino()
}

#[allow(clippy::unnecessary_cast)] // dev_t is signed on macOS and u64 on Linux.
fn same_identity(named: &libc::stat, owned: &Metadata) -> bool {
    named.st_dev as u64 == owned.dev() && named.st_ino == owned.ino()
}

fn component(value: &OsStr) -> Result<CString, NamespaceError> {
    CString::new(value.as_bytes()).map_err(|_| invalid("invalid namespace artifact name"))
}

fn stat_at(parent: &File, name: &OsStr) -> Result<libc::stat, NamespaceError> {
    let name = component(name)?;
    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: parent owns a live descriptor, name is terminated and info is
    // writable native storage; the final component is never followed.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful fstatat initialized info.
    Ok(unsafe { info.assume_init() })
}

/// A report pathname is checked for visibility, but it never authorizes a
/// write. All private artifacts are created relative to the retained directory
/// descriptor, including failure receipts and partial-publication cleanup.
pub(crate) struct ReportDirectory {
    path: PathBuf,
    parent_path: PathBuf,
    parent: File,
    name: OsString,
    file: File,
}

impl ReportDirectory {
    pub(crate) fn verify_file(&self, name: &str, file: &File) -> Result<(), NamespaceError> {
        if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
            return Err(invalid("invalid namespace artifact name"));
        }
        self.verify()?;
        let named = stat_at(&self.file, OsStr::new(name))?;
        let owned = file.metadata()?;
        if !owned.is_file()
            || named.st_mode & libc::S_IFMT != libc::S_IFREG
            || !same_identity(&named, &owned)
        {
            return Err(invalid("namespace report artifact changed"));
        }
        Ok(())
    }

    pub(crate) fn canonical_path(&self) -> Result<PathBuf, NamespaceError> {
        self.verify()?;
        Ok(self.parent_path.join(&self.name))
    }

    pub(crate) fn verify(&self) -> Result<(), NamespaceError> {
        let parent = self.parent.metadata()?;
        let owned = self.file.metadata()?;
        let named = stat_at(&self.parent, &self.name)?;
        if !same_directory(&parent, &fs::symlink_metadata(&self.parent_path)?)
            || !same_directory(&owned, &fs::symlink_metadata(&self.path)?)
            || named.st_mode & libc::S_IFMT != libc::S_IFDIR
            || !same_identity(&named, &owned)
        {
            return Err(invalid("namespace report directory changed"));
        }
        Ok(())
    }

    fn remove_owned(&self, name: &OsStr, file: &File) -> Result<(), NamespaceError> {
        let named = match stat_at(&self.file, name) {
            Err(NamespaceError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            result => result?,
        };
        let owned = file.metadata()?;
        if named.st_mode & libc::S_IFMT != libc::S_IFREG || !same_identity(&named, &owned) {
            return Err(invalid("namespace report artifact changed"));
        }
        let name = component(name)?;
        // SAFETY: a retained directory and validated single filename are used;
        // only the inode created by this owner is eligible for cleanup.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub(crate) fn write_private(&self, name: &str, raw: &[u8]) -> Result<File, NamespaceError> {
        if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
            return Err(invalid("invalid namespace artifact name"));
        }
        self.verify()?;
        let name = OsStr::new(name);
        let terminated = component(name)?;
        // SAFETY: the fresh O_EXCL descriptor is uniquely owned below. Artifact
        // creation remains anchored even if an ancestor is renamed concurrently.
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                terminated.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful openat created this unique live descriptor.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let result = (|| {
            file.write_all(raw)?;
            file.sync_all()?;
            self.verify()?;
            let named = stat_at(&self.file, name)?;
            let owned = file.metadata()?;
            if !same_identity(&named, &owned) || named.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(invalid("namespace report artifact changed"));
            }
            Ok::<_, NamespaceError>(())
        })();
        if let Err(error) = result {
            let _ = self.remove_owned(name, &file);
            return Err(error);
        }
        Ok(file)
    }

    pub(crate) fn write_json(&self, name: &str, value: &Value) -> Result<File, NamespaceError> {
        let mut raw =
            serde_json::to_vec_pretty(value).map_err(|_| invalid("invalid namespace receipt"))?;
        raw.push(b'\n');
        self.write_private(name, &raw)
    }
}

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

pub(crate) fn create_report_directory(
    directory: &Path,
    protected: &[PathBuf],
) -> Result<ReportDirectory, NamespaceError> {
    // Campaign outputs already have an existing parent. Anchor validation there
    // so an alias cannot place analyzer artifacts inside an input tree.
    let parent = directory
        .parent()
        .ok_or_else(|| invalid("invalid namespace report directory"))?
        .canonicalize()?;
    let name = directory
        .file_name()
        .ok_or_else(|| invalid("invalid namespace report directory"))?
        .to_owned();
    let output = parent.join(&name);
    let parent_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&parent)?;
    if protected
        .iter()
        .any(|root| output.starts_with(root) || root.starts_with(&output))
    {
        return Err(invalid(
            "namespace report directory overlaps protected inputs",
        ));
    }
    if !same_directory(&parent_file.metadata()?, &fs::symlink_metadata(&parent)?)
        || directory
            .parent()
            .ok_or_else(|| invalid("invalid namespace report directory"))?
            .canonicalize()?
            != parent
    {
        return Err(invalid("namespace report parent changed"));
    }
    let terminated = component(&name)?;
    // SAFETY: the validated single name is created beneath the retained parent.
    if unsafe { libc::mkdirat(parent_file.as_raw_fd(), terminated.as_ptr(), 0o700) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returns a unique descriptor and never follows a replacement
    // final-component symlink.
    let descriptor = unsafe {
        libc::openat(
            parent_file.as_raw_fd(),
            terminated.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let report = ReportDirectory {
        path: directory.into(),
        parent_path: parent,
        parent: parent_file,
        name,
        // SAFETY: this successful descriptor has exactly one owner.
        file: unsafe { File::from_raw_fd(descriptor) },
    };
    report.verify()?;
    Ok(report)
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
    let program = runtime.dart_sdk.join("bin/dart");
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
        .filter_map(|name| name.to_str())
        .filter(|name| name.starts_with("test/") && name.ends_with(".dart"))
        .map(str::to_owned)
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

#[cfg(test)]
#[path = "namespace_planner_tests.rs"]
mod tests;
