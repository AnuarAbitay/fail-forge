//! One native owner of a candidate and its ordered causal controls.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use failforge_dart::discovery::{VerifiedCandidate, verify_candidate};
use failforge_dart::namespace::NamespaceError;
use failforge_dart::{
    InitializationHandoff, InitializationRequest, InitializationRole, SelectionContext,
};
use failforge_workspace::lifecycle::LifecycleCaps;
use failforge_workspace::{Cancellation, PreparedProject, inspect_project, parse_json};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};

use crate::attempt::{NamespaceCache, OwnedAttempt, Publication};
use crate::dart_namespace::{absolute, invalid};
use crate::mutant_confirmation::Confirmation;
use crate::namespace_planner::{PlanProject, PlanRuntime, ReportDirectory};
use crate::workspace_inputs::RuntimeInputs;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    operation: String,
    project: PreparedProject,
    case: PathBuf,
    runtime: RuntimeInputs,
    config: crate::attempt::Config,
    workspace: crate::attempt::Workspace,
    candidate: Value,
    context: SelectionContext,
    namespace_plan: Option<NamespaceCache>,
    frozen: Value,
    environment: BTreeMap<String, String>,
}

fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    decode_value(parse_json(raw)?)
}

pub(crate) fn decode_value(value: Value) -> Result<Request, NamespaceError> {
    if !value.is_object() {
        return Err(invalid("native mutant request must be an object"));
    }
    // Serde structs also accept positional arrays. The private protocol has
    // named fields only, so reject sequence-shaped objects before decoding.
    for pointer in [
        "/project",
        "/runtime",
        "/config",
        "/workspace",
        "/context",
        "/candidate",
        "/frozen",
        "/environment",
        "/project/packages",
        "/project/graph",
        "/context/mutant",
        "/context/baseline",
        "/context/initialization",
        "/context/initialization/baseline",
    ] {
        if !value.pointer(pointer).is_some_and(Value::is_object) {
            return Err(invalid("invalid native mutant object shape"));
        }
    }
    let request: Request =
        serde_json::from_value(value).map_err(|_| invalid("invalid native mutant request"))?;
    if request.operation != "mutant"
        || !request.frozen.is_object()
        || request.context.mutant.as_ref() != request.candidate.get("mutant")
        || request.context.expected.is_none()
        || request.context.baseline.is_none()
        || request.case.file_name().and_then(|name| name.to_str())
            != request.candidate["mutant"]["id"].as_str()
    {
        return Err(invalid("invalid native mutant binding"));
    }
    absolute(&request.runtime.installation)?;
    let Some(InitializationRequest::Initial {
        candidate_valid, ..
    }) = &request.context.initialization
    else {
        return Err(invalid(
            "mutant execution requires initial baseline evidence",
        ));
    };
    if request.candidate["validation"]["valid"].as_bool() != Some(*candidate_valid) {
        return Err(invalid("mutant validation and initialization disagree"));
    }
    // Validate all shared attempt fields before opening reports or allocating.
    let mut context = request.context.clone();
    if !candidate_valid {
        context.initialization = None;
    }
    attempt_request(
        &request,
        request.case.clone(),
        context,
        if *candidate_valid {
            request.candidate["safePatch"]["patchedHash"]
                .as_str()
                .map(str::to_owned)
        } else {
            None
        },
        request.namespace_plan.clone(),
    )?;
    if request
        .environment
        .iter()
        .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
    {
        return Err(invalid("invalid mutant binding environment"));
    }
    Ok(request)
}

fn attempt_request(
    request: &Request,
    case: PathBuf,
    context: SelectionContext,
    patch_sha256: Option<String>,
    namespace_plan: Option<NamespaceCache>,
) -> Result<crate::attempt::Request, NamespaceError> {
    crate::attempt::decode_value(json!({"operation":"attempt", "project":request.project,
        "case":case, "runtime":{"runner":request.runtime.runner,"dartSdk":request.runtime.dart_sdk,
            "flutterSdk":request.runtime.flutter_sdk,"adapterRoot":request.runtime.adapter_root},
        "config":{"commandTimeout":request.config.command_timeout,"testTimeout":request.config.test_timeout},
        "workspace":{"root":request.workspace.root,"campaign":request.workspace.campaign},
        "context":context,"patchSha256":patch_sha256,
        "namespacePlan":namespace_plan.map(|cache| json!({"plan":cache.plan,"receipt":cache.receipt}))}))
}

struct InputGuard {
    project: PreparedProject,
    root: File,
}
impl InputGuard {
    fn new(project: &PreparedProject) -> Result<Self, NamespaceError> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&project.root)?;
        let guard = Self {
            project: project.clone(),
            root,
        };
        guard.verify()?;
        Ok(guard)
    }
    fn verify(&self) -> Result<(), NamespaceError> {
        let owned = self.root.metadata()?;
        let named = std::fs::symlink_metadata(&self.project.root)?;
        if !named.is_dir()
            || owned.dev() != named.dev()
            || owned.ino() != named.ino()
            || inspect_project(&self.project.root, &Cancellation::default())? != self.project
        {
            return Err(invalid("mutant prepared inputs changed"));
        }
        Ok(())
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, NamespaceError> + Send + 'static,
) -> Result<T, NamespaceError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| invalid("native mutant operation failed"))?
}

async fn verify_frozen(request: &Request) -> Result<(), NamespaceError> {
    let request = request.clone();
    blocking(move || {
        let current = crate::workspace_inputs::bind_inputs(
            &request.runtime,
            &request.project,
            &request.environment,
            &Cancellation::default(),
        )?;
        if current != request.frozen {
            return Err(invalid(
                "execution bindings changed during initialization confirmation",
            ));
        }
        Ok(())
    })
    .await
}

fn record(candidate: &VerifiedCandidate) -> Value {
    let mutant = &candidate.raw_mutant;
    let mut value = json!({"recordKind":"mutation","mutantId":mutant["id"],
        "family":mutant["family"],"operatorId":mutant["operatorId"],"file":mutant["file"],
        "edit":mutant["edit"],"validation":candidate.validation});
    if candidate.mutant.family == failforge_dart::MutationFamily::LanguageSpecific {
        value["language"] = "dart".into();
        if let Some((category, _)) = failforge_config::catalogue::DART_OPERATOR_GROUPS
            .iter()
            .find(|(_, operators)| operators.contains(&candidate.mutant.operator_id.as_str()))
        {
            value["category"] = (*category).into();
        }
    }
    value
}

fn handoff(value: &Value) -> Result<Option<InitializationHandoff>, NamespaceError> {
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| invalid("invalid owned initialization evidence"))
}
fn cache(value: &Value) -> Result<Option<NamespaceCache>, NamespaceError> {
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| invalid("invalid owned namespace cache"))
}
fn role_name(role: InitializationRole) -> &'static str {
    match role {
        InitializationRole::OriginalBefore => "original-before",
        InitializationRole::MutantReplay => "mutant-replay",
        InitializationRole::OriginalAfter => "original-after",
    }
}

fn finalize_cancel(value: &mut Value, cancel: &Cancellation) {
    if cancel.signum().is_some() && value["outcome"] != "INFRA_ERROR" {
        value["outcome"] = "CANCELLED".into();
        value["reason"] = "campaign_cancelled".into();
        if let Some(proof) = value.get_mut("initializationConfirmation") {
            proof["confirmed"] = false.into();
        }
    }
}

fn finalize_source(
    value: &mut Value,
    verification: Result<(), NamespaceError>,
) -> Result<(), NamespaceError> {
    if let Err(error) = verification {
        // An executed attempt has authenticated diagnostics worth retaining.
        // A static-invalid candidate has no such receipt to publish on drift.
        if value["compileChecked"] != true {
            return Err(error);
        }
        if value["outcome"] != "INFRA_ERROR" {
            value["outcome"] = "INFRA_ERROR".into();
            value["reason"] = "source_input_changed".into();
        }
        if let Some(proof) = value.get_mut("initializationConfirmation") {
            proof["confirmed"] = false.into();
        }
    }
    Ok(())
}

struct Completed {
    directory: Arc<ReportDirectory>,
    record: Value,
    namespace_plan: Option<NamespaceCache>,
    namespace_checked: bool,
    // Keep the exact leased descriptors through final report publication.
    _attempts: Vec<OwnedAttempt>,
}

/// A terminal mutation receipt whose capabilities remain pinned until its
/// caller has published the corresponding completion event.
pub(crate) struct OwnedMutant {
    pub record: Value,
    pub namespace_plan: Option<NamespaceCache>,
    pub namespace_checked: bool,
    _directory: Arc<ReportDirectory>,
    _attempts: Vec<OwnedAttempt>,
    _caps: LifecycleCaps,
    private_response: Option<Vec<u8>>,
}

fn finalize_result(
    result: Result<Completed, NamespaceError>,
    original: Result<(), NamespaceError>,
) -> Result<Completed, NamespaceError> {
    match result {
        Ok(mut completed) => {
            finalize_source(&mut completed.record, original)?;
            Ok(completed)
        }
        Err(error) => {
            // With no receipt to preserve, source-integrity failures must still
            // take precedence over cancellation or other execution failures.
            original?;
            Err(error)
        }
    }
}

async fn run_valid(
    request: &Request,
    candidate: &VerifiedCandidate,
    caps: &LifecycleCaps,
    cancel: &Cancellation,
) -> Result<Completed, NamespaceError> {
    let first = attempt_request(
        request,
        request.case.clone(),
        request.context.clone(),
        candidate.patch_sha256.clone(),
        request.namespace_plan.clone(),
    )?;
    let initial =
        crate::attempt::run_owned(first, caps.try_clone()?, cancel, Publication::Deferred).await?;
    let directory = Arc::clone(&initial.directory);
    let mut namespace_plan = cache(&initial.namespace_plan)?;
    let mut value = initial.attempt.clone();
    let mut attempts = Vec::with_capacity(4);
    if value["outcome"] == "LOAD_FAILURE" {
        let mut confirmation = Confirmation::new(value, handoff(&initial.initialization)?);
        attempts.push(initial);
        loop {
            if cancel.signum().is_some() {
                confirmation.cancel();
                break;
            }
            let Some((role, initialization)) = confirmation.next_request()? else {
                break;
            };
            verify_frozen(request).await?;
            if cancel.signum().is_some() {
                confirmation.cancel();
                break;
            }
            let mut context = request.context.clone();
            context.initialization = Some(initialization);
            let case = directory
                .canonical_path()?
                .join("initialization")
                .join(role_name(role));
            let control = attempt_request(
                request,
                case,
                context,
                if role == InitializationRole::MutantReplay {
                    candidate.patch_sha256.clone()
                } else {
                    None
                },
                namespace_plan.clone(),
            )?;
            let control = match crate::attempt::run_owned(
                control,
                caps.try_clone()?,
                cancel,
                Publication::Attempt,
            )
            .await
            {
                Err(NamespaceError::Cancelled(_)) => {
                    confirmation.cancel();
                    break;
                }
                other => other?,
            };
            namespace_plan = cache(&control.namespace_plan)?;
            let infra = control.attempt["outcome"] == "INFRA_ERROR";
            confirmation.observe(
                role,
                control.attempt.clone(),
                handoff(&control.initialization)?,
            )?;
            attempts.push(control);
            if infra {
                break;
            }
            // Complete input verification even if a signal arrived after the
            // attempt's terminal commit. A policy decision cannot bypass it.
            verify_frozen(request).await?;
            if cancel.signum().is_some() {
                confirmation.cancel();
                break;
            }
        }
        value = confirmation.finish()?;
    } else {
        attempts.push(initial);
    }
    let mut result = record(candidate);
    result.as_object_mut().expect("mutation record").extend(
        value
            .as_object()
            .ok_or_else(|| invalid("invalid owned attempt result"))?
            .clone(),
    );
    Ok(Completed {
        directory,
        record: result,
        namespace_plan,
        namespace_checked: request.project.framework_collision,
        _attempts: attempts,
    })
}

async fn run_invalid(
    request: &Request,
    candidate: &VerifiedCandidate,
    cancel: &Cancellation,
) -> Result<Completed, NamespaceError> {
    cancel.check()?;
    let request = request.clone();
    let directory = blocking(move || {
        let project = PlanProject {
            root: request.project.root,
            name: request.project.name,
            flutter: request.project.flutter,
        };
        let runtime = PlanRuntime {
            dart_sdk: request.runtime.dart_sdk,
            flutter_sdk: request.runtime.flutter_sdk,
            adapter_root: request.runtime.adapter_root,
        };
        let mut protected = crate::namespace_planner::protected_inputs(&project, &runtime)?;
        protected.push(request.workspace.root.canonicalize()?);
        crate::attempt_artifacts::create(&request.case, &protected)
    })
    .await?;
    let mut result = record(candidate);
    result["outcome"] = "INVALID".into();
    result["reason"] = "static_analysis".into();
    result["compileChecked"] = false.into();
    Ok(Completed {
        directory: Arc::new(directory),
        record: result,
        namespace_plan: None,
        namespace_checked: false,
        _attempts: Vec::new(),
    })
}

/// Execute in the caller's runtime without another CLI or retained wire buffer.
pub(crate) async fn run_owned(
    request: Request,
    caps: LifecycleCaps,
    cancel: &Cancellation,
) -> Result<OwnedMutant, NamespaceError> {
    run_owned_inner(request, caps, cancel, false).await
}

async fn run_owned_inner(
    request: Request,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    retain_transport: bool,
) -> Result<OwnedMutant, NamespaceError> {
    let preparing = cancel.clone();
    let (request, caps, guard, candidate) = blocking(move || {
        preparing.check()?;
        caps.verify(
            &request.workspace.root,
            &request.workspace.campaign,
            &preparing,
        )?;
        let guard = InputGuard::new(&request.project)?;
        let candidate = verify_candidate(&request.project.root, &request.candidate)
            .map_err(|error| invalid(&error.to_string()))?;
        Ok((request, caps, guard, candidate))
    })
    .await?;
    let result = if candidate.patch_sha256.is_some() {
        run_valid(&request, &candidate, &caps, cancel).await
    } else {
        run_invalid(&request, &candidate, cancel).await
    };
    let finalizing = cancel.clone();
    blocking(move || {
        // Always verify the original and capability binding, including failures
        // and cancellation; preserve failures rather than relabel them clean.
        let original = guard.verify().and_then(|()| {
            verify_candidate(&request.project.root, &request.candidate)
                .map(|_| ())
                .map_err(|error| invalid(&error.to_string()))
        });
        caps.verify(
            &request.workspace.root,
            &request.workspace.campaign,
            &Cancellation::default(),
        )?;
        let completed = finalize_result(result, original)?;
        seal_completed(completed, caps, &finalizing, retain_transport)
    })
    .await
}

fn seal_completed(
    mut completed: Completed,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    retain_transport: bool,
) -> Result<OwnedMutant, NamespaceError> {
    completed.directory.verify()?;
    if !completed.record["compileChecked"]
        .as_bool()
        .unwrap_or(false)
    {
        cancel.check()?;
    } else {
        finalize_cancel(&mut completed.record, cancel);
    }
    if matches!(
        completed.record["outcome"].as_str(),
        Some("INFRA_ERROR" | "CANCELLED")
    ) {
        completed.namespace_plan = None;
        completed.namespace_checked = false;
    }
    let mut owned = OwnedMutant {
        record: completed.record,
        namespace_plan: completed.namespace_plan,
        namespace_checked: completed.namespace_checked,
        _directory: completed.directory,
        _attempts: completed._attempts,
        _caps: caps,
        private_response: None,
    };
    // Keep the same per-record preflight before sealing in both modes.
    // In-process callers publish their own bounded event and retain no encoding.
    let cache = owned
        .namespace_plan
        .as_ref()
        .map(|cache| json!({"plan":cache.plan,"receipt":cache.receipt}));
    let response = crate::private_response::encode(json!({"record":owned.record,
        "namespacePlan":cache,"namespaceChecked":owned.namespace_checked}))?;
    owned.private_response = retain_transport.then_some(response);
    owned._directory.write_json("result.json", &owned.record)?;
    Ok(owned)
}

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
        listening.request(
            tokio::select! { _=interrupt.recv()=>libc::SIGINT, _=terminate.recv()=>libc::SIGTERM },
        );
    });
    let result = async {
        if let Some(readiness) = readiness {
            readiness.acknowledge()?;
        }
        let mut completed = run_owned_inner(request, caps, &cancel, true).await?;
        blocking(move || {
            let bytes = completed
                .private_response
                .take()
                .ok_or_else(|| invalid("missing native mutant response"))?;
            let result = crate::private_response::publish(&bytes, &Cancellation::default());
            drop(completed);
            result.map_err(Into::into)
        })
        .await
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
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected mutant capability"));
            }
        }
        let fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing mutant request"))?;
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
#[path = "mutant_execution_tests.rs"]
mod tests;
