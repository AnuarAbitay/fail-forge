//! Baseline, bounded mutant queue and original control under one native owner.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use crate::signals::{SignalKind, signal};
use failforge_dart::discovery::verify_candidate;
use failforge_dart::namespace::NamespaceError;
use failforge_dart::{InitializationHandoff, SelectionContext};
use failforge_runner::AwakeInstant;
use failforge_workspace::lifecycle::LifecycleCaps;
#[cfg(any(unix, test))]
use failforge_workspace::parse_json;
use failforge_workspace::{Cancellation, PreparedProject, inspect_project};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::attempt::{Config, NamespaceCache, OwnedAttempt, Publication, Workspace};
use crate::dart_namespace::{absolute, invalid};
#[cfg(any(unix, test))]
use crate::family_stream::{FrameWriter, Stream};
use crate::namespace_planner::{PlanProject, PlanRuntime};
use crate::retained_replay::LoadedReplay;
use crate::workspace_inputs::RuntimeInputs;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EmptyPolicy {
    Controls,
    Skip,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) operation: String,
    pub(crate) project: PreparedProject,
    pub(crate) output: PathBuf,
    pub(crate) runtime: RuntimeInputs,
    pub(crate) config: Config,
    pub(crate) workspace: Workspace,
    pub(crate) candidates: Vec<Value>,
    pub(crate) family: String,
    pub(crate) workers: usize,
    pub(crate) empty_policy: EmptyPolicy,
    pub(crate) frozen: Value,
    pub(crate) environment: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) retained_expected: Option<Vec<[String; 2]>>,
    #[serde(default)]
    pub(crate) framework_root: Option<PathBuf>,
    #[serde(skip)]
    pub(crate) replay: Option<Arc<LoadedReplay>>,
}

#[cfg(any(unix, test))]
fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    decode_value(parse_json(raw)?)
}

pub(crate) fn decode_value(value: Value) -> Result<Request, NamespaceError> {
    for pointer in [
        "",
        "/project",
        "/project/packages",
        "/project/graph",
        "/runtime",
        "/config",
        "/workspace",
        "/frozen",
        "/environment",
    ] {
        if !value.pointer(pointer).is_some_and(Value::is_object) {
            return Err(invalid("family request must use named JSON objects"));
        }
    }
    let request: Request =
        serde_json::from_value(value).map_err(|_| invalid("invalid native family request"))?;
    validate_request(&request)?;
    Ok(request)
}

fn validate_request(request: &Request) -> Result<(), NamespaceError> {
    if request.operation != "family"
        || !(1..=4).contains(&request.workers)
        || request.candidates.len() > 10000
        || !request.project.graph.is_object()
        || !request.frozen.is_object()
        || !matches!(
            request.family.as_str(),
            "classic" | "failure_mode" | "language_specific"
        )
    {
        return Err(invalid("invalid native family configuration"));
    }
    absolute(&request.output)?;
    absolute(&request.runtime.installation)?;
    if request.retained_expected.is_some() != request.framework_root.is_some() {
        return Err(invalid(
            "replay identities and framework must be provided together",
        ));
    }
    if let Some(expected) = &request.retained_expected {
        if request.family != "classic"
            || request.empty_policy != EmptyPolicy::Controls
            || expected.is_empty()
            || expected
                .iter()
                .flatten()
                .any(|part| part.is_empty() || part.contains('\0'))
        {
            return Err(invalid("invalid retained baseline selection"));
        }
    }
    if let Some(framework) = &request.framework_root {
        absolute(framework)?;
    }
    if request
        .environment
        .iter()
        .any(|(key, value)| key.is_empty() || key.contains(['=', '\0']) || value.contains('\0'))
    {
        return Err(invalid("invalid family binding environment"));
    }
    // Reuse attempt preflight before opening artifacts or starting any child.
    attempt_request(request, request.output.join("baseline"), None, None)?;
    let mut ids = BTreeSet::new();
    for candidate in &request.candidates {
        let id = candidate["mutant"]["id"]
            .as_str()
            .ok_or_else(|| invalid("missing family mutant identity"))?;
        if candidate["mutant"]["family"] != request.family || !ids.insert(id) {
            return Err(invalid("mixed-family or duplicate mutation candidates"));
        }
    }
    Ok(())
}

/// A family may publish directly to its private pipe or into an outer campaign.
/// `finish` runs while the input guards and campaign capabilities are still held.
pub(crate) trait FamilyEvents {
    fn emit(&mut self, event: Value) -> io::Result<()>;
    fn finish(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed: usize,
        total: usize,
    ) -> io::Result<()>;
}

#[cfg(any(unix, test))]
impl<W: FrameWriter> FamilyEvents for Stream<W> {
    fn emit(&mut self, event: Value) -> io::Result<()> {
        Stream::emit(self, event)
    }

    fn finish(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed: usize,
        total: usize,
    ) -> io::Result<()> {
        Stream::finish(self, status, error, completed, total)
    }
}

fn attempt_request(
    request: &Request,
    case: PathBuf,
    context: Option<SelectionContext>,
    cache: Option<&NamespaceCache>,
) -> Result<crate::attempt::Request, NamespaceError> {
    let context = context.unwrap_or_else(|| SelectionContext {
        expected: request.retained_expected.clone(),
        alias: if request.project.framework_collision || request.framework_root.is_some() {
            "failforge_subject".into()
        } else {
            request.project.name.clone()
        },
        ..Default::default()
    });
    let mut attempt = crate::attempt::decode_value(
        json!({"operation":"attempt", "project":request.project,
        "case":case,"runtime":{"runner":request.runtime.runner,"dartSdk":request.runtime.dart_sdk,
            "flutterSdk":request.runtime.flutter_sdk,"adapterRoot":request.runtime.adapter_root},
        "config":{"commandTimeout":request.config.command_timeout,"testTimeout":request.config.test_timeout},
        "workspace":{"root":request.workspace.root,"campaign":request.workspace.campaign},
        "context":context,"patchSha256":null,"frameworkRoot":request.framework_root,
        "namespacePlan":cache.map(|cache| json!({"plan":cache.plan,"receipt":cache.receipt}))}),
    )?;
    attempt.replay = request.replay.clone();
    Ok(attempt)
}

/// Retain pathname identity even when all child receipts are already sealed.
pub(crate) struct DirectoryGuard {
    path: PathBuf,
    canonical: PathBuf,
    file: File,
}
impl DirectoryGuard {
    pub(crate) fn open(path: &Path) -> Result<Self, NamespaceError> {
        let canonical = path.canonicalize()?;
        #[cfg(unix)]
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        #[cfg(windows)]
        let file = failforge_workspace::windows_fs::directory(path)?;
        let value = Self {
            path: path.to_owned(),
            canonical,
            file,
        };
        value.verify()?;
        Ok(value)
    }
    pub(crate) fn verify(&self) -> Result<(), NamespaceError> {
        #[cfg(unix)]
        {
            let held = self.file.metadata()?;
            let named = std::fs::symlink_metadata(&self.path)?;
            if !named.is_dir()
                || held.dev() != named.dev()
                || held.ino() != named.ino()
                || self.path.canonicalize()? != self.canonical
            {
                return Err(invalid("family directory binding changed"));
            }
        }
        #[cfg(windows)]
        {
            use failforge_workspace::windows_fs as win;
            let named = win::directory(&self.path)?;
            if win::identity(&named)? != win::identity(&self.file)?
                || self.path.canonicalize()? != self.canonical
            {
                return Err(invalid("family directory binding changed"));
            }
        }
        Ok(())
    }

    pub(crate) fn canonical_path(&self) -> Result<PathBuf, NamespaceError> {
        self.verify()?;
        Ok(self.canonical.clone())
    }

    pub(crate) fn descriptor(&self) -> Result<File, NamespaceError> {
        self.verify()?;
        Ok(self.file.try_clone()?)
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, NamespaceError> + Send + 'static,
) -> Result<T, NamespaceError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| invalid("family input operation failed"))?
}

async fn verify_frozen(request: Arc<Request>) -> Result<(), NamespaceError> {
    blocking(move || {
        if let Some(replay) = &request.replay {
            replay.verify()?;
        }
        let current = crate::workspace_inputs::bind_inputs(
            &request.runtime,
            &request.project,
            &request.environment,
            &Cancellation::default(),
        )?;
        if current != request.frozen {
            return Err(invalid(
                "SDK, dependency or snapshot inputs changed during family",
            ));
        }
        Ok(())
    })
    .await
}

struct Inputs {
    root: DirectoryGuard,
    output: DirectoryGuard,
}
async fn prepare(
    request: Arc<Request>,
    caps: LifecycleCaps,
    cancel: Cancellation,
) -> Result<(Inputs, LifecycleCaps), NamespaceError> {
    blocking(move || {
        cancel.check()?;
        caps.verify(
            &request.workspace.root,
            &request.workspace.campaign,
            &cancel,
        )?;
        let root = DirectoryGuard::open(&request.project.root)?;
        if inspect_project(&request.project.root, &cancel)? != request.project {
            return Err(invalid("prepared family project changed"));
        }
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
        protected.push(request.workspace.root.canonicalize()?);
        let output = DirectoryGuard::open(&request.output)?;
        if protected
            .iter()
            .any(|path| output.canonical.starts_with(path) || path.starts_with(&output.canonical))
        {
            return Err(invalid("family report overlaps protected inputs"));
        }
        for candidate in &request.candidates {
            cancel.check()?;
            verify_candidate(&request.project.root, candidate)
                .map_err(|error| invalid(&error.to_string()))?;
        }
        Ok((Inputs { root, output }, caps))
    })
    .await
}

struct Baseline {
    context: SelectionContext,
    initialization: Value,
    cache: Option<NamespaceCache>,
    // Evidence and its namespace artifacts remain bound until every worker drains.
    _owned: OwnedAttempt,
}
fn baseline_evidence(request: &Request, owned: OwnedAttempt) -> Result<Baseline, NamespaceError> {
    let expected = serde_json::from_value(owned.attempt["testIdentities"].clone())
        .map_err(|_| invalid("baseline has invalid test identities"))?;
    let catalogue = serde_json::from_value(owned.catalogue.clone())
        .map_err(|_| invalid("baseline is missing private catalogue"))?;
    let handoff: InitializationHandoff = serde_json::from_value(owned.initialization.clone())
        .map_err(|_| invalid("baseline is missing initialization evidence"))?;
    let initialization = serde_json::to_value(handoff.observation)
        .map_err(|_| invalid("baseline initialization cannot be encoded"))?;
    let suites = initialization["observation"]["suites"].clone();
    if !suites
        .as_array()
        .is_some_and(|paths| paths.iter().all(Value::is_string))
    {
        return Err(invalid("baseline has invalid suite selection"));
    }
    let context = SelectionContext {
        expected: Some(expected),
        baseline: Some(catalogue),
        alias: if request.project.framework_collision || request.framework_root.is_some() {
            "failforge_subject".into()
        } else {
            request.project.name.clone()
        },
        ..Default::default()
    };
    failforge_dart::validate_selection_context(&context)
        .map_err(|_| invalid("invalid baseline selection"))?;
    owned.directory.write_json(
        "catalog.json",
        &json!({"identities":context.expected,"suites":suites}),
    )?;
    let cache = if owned.namespace_plan.is_null() {
        None
    } else {
        Some(
            serde_json::from_value(owned.namespace_plan.clone())
                .map_err(|_| invalid("invalid baseline namespace plan"))?,
        )
    };
    Ok(Baseline {
        context,
        initialization,
        cache,
        _owned: owned,
    })
}

fn mutant_request(
    request: &Request,
    baseline: &Baseline,
    candidate: &Value,
    output: &Path,
) -> Result<crate::mutant_execution::Request, NamespaceError> {
    let mut context =
        serde_json::to_value(&baseline.context).map_err(|_| invalid("invalid family selection"))?;
    context["mutant"] = candidate["mutant"].clone();
    context["initialization"] = json!({"role":"initial","candidateValid":candidate["validation"]["valid"],
        "baseline":baseline.initialization});
    let mut mutant = crate::mutant_execution::decode_value(
        json!({"operation":"mutant","project":request.project,
        "case":output.join("mutants").join(candidate["mutant"]["id"].as_str().ok_or_else(||invalid("missing mutant id"))?),
        "runtime":{"installation":request.runtime.installation,"runner":request.runtime.runner,
            "dartSdk":request.runtime.dart_sdk,"flutterSdk":request.runtime.flutter_sdk,"adapterRoot":request.runtime.adapter_root},
        "config":{"commandTimeout":request.config.command_timeout,"testTimeout":request.config.test_timeout},
        "workspace":{"root":request.workspace.root,"campaign":request.workspace.campaign},
        "candidate":candidate,"context":context,"frozen":request.frozen,"environment":request.environment,
        "frameworkRoot":request.framework_root,
        "namespacePlan":baseline.cache.as_ref().map(|cache|json!({"plan":cache.plan,"receipt":cache.receipt}))}),
    )?;
    mutant.replay = request.replay.clone();
    Ok(mutant)
}

#[derive(Default)]
struct Ledger {
    ids: BTreeSet<String>,
    inconclusive: bool,
    infrastructure: bool,
}
impl Ledger {
    fn publish(
        &mut self,
        request: &Request,
        index: usize,
        record: &Value,
        send: impl FnOnce() -> Result<(), NamespaceError>,
    ) -> Result<(), NamespaceError> {
        let candidate = request
            .candidates
            .get(index)
            .ok_or_else(|| invalid("unknown completed candidate"))?;
        let id = record["mutantId"]
            .as_str()
            .ok_or_else(|| invalid("missing completed mutant id"))?;
        if record["recordKind"] != "mutation"
            || record["family"] != request.family
            || candidate["mutant"]["id"] != id
            || self.ids.contains(id)
        {
            return Err(invalid("duplicate or mismatched completed mutant"));
        }
        let outcome = record["outcome"]
            .as_str()
            .ok_or_else(|| invalid("missing completed mutant outcome"))?;
        if !matches!(
            outcome,
            "KILLED"
                | "SURVIVED"
                | "INVALID"
                | "TIMEOUT"
                | "INFRA_ERROR"
                | "UNCLASSIFIED"
                | "NO_TESTS"
                | "CANCELLED"
        ) || !record["reason"].is_string()
        {
            return Err(invalid("invalid completed mutant verdict"));
        }
        send()?;
        self.ids.insert(id.to_owned());
        self.inconclusive |= matches!(
            outcome,
            "INFRA_ERROR" | "UNCLASSIFIED" | "NO_TESTS" | "CANCELLED"
        );
        self.infrastructure |= outcome == "INFRA_ERROR";
        Ok(())
    }
}

fn stage(
    stream: &mut impl FamilyEvents,
    name: &str,
    elapsed: Option<f64>,
) -> Result<(), NamespaceError> {
    let frame = match elapsed {
        Some(seconds) => json!({"kind":"stage_finished","stage":name,"wallSeconds":seconds}),
        None => json!({"kind":"stage_started","stage":name}),
    };
    stream.emit(frame)?;
    Ok(())
}

/// Suspension shorter than this is clock noise, not a sleep worth reporting.
const SLEEP_NOTICE: Duration = Duration::from_secs(1);

/// Name system sleep in a control that timed out while sleep overlapped it.
/// Command timeouts skip sleep, but Dart's own per-test timeouts count it.
/// Other failures keep their message: sleep did not cause them.
fn control_failure(message: &str, attempt: &Value, suspended: Duration) -> String {
    let timed_out = matches!(
        attempt["reason"].as_str(),
        Some("runner_timeout" | "test_timeout")
    );
    if !timed_out || suspended < SLEEP_NOTICE {
        return message.into();
    }
    format!(
        "{message}; the system slept for {}s during this run, which can expire \
         Dart test timeouts; keep the computer awake and run again",
        suspended.as_secs()
    )
}

fn require_passing(
    attempt: &Value,
    cancel: &Cancellation,
    message: &str,
) -> Result<(), NamespaceError> {
    // Integrity failures are authoritative even when the owner also received a signal.
    if attempt["outcome"] == "INFRA_ERROR" {
        return Err(invalid(message));
    }
    cancel.check()?;
    if attempt["outcome"] != "SURVIVED" {
        return Err(invalid(message));
    }
    Ok(())
}

async fn run_family(
    request: Arc<Request>,
    inputs: &Inputs,
    caps: &LifecycleCaps,
    cancel: &Cancellation,
    stream: &mut impl FamilyEvents,
    ledger: &mut Ledger,
) -> Result<(), NamespaceError> {
    cancel.check()?;
    if request.candidates.is_empty() && request.empty_policy == EmptyPolicy::Skip {
        return Ok(());
    }
    inputs.output.verify()?;
    let baseline_request = attempt_request(
        &request,
        inputs.output.canonical.join("baseline"),
        None,
        None,
    )?;
    let baseline_caps = caps.try_clone()?;
    stage(stream, "baseline", None)?;
    let begin = Instant::now();
    let awake = AwakeInstant::now();
    let baseline = crate::attempt::run_owned(
        baseline_request,
        baseline_caps,
        cancel,
        Publication::Attempt,
    )
    .await;
    if let Ok(owned) = &baseline {
        stream.emit(json!({"kind":"baseline","attempt":owned.attempt}))?;
    }
    stage(stream, "baseline", Some(begin.elapsed().as_secs_f64()))?;
    let baseline = baseline?;
    require_passing(
        &baseline.attempt,
        cancel,
        &control_failure(
            "original baseline did not pass; no mutation score established",
            &baseline.attempt,
            awake.suspended_elapsed(),
        ),
    )?;
    let baseline = Arc::new(baseline_evidence(&request, baseline)?);
    let _replay_control = if request.retained_expected.is_some() {
        let control_request = attempt_request(
            &request,
            inputs.output.canonical.join("original-before"),
            Some(baseline.context.clone()),
            baseline.cache.as_ref(),
        )?;
        stage(stream, "original_before", None)?;
        let begin = Instant::now();
        let awake = AwakeInstant::now();
        let control = crate::attempt::run_owned(
            control_request,
            caps.try_clone()?,
            cancel,
            Publication::Attempt,
        )
        .await;
        if let Ok(owned) = &control {
            stream.emit(json!({"kind":"original_before","attempt":owned.attempt}))?;
        }
        stage(
            stream,
            "original_before",
            Some(begin.elapsed().as_secs_f64()),
        )?;
        let control = control?;
        require_passing(
            &control.attempt,
            cancel,
            &control_failure(
                "second original control before replay did not pass",
                &control.attempt,
                awake.suspended_elapsed(),
            ),
        )?;
        verify_frozen(Arc::clone(&request)).await?;
        Some(control)
    } else {
        None
    };
    let owner = Arc::new(caps.try_clone()?);
    stage(stream, "mutations", None)?;
    let begin = Instant::now();
    let next_request = Arc::clone(&request);
    let next_baseline = Arc::clone(&baseline);
    let next_output = inputs.output.canonical.clone();
    let next_cancel = cancel.clone();
    let summary = crate::family_queue::run(
        0..request.candidates.len(),
        request.workers,
        cancel,
        move |_index, index| {
            let request = Arc::clone(&next_request);
            let baseline = Arc::clone(&next_baseline);
            let caps = Arc::clone(&owner);
            let output = next_output.clone();
            let cancel = next_cancel.clone();
            async move {
                let child =
                    mutant_request(&request, &baseline, &request.candidates[index], &output)?;
                crate::mutant_execution::run_owned(child, caps.try_clone()?, &cancel).await
            }
        },
        |index, owned| {
            inputs.output.verify()?;
            ledger.publish(&request, index, &owned.record, || {
                stream.emit(json!({"kind":"mutant","index":index,"record":owned.record}))?;
                Ok(())
            })
        },
    )
    .await;
    stage(stream, "mutations", Some(begin.elapsed().as_secs_f64()))?;
    if let Some(error) = summary.error {
        return Err(error);
    }
    cancel.check()?;
    inputs.output.verify()?;
    let control_request = attempt_request(
        &request,
        inputs.output.canonical.join("original-after"),
        Some(baseline.context.clone()),
        baseline.cache.as_ref(),
    )?;
    let control_caps = caps.try_clone()?;
    stage(stream, "original_after", None)?;
    let begin = Instant::now();
    let awake = AwakeInstant::now();
    let control =
        crate::attempt::run_owned(control_request, control_caps, cancel, Publication::Attempt)
            .await;
    if let Ok(owned) = &control {
        stream.emit(json!({"kind":"original_after","attempt":owned.attempt}))?;
    }
    stage(
        stream,
        "original_after",
        Some(begin.elapsed().as_secs_f64()),
    )?;
    let control = control?;
    require_passing(
        &control.attempt,
        cancel,
        &control_failure(
            "original control after mutations did not pass",
            &control.attempt,
            awake.suspended_elapsed(),
        ),
    )?;
    if ledger.ids.len() != request.candidates.len() {
        return Err(invalid("family did not process every discovered mutant"));
    }
    if ledger.inconclusive {
        return Err(invalid(
            "one or more mutant attempts lack a conclusive result; see records",
        ));
    }
    Ok(())
}

pub(crate) async fn run_owned(
    request: Request,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    stream: &mut impl FamilyEvents,
) -> Result<(), NamespaceError> {
    validate_request(&request)?;
    execute_owned(Arc::new(request), caps, cancel, stream).await
}

async fn execute_owned(
    request: Arc<Request>,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    stream: &mut impl FamilyEvents,
) -> Result<(), NamespaceError> {
    let total = request.candidates.len();
    let mut ledger = Ledger::default();
    // Even a cancellation before preparation is reported as an authenticated terminal.
    let prepared = prepare(Arc::clone(&request), caps, cancel.clone()).await;
    let (inputs, caps) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return terminal(stream, Err(error), cancel, &ledger, total),
    };
    let mut result = verify_frozen(Arc::clone(&request)).await;
    if result.is_ok() {
        result = run_family(
            Arc::clone(&request),
            &inputs,
            &caps,
            cancel,
            stream,
            &mut ledger,
        )
        .await;
    }
    // Every started task has been joined before these independent final guards.
    // Verify on cancellation/error too; no cancellation token may skip integrity.
    let frozen = verify_frozen(Arc::clone(&request)).await;
    let guards = blocking(move || {
        inputs.root.verify()?;
        inputs.output.verify()?;
        caps.verify(
            &request.workspace.root,
            &request.workspace.campaign,
            &Cancellation::default(),
        )?;
        Ok((inputs, caps))
    })
    .await;
    let guards = match guards {
        Ok(guards) => guards,
        Err(error) => return terminal(stream, Err(error), cancel, &ledger, total),
    };
    if let Err(error) = frozen {
        result = Err(error);
    }
    let published = terminal(stream, result, cancel, &ledger, total);
    drop(guards);
    published
}

fn terminal(
    stream: &mut impl FamilyEvents,
    result: Result<(), NamespaceError>,
    cancel: &Cancellation,
    ledger: &Ledger,
    total: usize,
) -> Result<(), NamespaceError> {
    let (status, error) = terminal_state(result, cancel, ledger, total);
    stream.finish(status, error.as_deref(), ledger.ids.len(), total)?;
    Ok(())
}

fn terminal_state(
    result: Result<(), NamespaceError>,
    cancel: &Cancellation,
    ledger: &Ledger,
    total: usize,
) -> (&'static str, Option<String>) {
    if ledger.infrastructure {
        (
            "failed",
            Some("infrastructure failure during family; see records".to_owned()),
        )
    } else {
        match result {
            Err(NamespaceError::Cancelled(_)) => ("cancelled", None),
            Err(error) => ("failed", Some(error.to_string())),
            Ok(()) if cancel.signum().is_some() => ("cancelled", None),
            Ok(()) if ledger.ids.len() != total || ledger.inconclusive => (
                "failed",
                Some("family is missing conclusive mutation results".to_owned()),
            ),
            Ok(()) if total == 0 => ("no_mutants", None),
            Ok(()) => ("completed", None),
        }
    }
}

#[cfg(unix)]
async fn process(
    request: Request,
    caps: LifecycleCaps,
    ready: Option<crate::readiness::Readiness>,
) -> Result<(), NamespaceError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancel = Cancellation::default();
    let listening = cancel.clone();
    let listener = tokio::spawn(async move {
        listening.request(tokio::select! {
        _=interrupt.recv()=>libc::SIGINT,_=terminate.recv()=>libc::SIGTERM});
    });
    let result = async {
        let mut stream = Stream::stdout()?;
        if let Some(ready) = ready {
            ready.acknowledge()?;
        }
        run_owned(request, caps, &cancel, &mut stream).await
    }
    .await;
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
                return Err(invalid("unexpected family capability"));
            }
        }
        let fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing family request"))?;
        let request = decode(&crate::private_request::read(&fd)?)?;
        let caps = crate::workspace_handoff::capabilities(4)?;
        let ready = crate::readiness::Readiness::from_env()?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(process(request, caps, ready))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(125)
        }
    }
}

#[cfg(test)]
#[path = "family_execution_tests.rs"]
mod tests;
