//! Private owner of a complete fresh-copy test attempt and its terminal receipt.
#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(unix)]
use crate::signals::{SignalKind, signal};
use failforge_dart::{
    Mutant, SelectionContext,
    attempt_workspace::AttemptWorkspace,
    namespace::{NamespaceError, isolate_subject},
};
#[cfg(any(unix, test))]
use failforge_workspace::parse_json;
use failforge_workspace::{Cancellation, PreparedProject, lifecycle::LifecycleCaps};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::attempt_execution::{ExecutedAttempt, ExecutionConfig};
use crate::dart_namespace::{absolute, invalid};
use crate::namespace_planner::{PlanProject, PlanRuntime, ReportDirectory};
use crate::retained_replay::LoadedReplay;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub operation: String,
    pub project: PreparedProject,
    pub case: PathBuf,
    pub runtime: Runtime,
    pub config: Config,
    pub workspace: Workspace,
    pub context: SelectionContext,
    pub patch_sha256: Option<String>,
    pub namespace_plan: Option<NamespaceCache>,
    #[serde(default)]
    pub framework_root: Option<PathBuf>,
    #[serde(skip)]
    pub replay: Option<Arc<LoadedReplay>>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Runtime {
    pub runner: PathBuf,
    pub dart_sdk: PathBuf,
    pub flutter_sdk: Option<PathBuf>,
    pub adapter_root: PathBuf,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Config {
    pub command_timeout: String,
    pub test_timeout: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workspace {
    pub root: PathBuf,
    pub campaign: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NamespaceCache {
    pub plan: Value,
    pub receipt: Value,
}

fn hex(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(any(unix, test))]
fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    decode_value(parse_json(raw)?)
}

pub(crate) fn decode_value(value: Value) -> Result<Request, NamespaceError> {
    let request: Request =
        serde_json::from_value(value).map_err(|_| invalid("invalid native attempt request"))?;
    validate_request(&request)?;
    Ok(request)
}

fn validate_request(request: &Request) -> Result<(), NamespaceError> {
    if request.operation != "attempt" {
        return Err(invalid("unknown native attempt operation"));
    }
    for path in [
        &request.project.root,
        &request.case,
        &request.runtime.runner,
        &request.runtime.dart_sdk,
        &request.runtime.adapter_root,
        &request.workspace.root,
    ] {
        absolute(path)?;
    }
    for path in request
        .runtime
        .flutter_sdk
        .iter()
        .chain(request.framework_root.iter())
        .chain(request.project.packages.values())
    {
        absolute(path)?;
    }
    let campaign = request
        .workspace
        .campaign
        .strip_prefix("campaign-")
        .ok_or_else(|| invalid("invalid attempt campaign"))?;
    if campaign.len() != 32 || !hex(campaign) {
        return Err(invalid("invalid attempt campaign"));
    }
    let alias = if request.project.framework_collision || request.framework_root.is_some() {
        "failforge_subject"
    } else {
        &request.project.name
    };
    if request.project.name.is_empty()
        || request.project.name.contains('\0')
        || request.context.alias != alias
        || (request.project.flutter && request.runtime.flutter_sdk.is_none())
    {
        return Err(invalid("invalid prepared attempt binding"));
    }
    failforge_dart::validate_selection_context(&request.context)
        .map_err(|_| invalid("invalid attempt selection context"))?;
    failforge_config::duration::resolve_timeout_policy(
        &json!(request.config.command_timeout),
        &json!(request.config.test_timeout),
    )
    .map_err(|_| invalid("invalid attempt timeout policy"))?;
    if let Some(digest) = &request.patch_sha256 {
        if digest.len() != 64 || !hex(digest) {
            return Err(invalid("invalid attempt patch digest"));
        }
        let mutant: Mutant = serde_json::from_value(
            request
                .context
                .mutant
                .clone()
                .ok_or_else(|| invalid("missing attempt mutant"))?,
        )
        .map_err(|_| invalid("invalid attempt mutant"))?;
        mutant
            .validate_model()
            .map_err(|_| invalid("invalid attempt mutant"))?;
    }
    if let Some(role) = &request.context.initialization {
        use failforge_dart::InitializationRequest;
        let patch = matches!(
            role,
            InitializationRequest::Initial { .. } | InitializationRequest::MutantReplay { .. }
        );
        if patch != request.patch_sha256.is_some() {
            return Err(invalid("initialization role does not match attempt patch"));
        }
    }
    if let Some(cache) = &request.namespace_plan {
        if !(request.project.framework_collision || request.framework_root.is_some())
            || !cache.plan.is_object()
            || cache.plan["kind"] != "dart_namespace_plan"
            || cache.plan["version"] != 1
            || cache.plan["packageName"] != request.project.name
            || cache.plan["alias"] != "failforge_subject"
            || cache.receipt["complete"] != true
            || cache.receipt["sourceInputsUnchanged"] != true
        {
            return Err(invalid("invalid cached attempt namespace plan"));
        }
    }
    Ok(())
}

type OwnedWorkspace = Arc<Mutex<AttemptWorkspace>>;
async fn step<T: Send + 'static>(
    workspace: &OwnedWorkspace,
    operation: impl FnOnce(&mut AttemptWorkspace) -> Result<T, NamespaceError> + Send + 'static,
) -> Result<T, NamespaceError> {
    let workspace = Arc::clone(workspace);
    tokio::task::spawn_blocking(move || {
        let mut workspace = workspace
            .lock()
            .map_err(|_| invalid("attempt workspace owner failed"))?;
        operation(&mut workspace)
    })
    .await
    .map_err(|_| invalid("attempt workspace operation failed"))?
}

struct Receipt {
    directory: Arc<ReportDirectory>,
    attempt: Value,
    catalogue: Value,
    initialization: Value,
    namespace_plan: Value,
}

/// The initial attempt belongs to the final mutation record. Controls and the
/// standalone command publish their own terminal attempt receipts immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Publication {
    Attempt,
    Deferred,
}

/// A sealed attempt and the capability retained through its caller's terminal
/// publication. The workspace has already been cleaned before this is returned.
pub(crate) struct OwnedAttempt {
    pub directory: Arc<ReportDirectory>,
    pub attempt: Value,
    pub catalogue: Value,
    pub initialization: Value,
    pub namespace_plan: Value,
    // Retaining this guard keeps the lease alive through caller publication.
    _workspace: OwnedWorkspace,
    #[cfg(unix)]
    private_response: Option<Vec<u8>>,
}
fn infra(reason: &str) -> Value {
    json!({"outcome":"INFRA_ERROR","reason":reason,"reporterComplete":false,"runner":{},"loadFailures":[],
        "tests":{"started":0,"passed":0,"failed":0,"skipped":0,"hidden":0,"loadingPassed":0,"loadingFailed":0}})
}
fn override_infra(receipt: &mut Receipt, reason: &str) {
    receipt.attempt["outcome"] = "INFRA_ERROR".into();
    receipt.attempt["reason"] = reason.into();
    revoke_private_evidence(receipt);
}
fn revoke_private_evidence(receipt: &mut Receipt) {
    receipt.initialization = Value::Null;
    receipt.catalogue = Value::Null;
    receipt.namespace_plan = Value::Null;
}

fn execution_receipt(
    directory: Arc<ReportDirectory>,
    executed: &mut ExecutedAttempt,
    namespace_plan: Value,
) -> Result<Receipt, NamespaceError> {
    let (mut attempt, catalogue, initialization, stdout_hash) = match executed.evidence.take() {
        Some(evidence) => (
            evidence.classification,
            serde_json::to_value(evidence.catalogue)
                .map_err(|_| invalid("invalid native catalogue"))?,
            serde_json::to_value(evidence.initialization)
                .map_err(|_| invalid("invalid native initialization"))?,
            evidence.stdout_hash,
        ),
        None => (
            infra(
                executed
                    .preflight_rejection
                    .unwrap_or("native_owner_failed"),
            ),
            Value::Null,
            Value::Null,
            format!("{:x}", Sha256::digest(b"")),
        ),
    };
    attempt["command"] = json!(executed.command);
    attempt["exitCode"] = executed.exit_code.into();
    attempt["compileChecked"] = executed.preflight_rejection.is_none().into();
    attempt["stdoutSha256"] = stdout_hash.into();
    if executed.preflight_rejection.is_some() {
        attempt["phase"] = "preflight".into();
    }
    let mut receipt = Receipt {
        directory,
        attempt,
        catalogue,
        initialization,
        namespace_plan,
    };
    if executed.preflight_rejection.is_some() {
        revoke_private_evidence(&mut receipt);
    }
    Ok(receipt)
}

async fn run_attempt(
    request: &Request,
    workspace: &OwnedWorkspace,
    cancel: &Cancellation,
) -> Result<Receipt, NamespaceError> {
    let project = PlanProject {
        root: request.project.root.clone(),
        name: request.project.name.clone(),
        flutter: request.project.flutter,
    };
    let runtime = PlanRuntime {
        dart_sdk: request.runtime.dart_sdk.clone(),
        flutter_sdk: request.runtime.flutter_sdk.clone(),
        adapter_root: request.runtime.adapter_root.clone(),
    };
    let mut protected = crate::namespace_planner::protected_inputs(&project, &runtime)?;
    if let Some(framework) = &request.framework_root {
        protected.push(framework.canonicalize()?);
    }
    let inputs = protected.clone();
    step(workspace, move |workspace| {
        workspace.protect_inputs(&inputs)
    })
    .await?;
    protected.push(request.workspace.root.canonicalize()?);
    let case = request.case.clone();
    let directory = Arc::new(
        tokio::task::spawn_blocking(move || crate::attempt_artifacts::create(&case, &protected))
            .await
            .map_err(|_| invalid("attempt report creation failed"))??,
    );
    let expected = request.project.clone();
    let copying = cancel.clone();
    step(workspace, move |workspace| {
        workspace.copy(&expected, &copying)
    })
    .await?;
    let copied = step(workspace, |workspace| Ok(workspace.copy_path().to_owned())).await?;
    let mut cache = request.namespace_plan.clone();
    let isolation = if request.project.framework_collision || request.framework_root.is_some() {
        let isolate = |cache: Option<&NamespaceCache>| {
            let source = request.project.root.clone();
            let framework = request
                .framework_root
                .clone()
                .unwrap_or_else(|| source.clone());
            let copied = copied.clone();
            let plan = cache.map(|cache| cache.plan.clone());
            let cancel = cancel.clone();
            tokio::task::spawn_blocking(move || {
                isolate_subject(
                    &source,
                    &copied,
                    &framework,
                    "failforge_subject",
                    plan.as_ref(),
                    &cancel,
                )
            })
        };
        let first = isolate(cache.as_ref())
            .await
            .map_err(|_| invalid("attempt isolation failed"))?;
        let mut value = match first {
            Err(NamespaceError::PlanRequired) if cache.is_none() => {
                // A rejected or cancelled attempt cannot leave a shared plan
                // artifact name occupied for the next fresh preparation.
                let path = directory.canonical_path()?.join("namespace-plan");
                let planned =
                    crate::namespace_planner::prepare(&project, &path, &runtime, None, cancel)
                        .await?;
                cache = Some(
                    serde_json::from_value(planned)
                        .map_err(|_| invalid("invalid namespace plan receipt"))?,
                );
                isolate(cache.as_ref())
                    .await
                    .map_err(|_| invalid("attempt isolation failed"))??
            }
            other => other?,
        };
        if let Some(cache) = &cache {
            value["planner"] = cache.receipt.clone();
        }
        value
    } else {
        Value::Null
    };
    if let Some(digest) = &request.patch_sha256 {
        let mutant: Mutant = serde_json::from_value(request.context.mutant.clone().unwrap())
            .map_err(|_| invalid("invalid attempt mutant"))?;
        let digest = digest.clone();
        let applying = cancel.clone();
        step(workspace, move |workspace| {
            workspace.apply_patch(&mutant, &digest, &applying)
        })
        .await?;
    }
    let sealing = cancel.clone();
    let hashes = step(workspace, move |workspace| workspace.seal(&sealing)).await?;
    directory.verify()?;
    let config = ExecutionConfig {
        runner: request.runtime.runner.clone(),
        dart_sdk: request.runtime.dart_sdk.clone(),
        flutter_sdk: request.runtime.flutter_sdk.clone(),
        command_timeout: request.config.command_timeout.clone(),
        test_timeout: request.config.test_timeout.clone(),
    };
    let mut executed = crate::attempt_execution::execute(
        &config,
        &request.project,
        &copied,
        &directory.canonical_path()?.join("capture"),
        &request.context,
        cancel,
    )
    .await?;
    directory.verify()?;
    let mut receipt = execution_receipt(
        Arc::clone(&directory),
        &mut executed,
        cache
            .map(|cache| json!({"plan":cache.plan,"receipt":cache.receipt}))
            .unwrap_or(Value::Null),
    )?;
    if step(workspace, |workspace| workspace.verify_execution_inputs())
        .await
        .is_err()
    {
        override_infra(&mut receipt, "execution_input_changed");
    }
    receipt.attempt["isolatedWorkingDirectory"] = json!(copied);
    receipt.attempt["namespaceIsolation"] = isolation;
    receipt.attempt["executionInputHashes"] = json!(hashes);
    validate_baseline(
        &mut receipt,
        request.context.expected.is_none()
            && request.context.mutant.is_none()
            && request.patch_sha256.is_none(),
    );
    tokio::task::spawn_blocking(move || {
        directory.write_private("runner.stdout", b"")?;
        directory.write_private("runner.stderr", executed.diagnostics.as_bytes())?;
        Ok::<_, NamespaceError>(())
    })
    .await
    .map_err(|_| invalid("attempt diagnostics publication failed"))??;
    Ok(receipt)
}

fn validate_baseline(receipt: &mut Receipt, pristine: bool) {
    if !pristine || receipt.attempt["outcome"] != "SURVIVED" {
        return;
    }
    if receipt.catalogue.is_null() {
        override_infra(receipt, "missing_native_catalogue");
    } else if !receipt.initialization["observation"]["observation"]["suites"]
        .as_array()
        .is_some_and(|suites| suites.iter().all(Value::is_string))
    {
        override_infra(receipt, "missing_native_initialization_evidence");
    }
}
fn finalize_cancel(receipt: &mut Receipt, cancel: &Cancellation) {
    let cancelled_owner = receipt.attempt["reason"] == "runner_not_completed"
        && receipt.attempt["runner"]["cause"]
            .as_str()
            .is_some_and(|cause| cause.starts_with("Cancelled"));
    let owned = if cancelled_owner {
        let expected = match receipt.attempt["runner"]["cause"].as_str() {
            Some("Cancelled(CtrlC)") => Some(130),
            Some("Cancelled(SigTerm)") => Some(143),
            _ => None,
        };
        if expected.is_none() || expected != receipt.attempt["exitCode"].as_u64() {
            override_infra(receipt, "cancelled_exit_code_mismatch");
            false
        } else {
            true
        }
    } else {
        false
    };
    if cancel.signum().is_some() && (receipt.attempt["outcome"] != "INFRA_ERROR" || owned) {
        receipt.attempt["outcome"] = "CANCELLED".into();
        receipt.attempt["reason"] = "campaign_cancelled".into();
    }
    if matches!(
        receipt.attempt["outcome"].as_str(),
        Some("INFRA_ERROR" | "CANCELLED")
    ) {
        // Classification can already be an infrastructure failure before the
        // owner's integrity overrides. It cannot authorize a reusable cache.
        revoke_private_evidence(receipt);
    }
}

fn seal_receipt(
    mut receipt: Receipt,
    cancel: &Cancellation,
    publication: Publication,
    retain_transport: bool,
) -> Result<(Receipt, Option<Vec<u8>>), NamespaceError> {
    receipt.directory.verify()?;
    // Terminal commit point: later signals stop the enclosing campaign rather
    // than reclassifying this attempt after its receipt has been sealed.
    finalize_cancel(&mut receipt, cancel);
    let response = if publication == Publication::Attempt {
        let bytes = crate::private_response::encode(json!({"attempt":receipt.attempt,
            "catalogue":receipt.catalogue,"initialization":receipt.initialization,"namespacePlan":receipt.namespace_plan}))?;
        receipt
            .directory
            .write_json("result.json", &receipt.attempt)?;
        retain_transport.then_some(bytes)
    } else {
        // Keep the authenticated initial diagnostics if a later causal control
        // fails, without occupying the final mutation verdict's result path.
        receipt
            .directory
            .write_json("attempt.json", &receipt.attempt)?;
        None
    };
    Ok((receipt, response))
}

/// Execute directly under the caller's cancellation token, without spawning a
/// second CLI owner or publishing a private transport response from this layer.
pub(crate) async fn run_owned(
    request: Request,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    publication: Publication,
) -> Result<OwnedAttempt, NamespaceError> {
    run_owned_inner(request, caps, cancel, publication, false).await
}

async fn run_owned_inner(
    request: Request,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    publication: Publication,
    retain_transport: bool,
) -> Result<OwnedAttempt, NamespaceError> {
    validate_request(&request)?;
    verify_replay(request.replay.clone()).await?;
    let started = Instant::now();
    let root = request.workspace.root.clone();
    let campaign = request.workspace.campaign.clone();
    let allocating = cancel.clone();
    let workspace = tokio::task::spawn_blocking(move || {
        AttemptWorkspace::allocate(&root, &campaign, caps, &allocating)
    })
    .await
    .map_err(|_| invalid("attempt allocation failed"))??;
    let workspace = Arc::new(Mutex::new(workspace));
    let result = run_attempt(&request, &workspace, cancel).await;
    // Both checks run regardless of the verdict or cancellation. The retained
    // capability lives past cleanup, through terminal response publication.
    let original = step(&workspace, |workspace| workspace.verify_original()).await;
    let cleanup = step(&workspace, |workspace| workspace.cleanup()).await;
    let replay = verify_replay(request.replay.clone()).await;
    let mut receipt = match result {
        Ok(receipt) => receipt,
        Err(error) => {
            original?;
            cleanup?;
            replay?;
            return Err(error);
        }
    };
    if original.is_err() {
        override_infra(&mut receipt, "source_input_changed");
    }
    if cleanup.is_err() {
        override_infra(&mut receipt, "workspace_cleanup_failed");
    }
    if replay.is_err() {
        override_infra(&mut receipt, "retained_replay_input_changed");
    }
    receipt.attempt["temporaryCopyCleaned"] = cleanup.is_ok().into();
    receipt.attempt["wallSeconds"] = started.elapsed().as_secs_f64().into();
    let cancel = cancel.clone();
    let (receipt, _private_response) = tokio::task::spawn_blocking(move || {
        seal_receipt(receipt, &cancel, publication, retain_transport)
    })
    .await
    .map_err(|_| invalid("attempt receipt publication failed"))??;
    Ok(OwnedAttempt {
        directory: receipt.directory,
        attempt: receipt.attempt,
        catalogue: receipt.catalogue,
        initialization: receipt.initialization,
        namespace_plan: receipt.namespace_plan,
        _workspace: workspace,
        #[cfg(unix)]
        private_response: _private_response,
    })
}

async fn verify_replay(replay: Option<Arc<LoadedReplay>>) -> Result<(), NamespaceError> {
    if let Some(replay) = replay {
        tokio::task::spawn_blocking(move || replay.verify())
            .await
            .map_err(|_| invalid("retained replay verification failed"))??;
    }
    Ok(())
}

#[cfg(unix)]
async fn perform(
    request: Request,
    caps: LifecycleCaps,
    cancel: &Cancellation,
) -> Result<(OwnedWorkspace, Vec<u8>), NamespaceError> {
    let owned = run_owned_inner(request, caps, cancel, Publication::Attempt, true).await?;
    let bytes = owned
        .private_response
        .expect("standalone attempt publication includes its private receipt");
    // The wire receipt is already bounded and sealed. Release its duplicate
    // typed data before stdout publication while retaining the workspace lease.
    drop((
        owned.directory,
        owned.attempt,
        owned.catalogue,
        owned.initialization,
        owned.namespace_plan,
    ));
    Ok((owned._workspace, bytes))
}

#[cfg(unix)]
async fn process(
    request: Request,
    caps: LifecycleCaps,
    readiness: Option<crate::readiness::Readiness>,
) -> Result<(), NamespaceError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancel = Cancellation::default();
    let listening = cancel.clone();
    let listener = tokio::spawn(async move {
        listening.request(tokio::select! { _ = interrupt.recv() => libc::SIGINT, _ = terminate.recv() => libc::SIGTERM });
    });
    if let Some(readiness) = readiness {
        readiness.acknowledge()?;
    }
    let result = match perform(request, caps, &cancel).await {
        Ok((workspace, bytes)) => tokio::task::spawn_blocking(move || {
            let result = crate::private_response::publish(&bytes, &Cancellation::default());
            drop(workspace);
            result.map_err(NamespaceError::from)
        })
        .await
        .map_err(|_| invalid("attempt private publication failed"))
        .and_then(|result| result),
        Err(error) => Err(error),
    };
    listener.abort();
    let _ = listener.await;
    result
}
#[cfg(unix)]
pub(crate) fn execute() -> ExitCode {
    let result = (|| {
        for key in [
            crate::workspace_handoff::HANDOFF_ENV,
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected attempt capability"));
            }
        }
        let fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing attempt request"))?;
        let request = decode(&crate::private_request::read(&fd)?)?;
        let caps = crate::workspace_handoff::capabilities(4)?;
        let readiness = crate::readiness::Readiness::from_env()?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(process(request, caps, readiness))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(match error {
                NamespaceError::Cancelled(libc::SIGTERM) => 143,
                NamespaceError::Cancelled(_) => 130,
                _ => 125,
            })
        }
    }
}

#[cfg(test)]
#[path = "attempt_tests.rs"]
mod tests;
