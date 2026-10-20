//! Sequential discovery and family execution over one frozen campaign snapshot.
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use failforge_dart::namespace::NamespaceError;
use failforge_runner::WorkspaceLease;
use failforge_workspace::lifecycle::LifecycleCaps;
use failforge_workspace::{
    Cancellation, Hashes, PreparedProject, inspect_project, parse_json, verify_source,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};

use crate::attempt::{Config, Workspace};
use crate::dart_namespace::{absolute, invalid};
use crate::family_execution::{DirectoryGuard, FamilyEvents};
use crate::family_stream::Stream;
use crate::namespace_planner::{PlanProject, PlanRuntime};
use crate::retained_replay::LoadedReplay;
use crate::workspace_inputs::RuntimeInputs;

const FAMILIES: [&str; 3] = ["classic", "failure_mode", "language_specific"];

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Direct,
    Profile,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Original {
    root: PathBuf,
    hashes: Hashes,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Family {
    family: String,
    rules: Value,
    workers: usize,
    config: Config,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    operation: String,
    mode: Mode,
    project: PreparedProject,
    original: Original,
    output: PathBuf,
    runtime: RuntimeInputs,
    workspace: Workspace,
    frozen: Value,
    environment: BTreeMap<String, String>,
    max_mutants: usize,
    families: Vec<Family>,
    #[serde(skip)]
    pub(crate) replay: Option<Arc<LoadedReplay>>,
}

fn rules(value: &Value, component: bool) -> Result<(), NamespaceError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("campaign rules must be an object"))?;
    for (key, value) in object {
        match key.as_str() {
            "operators" | "includePaths" | "excludePaths" | "classes" | "methods" => {
                if !value
                    .as_array()
                    .is_some_and(|values| values.iter().all(Value::is_string))
                {
                    return Err(invalid("campaign rule selections must be string arrays"));
                }
            }
            "components" if !component => {
                for child in value
                    .as_array()
                    .ok_or_else(|| invalid("invalid campaign components"))?
                {
                    rules(child, true)?;
                    if !child["name"].as_str().is_some_and(|name| !name.is_empty()) {
                        return Err(invalid("campaign component needs a name"));
                    }
                }
            }
            "name" if component && value.is_string() => {}
            _ => return Err(invalid("unknown campaign rule field")),
        }
    }
    Ok(())
}

fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    decode_value(parse_json(raw)?)
}

pub(crate) fn decode_value(value: Value) -> Result<Request, NamespaceError> {
    for key in [
        "",
        "/project",
        "/project/packages",
        "/project/graph",
        "/original",
        "/original/hashes",
        "/runtime",
        "/workspace",
        "/frozen",
        "/environment",
    ] {
        if !value.pointer(key).is_some_and(Value::is_object) {
            return Err(invalid("campaign request must use named JSON objects"));
        }
    }
    let mut request: Request =
        serde_json::from_value(value).map_err(|_| invalid("invalid native campaign request"))?;
    if request.operation != "campaign"
        || request.families.is_empty()
        || request.families.len() > FAMILIES.len()
        || request.max_mutants > 10000
        || request.mode == Mode::Direct && request.families.len() != 1
    {
        return Err(invalid("invalid native campaign configuration"));
    }
    absolute(&request.original.root)?;
    let mut seen = BTreeSet::new();
    for family in &request.families {
        if !FAMILIES.contains(&family.family.as_str()) || !seen.insert(&family.family) {
            return Err(invalid("duplicate or unsupported campaign family"));
        }
        rules(&family.rules, false)?;
        // Reuse the complete attempt/family preflight, without executing author code.
        family_request(&request, family, Vec::new())?;
    }
    request
        .families
        .sort_by_key(|family| FAMILIES.iter().position(|name| *name == family.family));
    Ok(request)
}

fn family_output(request: &Request, family: &Family) -> PathBuf {
    match request.mode {
        Mode::Direct => request.output.clone(),
        Mode::Profile => request.output.join("families").join(&family.family),
    }
}

fn family_request(
    request: &Request,
    family: &Family,
    candidates: Vec<Value>,
) -> Result<crate::family_execution::Request, NamespaceError> {
    let mut child = crate::family_execution::decode_value(json!({
        "operation":"family", "project":request.project, "output":family_output(request,family),
        "runtime":{"installation":request.runtime.installation,"runner":request.runtime.runner,
            "dartSdk":request.runtime.dart_sdk,"flutterSdk":request.runtime.flutter_sdk,
            "adapterRoot":request.runtime.adapter_root},
        "workspace":{"root":request.workspace.root,"campaign":request.workspace.campaign},
        "config":{"commandTimeout":family.config.command_timeout,"testTimeout":family.config.test_timeout},
        "candidates":candidates, "family":family.family,"workers":family.workers,
        "emptyPolicy":if request.mode==Mode::Direct {"controls"} else {"skip"},
        "frozen":request.frozen,"environment":request.environment,
        "retainedExpected":request.replay.as_ref().map(|replay|&replay.expected),
        "frameworkRoot":request.replay.as_ref().map(|replay|replay.framework())
    }))?;
    child.replay = request.replay.clone();
    Ok(child)
}

fn discovery_request(
    request: &Request,
    family: &Family,
) -> Result<crate::adapter_discovery::Request, NamespaceError> {
    let mut payload = json!({"workspaceRoot":request.project.root,"packageRoot":".",
        "dartSdkPath":request.runtime.dart_sdk,"rules":family.rules,
        "limits":{"maxMutants":if request.mode==Mode::Profile {10000} else {request.max_mutants}}});
    if request.project.flutter {
        payload["flutterSdkPath"] = json!(request.runtime.flutter_sdk);
    }
    crate::adapter_discovery::decode_value(json!({"operation":"discover","project":request.project,
        "directory":family_output(request,family).join("discovery"),"family":family.family,"payload":payload,
        "runtime":{"dartSdk":request.runtime.dart_sdk,"adapterRoot":request.runtime.adapter_root,
            "helper":request.runtime.runner.parent().ok_or_else(||invalid("runner has no parent"))?
                .join("failforge-adapter-campaign")}}))
}

fn validate_selection(value: &Value, family: &Family) -> Result<(), NamespaceError> {
    let operators = value["steps"]["handshake"]["operators"]
        .as_array()
        .ok_or_else(|| invalid("missing discovery operator catalogue"))?;
    let components = family
        .rules
        .get("components")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for selection in std::iter::once(&family.rules).chain(components) {
        for operator in selection
            .get("operators")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if !operators
                .iter()
                .any(|row| row["id"] == *operator && row["family"] == family.family)
            {
                return Err(invalid("operator does not belong to the selected family"));
            }
        }
    }
    let candidates = value["candidates"]
        .as_array()
        .ok_or_else(|| invalid("missing discovery candidates"))?;
    let mut ids = BTreeSet::new();
    for candidate in candidates {
        let id = candidate["mutant"]["id"]
            .as_str()
            .ok_or_else(|| invalid("missing discovered identity"))?;
        if candidate["mutant"]["family"] != family.family || !ids.insert(id) {
            return Err(invalid("mixed-family or duplicate discovered candidates"));
        }
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, NamespaceError> + Send + 'static,
) -> Result<T, NamespaceError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| invalid("campaign input operation failed"))?
}

struct Guards {
    project: DirectoryGuard,
    original: DirectoryGuard,
    output: DirectoryGuard,
    families: Vec<DirectoryGuard>,
}
impl Guards {
    fn verify(&self) -> Result<(), NamespaceError> {
        self.project.verify()?;
        self.original.verify()?;
        self.output.verify()?;
        for family in &self.families {
            family.verify()?;
        }
        Ok(())
    }
}

async fn prepare(
    request: Arc<Request>,
    caps: LifecycleCaps,
    cancel: Cancellation,
) -> Result<(Guards, LifecycleCaps), NamespaceError> {
    blocking(move || {
        cancel.check()?;
        caps.verify(
            &request.workspace.root,
            &request.workspace.campaign,
            &cancel,
        )?;
        let project = DirectoryGuard::open(&request.project.root)?;
        let original = DirectoryGuard::open(&request.original.root)?;
        if inspect_project(&request.project.root, &cancel)? != request.project {
            return Err(invalid("prepared campaign project changed"));
        }
        verify_source(&request.original.root, &request.original.hashes, &cancel)?;
        let output = DirectoryGuard::open(&request.output)?;
        let path = output.canonical_path()?;
        let mut protected = crate::namespace_planner::protected_inputs(
            &PlanProject {
                root: request.project.root.clone(),
                name: request.project.name.clone(),
                flutter: request.project.flutter,
            },
            &PlanRuntime {
                dart_sdk: request.runtime.dart_sdk.clone(),
                flutter_sdk: request.runtime.flutter_sdk.clone(),
                adapter_root: request.runtime.adapter_root.clone(),
            },
        )?;
        protected.push(request.workspace.root.canonicalize()?);
        protected.push(request.runtime.runner.canonicalize()?);
        if protected
            .iter()
            .any(|protected| path.starts_with(protected) || protected.starts_with(&path))
        {
            return Err(invalid("campaign report overlaps protected inputs"));
        }
        let original_path = original.canonical_path()?;
        if original_path.starts_with(&path)
            || path.starts_with(&original_path)
                && !path.starts_with(original_path.join(".failforge"))
        {
            return Err(invalid(
                "campaign output inside original must be under .failforge",
            ));
        }
        let mut families = Vec::new();
        for family in &request.families {
            let guard = DirectoryGuard::open(&family_output(&request, family))?;
            let family_path = guard.canonical_path()?;
            if !family_path.starts_with(&path)
                || request.mode == Mode::Profile && family_path == path
            {
                return Err(invalid("campaign family output escaped its report"));
            }
            families.push(guard);
        }
        Ok((
            Guards {
                project,
                original,
                output,
                families,
            },
            caps,
        ))
    })
    .await
}

async fn verify_inputs(request: Arc<Request>) -> Result<(), NamespaceError> {
    blocking(move || {
        if let Some(replay) = &request.replay {
            replay.verify()?;
        }
        let cancel = Cancellation::default();
        verify_source(&request.original.root, &request.original.hashes, &cancel)?;
        let actual = crate::workspace_inputs::bind_inputs(
            &request.runtime,
            &request.project,
            &request.environment,
            &cancel,
        )?;
        if actual != request.frozen {
            return Err(invalid(
                "SDK, dependency or snapshot inputs changed during campaign",
            ));
        }
        Ok(())
    })
    .await
}

enum Discovery {
    Fresh(Box<crate::adapter_discovery::OwnedDiscovery>),
    Retained(Value),
}

impl Discovery {
    fn value(&self) -> &Value {
        match self {
            Self::Fresh(owned) => &owned.value,
            Self::Retained(value) => value,
        }
    }
}

/// Shared campaign semantics; the enclosing lifecycle owns the wire commitment.
pub(crate) trait CampaignEvents {
    fn emit_campaign(&mut self, event: Value) -> io::Result<()>;
    fn finish_campaign(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed_families: usize,
        expected_families: usize,
        completed_mutants: usize,
        expected_mutants: Option<usize>,
    ) -> io::Result<()>;
}

impl CampaignEvents for Stream {
    fn emit_campaign(&mut self, event: Value) -> io::Result<()> {
        self.emit_campaign(event)
    }
    fn finish_campaign(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed_families: usize,
        expected_families: usize,
        completed_mutants: usize,
        expected_mutants: Option<usize>,
    ) -> io::Result<()> {
        self.finish_campaign(
            status,
            error,
            completed_families,
            expected_families,
            completed_mutants,
            expected_mutants,
        )
    }
}

fn stage(
    stream: &mut impl CampaignEvents,
    name: &str,
    family: Option<&str>,
    elapsed: Option<f64>,
) -> Result<(), NamespaceError> {
    stream.emit_campaign(match elapsed {
        None => json!({"kind":"stage_started","stage":name,"family":family}),
        Some(seconds) => {
            json!({"kind":"stage_finished","stage":name,"family":family,"wallSeconds":seconds})
        }
    })?;
    Ok(())
}

async fn verify_phase(
    request: Arc<Request>,
    stream: &mut impl CampaignEvents,
    name: &str,
    family: Option<&str>,
    cancel: &Cancellation,
) -> Result<(), NamespaceError> {
    cancel.check()?;
    stage(stream, name, family, None)?;
    let started = Instant::now();
    let result = verify_inputs(request).await;
    stage(stream, name, family, Some(started.elapsed().as_secs_f64()))?;
    result?;
    cancel.check()?;
    Ok(())
}

#[derive(Default)]
struct Ledger {
    expected: Option<usize>,
    mutants: usize,
    families: usize,
    infrastructure: bool,
}

struct FamilySink<'a, S> {
    stream: &'a mut S,
    ledger: &'a mut Ledger,
    family: &'a str,
    started: Instant,
    terminal: Option<(String, Option<String>)>,
}
impl<S: CampaignEvents> FamilyEvents for FamilySink<'_, S> {
    fn emit(&mut self, event: Value) -> io::Result<()> {
        let mutant = event["kind"] == "mutant";
        let infrastructure = mutant && event["record"]["outcome"] == "INFRA_ERROR";
        self.stream
            .emit_campaign(json!({"kind":"family_event","family":self.family,"event":event}))?;
        self.ledger.mutants += usize::from(mutant);
        self.ledger.infrastructure |= infrastructure;
        Ok(())
    }
    fn finish(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed: usize,
        total: usize,
    ) -> io::Result<()> {
        self.stream.emit_campaign(
            json!({"kind":"family_finished","family":self.family,"status":status,
            "error":error,"completedMutants":completed,"expectedMutants":total,
            "wallSeconds":self.started.elapsed().as_secs_f64()}),
        )?;
        self.ledger.families += usize::from(matches!(status, "completed" | "no_mutants"));
        self.terminal = Some((status.to_owned(), error.map(str::to_owned)));
        Ok(())
    }
}

async fn schedule(
    request: Arc<Request>,
    guards: &Guards,
    caps: &LifecycleCaps,
    cancel: &Cancellation,
    stream: &mut impl CampaignEvents,
    ledger: &mut Ledger,
) -> Result<(), NamespaceError> {
    let mut discoveries = Vec::new();
    for family in &request.families {
        cancel.check()?;
        guards.verify()?;
        stage(stream, "discovery_validation", Some(&family.family), None)?;
        let started = Instant::now();
        let discovery = async {
            if let Some(replay) = &request.replay {
                replay.verify()?;
                Ok(Discovery::Retained(replay.discovery.clone()))
            } else {
                let child = discovery_request(&request, family)?;
                let lease = WorkspaceLease::from_borrowed(
                    caps.lease
                        .as_ref()
                        .ok_or_else(|| invalid("missing campaign lease"))?
                        .as_fd(),
                )?;
                crate::adapter_discovery::run_owned(child, lease, cancel)
                    .await
                    .map(|owned| Discovery::Fresh(Box::new(owned)))
            }
        }
        .await;
        // Selection validation is part of discovery, before the result is exposed.
        let discovery = discovery.and_then(|owned| {
            validate_selection(owned.value(), family)?;
            guards.verify()?;
            Ok(owned)
        });
        // A frame-size preflight can fail without writing any bytes. Keep that
        // failure inside the phase so its balanced ending and failed terminal
        // still preserve the actual error. A broken pipe remains poisoned.
        let discovery = discovery.and_then(|owned| {
            stream.emit_campaign(
                json!({"kind":"discovery","family":family.family,"value":owned.value()}),
            )?;
            Ok(owned)
        });
        stage(
            stream,
            "discovery_validation",
            Some(&family.family),
            Some(started.elapsed().as_secs_f64()),
        )?;
        let discovery = discovery?;
        discoveries.push(discovery);
        if request.mode == Mode::Profile {
            verify_phase(
                Arc::clone(&request),
                stream,
                "verify_discovery_inputs",
                Some(&family.family),
                cancel,
            )
            .await?;
        }
    }
    let total = discoveries
        .iter()
        .map(|discovery| {
            discovery.value()["candidates"]
                .as_array()
                .map_or(0, Vec::len)
        })
        .sum();
    stream.emit_campaign(json!({"kind":"discovery_finished","expectedMutants":total}))?;
    ledger.expected = Some(total);
    if total > request.max_mutants {
        return Err(invalid(&format!(
            "profile mutant limit exceeded: discovered {total}, configured {}",
            request.max_mutants
        )));
    }
    for (family, discovery) in request.families.iter().zip(&discoveries) {
        cancel.check()?;
        guards.verify()?;
        if request.mode == Mode::Profile {
            verify_phase(
                Arc::clone(&request),
                stream,
                "verify_execution_inputs",
                Some(&family.family),
                cancel,
            )
            .await?;
        }
        let child = family_request(
            &request,
            family,
            discovery.value()["candidates"]
                .as_array()
                .ok_or_else(|| invalid("missing discovery candidates"))?
                .clone(),
        )?;
        let child_caps = caps.try_clone()?;
        stream.emit_campaign(json!({"kind":"family_started","family":family.family}))?;
        let mut sink = FamilySink {
            stream,
            ledger,
            family: &family.family,
            started: Instant::now(),
            terminal: None,
        };
        crate::family_execution::run_owned(child, child_caps, cancel, &mut sink).await?;
        let (status, error) = sink
            .terminal
            .ok_or_else(|| invalid("family did not publish completion"))?;
        match status.as_str() {
            "completed" | "no_mutants" => {}
            "cancelled" => {
                return Err(NamespaceError::Cancelled(
                    cancel.signum().unwrap_or(libc::SIGINT),
                ));
            }
            _ => return Err(invalid(error.as_deref().unwrap_or("native family failed"))),
        }
        verify_phase(
            Arc::clone(&request),
            stream,
            "verify_inputs",
            if request.mode == Mode::Profile {
                Some(&family.family)
            } else {
                None
            },
            cancel,
        )
        .await?;
    }
    if request.mode == Mode::Profile {
        verify_phase(
            Arc::clone(&request),
            stream,
            "verify_profile_inputs",
            None,
            cancel,
        )
        .await?;
    }
    Ok(())
}

fn terminal(
    stream: &mut impl CampaignEvents,
    result: Result<(), NamespaceError>,
    cancel: &Cancellation,
    ledger: &Ledger,
    expected_families: usize,
) -> Result<(), NamespaceError> {
    let (status, error) = terminal_state(result, cancel, ledger, expected_families);
    stream.finish_campaign(
        status,
        error.as_deref(),
        ledger.families,
        expected_families,
        ledger.mutants,
        ledger.expected,
    )?;
    Ok(())
}

fn terminal_state(
    result: Result<(), NamespaceError>,
    cancel: &Cancellation,
    ledger: &Ledger,
    expected_families: usize,
) -> (&'static str, Option<String>) {
    if ledger.infrastructure {
        (
            "failed",
            Some("infrastructure failure during campaign; see records".to_owned()),
        )
    } else {
        match result {
            Err(NamespaceError::Cancelled(_)) => ("cancelled", None),
            Err(error) => ("failed", Some(error.to_string())),
            Ok(()) if cancel.signum().is_some() => ("cancelled", None),
            Ok(())
                if ledger.expected != Some(ledger.mutants)
                    || ledger.families != expected_families =>
            {
                (
                    "failed",
                    Some("campaign lacks complete family results".to_owned()),
                )
            }
            Ok(()) if ledger.expected == Some(0) => ("no_mutants", None),
            Ok(()) => ("completed", None),
        }
    }
}

pub(crate) async fn run_owned(
    request: Arc<Request>,
    caps: LifecycleCaps,
    cancel: &Cancellation,
    stream: &mut impl CampaignEvents,
) -> Result<(), NamespaceError> {
    let expected_families = request.families.len();
    let mut ledger = Ledger::default();
    let (guards, caps) = match prepare(Arc::clone(&request), caps, cancel.clone()).await {
        Ok(value) => value,
        Err(error) => return terminal(stream, Err(error), cancel, &ledger, expected_families),
    };
    let mut result = verify_inputs(Arc::clone(&request)).await;
    if result.is_ok() {
        result = schedule(
            Arc::clone(&request),
            &guards,
            &caps,
            cancel,
            stream,
            &mut ledger,
        )
        .await;
    }
    // These checks deliberately ignore cancellation, after every started owner drained.
    let bindings = verify_inputs(Arc::clone(&request)).await;
    let retained = blocking(move || {
        guards.verify()?;
        caps.verify(
            &request.workspace.root,
            &request.workspace.campaign,
            &Cancellation::default(),
        )?;
        Ok((guards, caps))
    })
    .await;
    let retained = match retained {
        Ok(value) => value,
        Err(error) => return terminal(stream, Err(error), cancel, &ledger, expected_families),
    };
    if let Err(error) = bindings {
        result = Err(error);
    }
    let published = terminal(stream, result, cancel, &ledger, expected_families);
    drop(retained);
    published
}

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
        listening.request(
            tokio::select! {_=interrupt.recv()=>libc::SIGINT,_=terminate.recv()=>libc::SIGTERM},
        );
    });
    let result = async {
        let mut stream = Stream::stdout()?;
        if let Some(ready) = ready {
            ready.acknowledge()?;
        }
        run_owned(Arc::new(request), caps, &cancel, &mut stream).await
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
                return Err(invalid("unexpected campaign capability"));
            }
        }
        let fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing campaign request"))?;
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
#[path = "campaign_execution_tests.rs"]
mod tests;
