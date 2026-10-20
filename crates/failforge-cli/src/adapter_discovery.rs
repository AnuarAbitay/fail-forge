//! Private owner of adapter discovery, verified artifacts and source bindings.
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use failforge_dart::discovery::{DiscoveryConfig, DiscoveryError, DiscoveryRun, discover};
use failforge_dart::{MutationFamily, namespace::NamespaceError};
use failforge_runner::WorkspaceLease;
use failforge_workspace::{
    Cancellation, Hashes, PreparedProject, hash_file, inspect_project, parse_json, source_hashes,
    verify_source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::signal::unix::{SignalKind, signal};

use crate::dart_namespace::{absolute, invalid};
use crate::namespace_planner::{PlanProject, PlanRuntime, ReportDirectory};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    operation: String,
    project: PreparedProject,
    directory: PathBuf,
    runtime: Runtime,
    family: MutationFamily,
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Runtime {
    dart_sdk: PathBuf,
    adapter_root: PathBuf,
    // Retained in the legacy reproduction recipe, never executed by this owner.
    helper: PathBuf,
}

fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    decode_value(parse_json(raw)?)
}

pub(crate) fn decode_value(value: Value) -> Result<Request, NamespaceError> {
    let request: Request =
        serde_json::from_value(value).map_err(|_| invalid("invalid native discovery request"))?;
    if request.operation != "discover" || !request.payload.is_object() {
        return Err(invalid("invalid native discovery operation or payload"));
    }
    for path in [
        &request.project.root,
        &request.directory,
        &request.runtime.dart_sdk,
        &request.runtime.adapter_root,
        &request.runtime.helper,
    ]
    .into_iter()
    .chain(request.project.packages.values())
    {
        absolute(path)?;
    }
    if request.project.name.is_empty() || request.project.name.contains('\0') {
        return Err(invalid("invalid native discovery project"));
    }
    Ok(request)
}

fn payload_path(request: &Request, key: &str) -> Result<PathBuf, NamespaceError> {
    let path = PathBuf::from(
        request
            .payload
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing native discovery input path"))?,
    );
    absolute(&path)?;
    Ok(path)
}

struct InputBinding {
    project: PreparedProject,
    root: File,
    hashes: Hashes,
}

impl InputBinding {
    fn new(request: &Request, cancel: &Cancellation) -> Result<Self, NamespaceError> {
        let workspace = payload_path(request, "workspaceRoot")?;
        let package = request
            .payload
            .get("packageRoot")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing discovery package root"))?;
        if workspace.join(package).canonicalize()? != request.project.root
            || payload_path(request, "dartSdkPath")?.canonicalize()?
                != request.runtime.dart_sdk.canonicalize()?
        {
            return Err(invalid(
                "discovery payload does not identify the prepared inputs",
            ));
        }
        if request.project.flutter && request.payload.get("flutterSdkPath").is_none() {
            return Err(invalid("missing discovery Flutter SDK"));
        }
        if request.payload.get("flutterSdkPath").is_some() {
            payload_path(request, "flutterSdkPath")?.canonicalize()?;
        }
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&request.project.root)?;
        let binding = Self {
            project: request.project.clone(),
            root,
            hashes: source_hashes(&request.project.root, cancel)?,
        };
        binding.verify_with(cancel)?;
        Ok(binding)
    }

    fn verify(&self) -> Result<(), NamespaceError> {
        // A cancelled run still owes an uncancelled source-integrity check.
        self.verify_with(&Cancellation::default())
    }

    fn verify_with(&self, cancel: &Cancellation) -> Result<(), NamespaceError> {
        let owned = self.root.metadata()?;
        let named = std::fs::symlink_metadata(&self.project.root)?;
        if !named.is_dir()
            || owned.dev() != named.dev()
            || owned.ino() != named.ino()
            || inspect_project(&self.project.root, cancel)? != self.project
        {
            return Err(invalid("prepared discovery inputs changed"));
        }
        verify_source(&self.project.root, &self.hashes, cancel)?;
        let named = std::fs::symlink_metadata(&self.project.root)?;
        if !named.is_dir() || owned.dev() != named.dev() || owned.ino() != named.ino() {
            return Err(invalid("prepared discovery root changed"));
        }
        Ok(())
    }
}

struct Transcript {
    file: File,
    hash: Sha256,
}
impl Write for Transcript {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.file.write(bytes)?;
        self.hash.update(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

struct Prepared {
    request: Request,
    binding: InputBinding,
    directory: ReportDirectory,
    config: DiscoveryConfig,
    payload: File,
    payload_hash: String,
    transcript: Transcript,
}

/// Sealed discovery evidence and the retained artifacts and lease that
/// authorize its caller's publication. No private wire buffer is retained.
pub(crate) struct OwnedDiscovery {
    pub value: Value,
    _prepared: Prepared,
    _lease: WorkspaceLease,
}

fn prepare(request: Request, cancel: &Cancellation) -> Result<Prepared, NamespaceError> {
    let binding = InputBinding::new(&request, cancel)?;
    let config = DiscoveryConfig::new(
        request.runtime.dart_sdk.join("bin/dart").into_os_string(),
        request.runtime.adapter_root.clone(),
        request.payload.clone(),
        request.family,
    )
    .map_err(discovery_error)?;
    if config.package_root() != request.project.root {
        return Err(invalid(
            "discovery configuration identifies a different package",
        ));
    }
    let project = PlanProject {
        root: request.project.root.clone(),
        name: request.project.name.clone(),
        flutter: request.project.flutter,
    };
    let runtime = PlanRuntime {
        dart_sdk: request.runtime.dart_sdk.clone(),
        adapter_root: request.runtime.adapter_root.clone(),
        flutter_sdk: request
            .payload
            .get("flutterSdkPath")
            .map(|_| payload_path(&request, "flutterSdkPath"))
            .transpose()?,
    };
    let protected = crate::namespace_planner::protected_inputs(&project, &runtime)?;
    cancel.check()?;
    let directory = crate::attempt_artifacts::create(&request.directory, &protected)?;
    let config = config
        .with_capture_dir(directory.canonical_path()?.join("adapter-capture"))
        .map_err(discovery_error)?;
    let mut payload_bytes = serde_json::to_vec_pretty(&request.payload)
        .map_err(|_| invalid("invalid discovery payload"))?;
    payload_bytes.push(b'\n');
    let payload_hash = format!("{:x}", Sha256::digest(&payload_bytes));
    let payload = directory.write_private("payload.json", &payload_bytes)?;
    let transcript = Transcript {
        file: directory.write_private("adapter-session.jsonl", &[])?,
        hash: Sha256::new(),
    };
    Ok(Prepared {
        request,
        binding,
        directory,
        config,
        payload,
        payload_hash,
        transcript,
    })
}

fn discovery_error(error: DiscoveryError) -> NamespaceError {
    match error {
        DiscoveryError::Cancelled(signal) => NamespaceError::Cancelled(signal),
        error => invalid(&error.to_string()),
    }
}

fn verify_artifacts(prepared: &Prepared, run: &DiscoveryRun) -> Result<Vec<u8>, NamespaceError> {
    prepared.binding.verify()?;
    prepared.transcript.file.sync_all()?;
    prepared
        .directory
        .verify_file("payload.json", &prepared.payload)?;
    prepared
        .directory
        .verify_file("adapter-session.jsonl", &prepared.transcript.file)?;
    let root = prepared.directory.canonical_path()?;
    let cancel = Cancellation::default();
    if hash_file(&root.join("payload.json"), &cancel)? != prepared.payload_hash
        || hash_file(&root.join("adapter-session.jsonl"), &cancel)?
            != format!("{:x}", prepared.transcript.hash.clone().finalize())
    {
        return Err(invalid("discovery artifacts changed"));
    }
    let mut stderr = Vec::new();
    if let Some(result) = &run.command_result {
        crate::test_evidence::read_capture_limited(&result.stdout, false, 128 * 1024 * 1024)
            .map_err(|_| invalid("discovery stdout capture verification failed"))?;
        stderr = crate::test_evidence::read_capture_limited(&result.stderr, true, 8 * 1024 * 1024)
            .map_err(|_| invalid("discovery stderr capture verification failed"))?
            .bytes;
    } else if run.outcome.is_ok() {
        return Err(invalid("discovery command result is missing"));
    }
    prepared.directory.verify()?;
    Ok(stderr)
}

fn finish(
    prepared: Prepared,
    run: DiscoveryRun,
    started: Instant,
    lease: WorkspaceLease,
) -> Result<OwnedDiscovery, NamespaceError> {
    // Integrity failures take precedence over an otherwise clean cancellation.
    let verification = verify_artifacts(&prepared, &run);
    let stderr = verification
        .as_ref()
        .map_or_else(|_| Vec::new(), Clone::clone);
    let outcome = match verification {
        Ok(_) => run.outcome.map_err(discovery_error),
        Err(error) => Err(error),
    };
    let command = json!([
        prepared.request.runtime.helper,
        prepared.request.runtime.dart_sdk.join("bin/dart"),
        prepared.request.runtime.adapter_root,
        prepared.request.directory.join("payload.json")
    ]);
    let mut receipt = json!({"command":command,"exitCode":0,
        "wallSeconds":started.elapsed().as_secs_f64(),"cleanup":run.cleanup,
        "candidates":run.cleanup["validatedCandidates"],"freshStaticValidation":outcome.is_ok()});
    prepared
        .directory
        .write_private("adapter.stderr", &stderr)?;
    match outcome {
        Ok(result) => {
            let value = json!({"steps":result.steps,
                "candidates":result.candidates,"receipt":receipt});
            // Keep the standalone 64 MiB success preflight before publishing
            // result.json, including for callers that use no private transport.
            drop(crate::private_response::encode(value.clone())?);
            prepared.directory.write_json("result.json", &receipt)?;
            Ok(OwnedDiscovery {
                value,
                _prepared: prepared,
                _lease: lease,
            })
        }
        Err(error) => {
            receipt["exitCode"] = exit_code(&error).into();
            receipt["error"] = error.to_string().into();
            prepared.directory.write_json("result.json", &receipt)?;
            Err(error)
        }
    }
}

/// Discover directly under the caller's cancellation token. The adapter is
/// fully drained and uncancelled integrity checks finish before this returns.
pub(crate) async fn run_owned(
    request: Request,
    lease: WorkspaceLease,
    cancel: &Cancellation,
) -> Result<OwnedDiscovery, NamespaceError> {
    let started = Instant::now();
    let preparing = cancel.clone();
    let mut prepared = tokio::task::spawn_blocking(move || prepare(request, &preparing))
        .await
        .map_err(|_| invalid("discovery preparation failed"))??;
    let run = discover(&prepared.config, &mut prepared.transcript, cancel).await;
    tokio::task::spawn_blocking(move || finish(prepared, run, started, lease))
        .await
        .map_err(|_| invalid("discovery publication failed"))?
}

fn exit_code(error: &NamespaceError) -> u8 {
    match error {
        NamespaceError::Cancelled(libc::SIGTERM) => 143,
        NamespaceError::Cancelled(_) => 130,
        _ => 125,
    }
}

async fn process(
    request: Request,
    lease: WorkspaceLease,
    readiness: Option<crate::readiness::Readiness>,
) -> Result<(), NamespaceError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancel = Cancellation::default();
    let listening = cancel.clone();
    let listener = tokio::spawn(async move {
        listening.request(tokio::select! { _ = interrupt.recv() => libc::SIGINT,
        _ = terminate.recv() => libc::SIGTERM });
    });
    let result = async {
        if let Some(readiness) = readiness {
            readiness.acknowledge()?;
        }
        let owned = run_owned(request, lease, &cancel).await?;
        // Keep the lease and signal listener until artifacts and private output
        // have been committed. A later signal is handled by the controller.
        tokio::task::spawn_blocking(move || {
            let bytes = crate::private_response::encode(owned.value.clone())?;
            crate::private_response::publish(&bytes, &Cancellation::default())?;
            drop(owned);
            Ok::<_, NamespaceError>(())
        })
        .await
        .map_err(|_| invalid("discovery publication failed"))?
    }
    .await;
    listener.abort();
    let _ = listener.await;
    result
}

pub(crate) fn execute() -> ExitCode {
    let result = (|| {
        for key in [
            crate::workspace_handoff::HANDOFF_ENV,
            crate::workspace_handoff::CAPS_ENV,
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected discovery capability"));
            }
        }
        let fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing discovery request"))?;
        let request = decode(&crate::private_request::read(&fd)?)?;
        let lease =
            WorkspaceLease::from_env()?.ok_or_else(|| invalid("missing discovery lease"))?;
        let readiness = crate::readiness::Readiness::from_env()?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(process(request, lease, readiness))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(exit_code(&error))
        }
    }
}

#[cfg(test)]
#[path = "adapter_discovery_tests.rs"]
mod tests;
