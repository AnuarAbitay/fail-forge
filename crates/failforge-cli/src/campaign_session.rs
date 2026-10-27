//! One owner from original-project preflight through the final workspace close.
use std::future::Future;
use std::io;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[cfg(unix)]
use crate::signals::{SignalKind, signal};
use failforge_dart::namespace::NamespaceError;
use failforge_workspace::Cancellation;
use serde_json::{Value, json};

use crate::campaign_execution::CampaignEvents;
use crate::campaign_progress::{State as ProgressState, Writer as ProgressWriter};
use crate::campaign_workspace::{self as workspace, Preflight, Request, Workspace};
use crate::dart_namespace::invalid;
#[cfg(any(unix, test))]
use crate::family_stream::{FrameWriter, Stream};

type Owner = Arc<Mutex<Option<Workspace>>>;

/// Shared in-process events; the diagnostic wire is only one possible sink.
pub(crate) trait SessionEvents {
    fn bind_output(
        &mut self,
        _directory: &crate::family_execution::DirectoryGuard,
    ) -> io::Result<()> {
        Ok(())
    }
    fn emit_session(&mut self, event: Value) -> io::Result<()>;
    fn finish_session(&mut self, status: &str, error: Option<&str>) -> io::Result<()>;
}

#[cfg(any(unix, test))]
impl<W: FrameWriter> SessionEvents for Stream<W> {
    fn emit_session(&mut self, event: Value) -> io::Result<()> {
        Stream::emit_session(self, event)
    }
    fn finish_session(&mut self, status: &str, error: Option<&str>) -> io::Result<()> {
        Stream::finish_session(self, status, error)
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, NamespaceError> + Send + 'static,
) -> Result<T, NamespaceError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| invalid("campaign lifecycle operation failed"))?
}

fn stage(stream: &mut dyn SessionEvents, name: &str, seconds: Option<f64>) -> io::Result<()> {
    stream.emit_session(match seconds {
        None => json!({"kind":"stage_started","stage":name}),
        Some(seconds) => {
            json!({"kind":"stage_finished","stage":name,"wallSeconds":seconds})
        }
    })
}

async fn phase(
    stream: &mut dyn SessionEvents,
    name: &str,
    cancel: &Cancellation,
    work: impl Future<Output = Result<Value, NamespaceError>>,
) -> Result<Value, NamespaceError> {
    cancel.check()?;
    stage(stream, name, None)?;
    let begin = Instant::now();
    // A clean frame-size rejection must still balance this phase. Owned state
    // has already been retained outside this future before metadata is emitted.
    let result = work.await.and_then(|event| {
        stream.emit_session(event.clone())?;
        Ok(event)
    });
    stage(stream, name, Some(begin.elapsed().as_secs_f64()))?;
    result
}

#[derive(Debug)]
struct Completion {
    status: String,
    error: Option<String>,
}

struct CampaignSink<'a> {
    stream: &'a mut dyn SessionEvents,
    completion: Option<Completion>,
    progress: &'a mut LiveProgress,
    cancel: &'a Cancellation,
}

impl CampaignEvents for CampaignSink<'_> {
    fn emit_campaign(&mut self, event: Value) -> io::Result<()> {
        self.stream
            .emit_session(json!({"kind":"campaign_event","event":event}))?;
        // A reporting failure requests cancellation but must not poison the
        // event sink: sealed results from already-started workers still drain.
        self.progress.observe(&event, self.cancel);
        Ok(())
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
        self.stream.emit_session(json!({
            "kind":"campaign_finished","status":status,"error":error,
            "completedFamilies":completed_families,"expectedFamilies":expected_families,
            "completedMutants":completed_mutants,"expectedMutants":expected_mutants
        }))?;
        self.completion = Some(Completion {
            status: status.to_owned(),
            error: error.map(str::to_owned),
        });
        Ok(())
    }
}

struct LiveProgress {
    state: ProgressState,
    writer: ProgressWriter,
    started: Instant,
    error: Option<String>,
}

impl LiveProgress {
    fn new(
        preflight: &Preflight,
        report: &crate::family_execution::DirectoryGuard,
    ) -> Result<Self, NamespaceError> {
        let families = preflight
            .families
            .iter()
            .map(|family| family.family.clone())
            .collect::<Vec<_>>();
        Ok(Self {
            state: ProgressState::new(
                preflight.mode.as_str(),
                &preflight.run_id,
                &preflight.output,
                &families,
            )?,
            writer: ProgressWriter::new(&preflight.output, report.descriptor()?)?,
            started: Instant::now(),
            error: None,
        })
    }

    fn fail(&mut self, error: io::Error, cancel: &Cancellation) {
        self.error
            .get_or_insert_with(|| format!("campaign progress publication failed: {error}"));
        cancel.request(libc::SIGINT);
    }

    fn observe(&mut self, event: &Value, cancel: &Cancellation) {
        match self.state.observe(event) {
            Ok(true) if self.error.is_none() => self.write(None, cancel),
            Ok(_) => {}
            Err(error) => self.fail(error, cancel),
        }
    }

    fn write(&mut self, status: Option<&str>, cancel: &Cancellation) {
        if self.error.is_some() {
            return;
        }
        let result = self
            .state
            .snapshot(self.started.elapsed().as_secs_f64(), status)
            .and_then(|value| self.writer.write(&value));
        if let Err(error) = result {
            self.fail(error, cancel);
        }
    }
}

fn runtime_value(preflight: &Preflight) -> Value {
    let runtime = &preflight.runtime;
    json!({"installation":runtime.installation,"runner":runtime.runner,
        "dartSdk":runtime.dart_sdk,"flutterSdk":runtime.flutter_sdk,
        "adapterRoot":runtime.adapter_root})
}

fn selections(preflight: &Preflight) -> Vec<Value> {
    preflight
        .families
        .iter()
        .map(|family| family.to_value())
        .collect()
}

async fn campaign(
    request: Arc<Request>,
    preflight: Arc<Preflight>,
    owner: Owner,
    cancel: &Cancellation,
    stream: &mut dyn SessionEvents,
    progress: &mut LiveProgress,
) -> Result<Completion, NamespaceError> {
    let preparing = Arc::clone(&preflight);
    let retained = Arc::clone(&owner);
    let working = cancel.clone();
    phase(
        stream,
        "workspace_recovery",
        cancel,
        blocking(move || {
            let workspace = Workspace::start(&preparing, &working)?;
            let recovery = workspace.recovery();
            let event = json!({"kind":"workspace","directory":workspace.directory(),
            "recovered":recovery["recovered"],"skipped":recovery["skipped"]});
            // Retain ownership before a metadata write can fail or cancel.
            *retained.lock().unwrap_or_else(|error| error.into_inner()) = Some(workspace);
            Ok(event)
        }),
    )
    .await?;

    let preparing = Arc::clone(&preflight);
    let retained = Arc::clone(&owner);
    let working = cancel.clone();
    let snapshot = phase(
        stream,
        "snapshot",
        cancel,
        blocking(move || {
            let mut guard = retained.lock().unwrap_or_else(|error| error.into_inner());
            let workspace = guard
                .as_mut()
                .ok_or_else(|| invalid("missing campaign workspace"))?;
            let project = workspace.snapshot(&preparing, &working)?;
            Ok(json!({"kind":"snapshot","project":project}))
        }),
    )
    .await?;

    let preparing = Arc::clone(&preflight);
    let retained = Arc::clone(&owner);
    let environment = request.environment.clone();
    let working = cancel.clone();
    let bindings = phase(
        stream,
        "bind_inputs",
        cancel,
        blocking(move || {
            let guard = retained.lock().unwrap_or_else(|error| error.into_inner());
            let workspace = guard
                .as_ref()
                .ok_or_else(|| invalid("missing campaign workspace"))?;
            let value = workspace.bind_inputs(&preparing, &environment, &working)?;
            Ok(json!({"kind":"bindings","value":value}))
        }),
    )
    .await?;

    let (caps, root, name) = {
        let guard = owner.lock().unwrap_or_else(|error| error.into_inner());
        let workspace = guard
            .as_ref()
            .ok_or_else(|| invalid("missing campaign workspace"))?;
        (
            workspace.caps()?,
            workspace.root().to_owned(),
            workspace.campaign().to_owned(),
        )
    };
    let mut inner = crate::campaign_execution::decode_value(json!({
        "operation":"campaign","mode":request.mode.as_str(),
        "project":snapshot["project"],
        "original":{"root":preflight.original.root,"hashes":preflight.hashes},
        "output":preflight.output,"runtime":runtime_value(&preflight),
        "workspace":{"root":root,"campaign":name},
        "environment":request.environment,"frozen":bindings["value"],
        "maxMutants":request.max_mutants,"families":selections(&preflight)
    }))?;
    inner.replay = preflight.replay.clone();
    let mut sink = CampaignSink {
        stream,
        completion: None,
        progress,
        cancel,
    };
    crate::campaign_execution::run_owned(Arc::new(inner), caps, cancel, &mut sink).await?;
    sink.completion
        .ok_or_else(|| invalid("campaign did not publish completion"))
}

/// Cleanup is not a normal cancellable phase: neither a signal nor a broken
/// progress pipe may bypass it. Both operations are joined before publication.
async fn finish(
    preflight: Arc<Preflight>,
    owner: Owner,
    stream: &mut dyn SessionEvents,
) -> Result<Value, NamespaceError> {
    let mut wire_error = stage(stream, "workspace_cleanup", None).err();
    let begin = Instant::now();
    let retained = Arc::clone(&owner);
    let preparing = Arc::clone(&preflight);
    let result = blocking(move || {
        let workspace = retained
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let directory = workspace.as_ref().map(Workspace::directory);
        let mut value = match workspace {
            Some(workspace) => serde_json::to_value(workspace.finish(&preparing))
                .map_err(|_| invalid("invalid campaign cleanup receipt"))?,
            None => json!({"originalUnchanged":true,"snapshotCleaned":true,
                "cleanupDeferred":false,"errors":[],"skipped":[]}),
        };
        value["workspaceDirectory"] = json!(directory);
        Ok(value)
    })
    .await;
    if let Err(error) = stage(
        stream,
        "workspace_cleanup",
        Some(begin.elapsed().as_secs_f64()),
    ) {
        wire_error.get_or_insert(error);
    }

    if let Err(error) = stage(stream, "verify_original", None) {
        wire_error.get_or_insert(error);
    }
    let begin = Instant::now();
    let original = blocking(move || workspace::verify_original(&preflight)).await;
    if let Err(error) = stage(
        stream,
        "verify_original",
        Some(begin.elapsed().as_secs_f64()),
    ) {
        wire_error.get_or_insert(error);
    }
    let mut cleanup = result?;
    if let Err(error) = original {
        cleanup["originalUnchanged"] = false.into();
        cleanup["errors"]
            .as_array_mut()
            .ok_or_else(|| invalid("invalid cleanup errors"))?
            .push(error.to_string().into());
    }
    if let Some(error) = wire_error {
        return Err(error.into());
    }
    cleanup["kind"] = "cleanup".into();
    stream.emit_session(cleanup.clone())?;
    Ok(cleanup)
}

fn outcome(
    result: Result<Completion, NamespaceError>,
    cleanup: &Value,
    cancel: &Cancellation,
) -> Completion {
    if cleanup["originalUnchanged"] != true
        || cleanup["snapshotCleaned"] != true
        || cleanup["cleanupDeferred"] != false
        || cleanup["errors"]
            .as_array()
            .is_none_or(|errors| !errors.is_empty())
    {
        return Completion {
            status: "failed".into(),
            error: Some(format!(
                "campaign cleanup or source verification failed: {}",
                cleanup["errors"]
            )),
        };
    }
    match result {
        Err(NamespaceError::Cancelled(_)) => Completion {
            status: "cancelled".into(),
            error: None,
        },
        Err(error) => Completion {
            status: "failed".into(),
            error: Some(error.to_string()),
        },
        Ok(result) if result.status == "failed" => result,
        Ok(_) if cancel.signum().is_some() => Completion {
            status: "cancelled".into(),
            error: None,
        },
        Ok(result) => result,
    }
}

fn with_progress_failure(
    result: Result<Completion, NamespaceError>,
    message: &str,
) -> Result<Completion, NamespaceError> {
    // Reporting must prevent success and supersede its own cancellation, while
    // an established execution failure keeps its original diagnostic.
    match result {
        Ok(completion) if completion.status == "failed" => Ok(completion),
        Err(error) if !matches!(error, NamespaceError::Cancelled(_)) => Err(error),
        _ => Err(invalid(message)),
    }
}

pub(crate) async fn run_owned(
    request: Arc<Request>,
    cancel: &Cancellation,
    stream: &mut dyn SessionEvents,
) -> Result<(), NamespaceError> {
    let preparing = Arc::clone(&request);
    let working = cancel.clone();
    let preflight = Arc::new(blocking(move || workspace::preflight(&preparing, &working)).await?);
    cancel.check()?;
    let preparing = Arc::clone(&preflight);
    let report = blocking(move || workspace::create_output(&preparing)).await?;
    stream.bind_output(&report)?;
    let mut prepared = json!({"kind":"prepared","runId":preflight.run_id,
        "output":preflight.output,"original":preflight.original,"originalHashes":preflight.hashes,
        "runtime":runtime_value(&preflight),"families":selections(&preflight)});
    if let Some(replay) = &preflight.replay {
        prepared["retainedReplay"] = replay.binding();
    }
    stream.emit_session(prepared)?;

    let owner = Arc::new(Mutex::new(None));
    let mut progress = LiveProgress::new(&preflight, &report);
    let mut result = match &mut progress {
        Ok(progress) => {
            campaign(
                request,
                Arc::clone(&preflight),
                Arc::clone(&owner),
                cancel,
                stream,
                progress,
            )
            .await
        }
        Err(error) => Err(invalid(&format!(
            "campaign progress publication failed: {error}"
        ))),
    };
    if let Ok(progress) = &progress {
        if let Some(error) = &progress.error {
            result = with_progress_failure(result, error);
        }
    }
    // Retain the report inode through cleanup and terminal publication. An
    // altered output path may not be used as evidence of successful completion.
    if let Err(error) = report.verify() {
        result = Err(error);
    }
    let cleanup = finish(Arc::clone(&preflight), owner, stream).await?;
    if let Err(error) = report.verify() {
        result = Err(error);
    }
    let mut completion = outcome(result, &cleanup, cancel);
    if let Ok(progress) = &mut progress {
        progress.write(Some(&completion.status), cancel);
        if let Some(error) = &progress.error {
            completion.status = "failed".into();
            // Preserve a more specific execution/cleanup failure if present.
            completion.error.get_or_insert_with(|| error.clone());
        }
    }
    stream.finish_session(&completion.status, completion.error.as_deref())?;
    drop(report);
    Ok(())
}

#[cfg(unix)]
async fn process(
    request: Request,
    ready: Option<crate::readiness::Readiness>,
) -> Result<(), NamespaceError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancel = Cancellation::default();
    let listening = cancel.clone();
    let listener = tokio::spawn(async move {
        listening.request(tokio::select! {
            _=interrupt.recv()=>libc::SIGINT, _=terminate.recv()=>libc::SIGTERM
        });
    });
    let result = async {
        let mut stream = Stream::stdout()?;
        if let Some(ready) = ready {
            ready.acknowledge()?;
        }
        run_owned(Arc::new(request), &cancel, &mut stream).await
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
            crate::workspace_handoff::CAPS_ENV,
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected session capability"));
            }
        }
        let fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing campaign session request"))?;
        let request = workspace::decode(&crate::private_request::read(&fd)?)?;
        let ready = crate::readiness::Readiness::from_env()?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(process(request, ready))
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
#[path = "campaign_session_tests.rs"]
mod tests;
