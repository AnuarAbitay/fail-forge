//! One task owns each child from successful spawn through group cleanup and reap.
use std::collections::BTreeMap;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;

use crate::output::{OutputSession, OutputSnapshot, StreamReceiver};
use crate::platform::ManagedChild;
use crate::{
    CancelReason, CleanupReport, CommandResult, CommandSpec, CompletionCause, GroupState,
    RunTimings, RunnerError, SignalOutcome, SignalRecord,
};

const POLL: Duration = Duration::from_millis(5);
const MAX_RECORDS: usize = 64;
pub type RunOutcome = Result<Arc<CommandResult>, Arc<RunnerError>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct Control {
    request: Option<CancelReason>,
    force: bool,
    selected: bool,
}

impl Control {
    fn select(
        &mut self,
        completed: bool,
        failed: bool,
        output_failed: bool,
        timed_out: bool,
    ) -> Option<CompletionCause> {
        if self.selected {
            return None;
        }
        // The same mutex protects request and selection; freezing is atomic.
        let cause = if completed && !failed {
            Some(CompletionCause::ProcessCompleted)
        } else if let Some(reason) = self.request {
            Some(CompletionCause::Cancelled(reason))
        } else if failed || output_failed {
            Some(CompletionCause::RunnerFailure)
        } else if timed_out {
            Some(CompletionCause::TimedOut)
        } else {
            None
        };
        self.selected = cause.is_some();
        cause
    }
}

struct Record {
    control: Mutex<Control>,
    result: watch::Sender<Option<RunOutcome>>,
    shutdown_budget: Duration,
    observed: AtomicBool,
    resources_complete: AtomicBool,
}

impl Record {
    fn request(&self, reason: CancelReason, force: bool) -> bool {
        let mut control = lock(&self.control);
        control.force |= force;
        if control.selected || control.request.is_some() {
            return false;
        }
        control.request = Some(reason);
        true
    }

    fn publish(&self, outcome: RunOutcome) {
        let result = match &outcome {
            Ok(result) => Some(result.as_ref()),
            Err(error) => error.partial_result(),
        };
        let resources_complete = result.is_some_and(|result| {
            result.cleanup.child_reaped
                && result.stdout.writer_finished
                && result.stderr.writer_finished
                && matches!(result.cleanup.group, GroupState::NoLiveMembers { .. })
        });
        self.resources_complete
            .store(resources_complete, Ordering::Release);
        self.result.send_replace(Some(outcome));
    }

    fn retain_failure(&self) -> bool {
        self.result
            .borrow()
            .as_ref()
            .is_some_and(|outcome| outcome.is_err())
            && (!self.observed.load(Ordering::Acquire)
                || !self.resources_complete.load(Ordering::Acquire))
    }
}

#[derive(Default)]
struct Registry {
    closing: bool,
    runs: BTreeMap<u64, Arc<Record>>,
    failures: BTreeMap<u64, Arc<Record>>,
}

impl Registry {
    fn prune_acknowledged(&mut self) {
        self.failures.retain(|_, record| record.retain_failure());
    }
}

/// Removes a pre-spawn reservation on every error or unwind. After spawn the
/// supervisor guard takes this duty, together with the process ownership.
struct PreparationReservation {
    registry: Arc<Mutex<Registry>>,
    id: u64,
    armed: bool,
}

impl Drop for PreparationReservation {
    fn drop(&mut self) {
        if self.armed {
            lock(&self.registry).runs.remove(&self.id);
        }
    }
}

/// Reuse a runner in an existing Tokio runtime. It never creates a runtime.
pub struct Runner {
    registry: Arc<Mutex<Registry>>,
    next_id: AtomicU64,
    #[cfg(test)]
    preparation_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    pub fn new() -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry::default())),
            next_id: AtomicU64::new(1),
            #[cfg(test)]
            preparation_hook: Mutex::new(None),
        }
    }

    /// Preparation/spawn are synchronous OS operations. Registry locks cover
    /// only bookkeeping; filesystem calls never hold the accounting mutex.
    /// Once spawn succeeds, an unwind guard owns publication and accounting.
    pub fn start(&self, spec: CommandSpec) -> Result<RunHandle, RunnerError> {
        self.start_inner(spec, None)
    }

    /// Start with a caller-owned input descriptor instead of null stdin.
    ///
    /// The descriptor is transferred to the child. The caller owns feeding and
    /// closing its other endpoint; Runner still exclusively owns process cleanup.
    /// This supports bounded protocols without introducing another process owner.
    pub fn start_with_stdin(
        &self,
        spec: CommandSpec,
        input: std::fs::File,
    ) -> Result<RunHandle, RunnerError> {
        self.start_inner(spec, Some(input))
    }

    fn start_inner(
        &self,
        spec: CommandSpec,
        input: Option<std::fs::File>,
    ) -> Result<RunHandle, RunnerError> {
        spec.validate().map_err(RunnerError::InvalidSpec)?;
        let id = self.allocate_id()?;
        let runtime = validate_runtime()?;
        let shutdown_budget = checked_budget(&spec)?;
        let (result, receiver) = watch::channel(None);
        let record = Arc::new(Record {
            control: Mutex::new(Control::default()),
            result,
            shutdown_budget,
            observed: AtomicBool::new(false),
            resources_complete: AtomicBool::new(false),
        });
        {
            let mut registry = lock(&self.registry);
            registry.prune_acknowledged();
            if registry.closing {
                return Err(RunnerError::ShuttingDown);
            }
            let active = registry.runs.len() + registry.failures.len();
            if active >= MAX_RECORDS {
                return Err(RunnerError::RunLimitReached { active });
            }
            registry.runs.insert(id, record.clone());
        }
        let mut reservation = PreparationReservation {
            registry: self.registry.clone(),
            id,
            armed: true,
        };
        #[cfg(test)]
        if let Some(hook) = lock(&self.preparation_hook).clone() {
            hook();
        }

        let preparation_start = Instant::now();
        let invocation = std::env::current_dir()
            .map_err(|error| RunnerError::preparation("invocation cwd", error))?;
        let mut command = crate::prepare::prepare(&spec, &invocation)
            .map_err(|error| RunnerError::preparation("command preparation", error))?;
        if let Some(input) = input {
            command.stdin(std::process::Stdio::from(input));
        }
        let mut output = OutputSession::prepare(&spec.capture_policy, &invocation)
            .map_err(|error| RunnerError::preparation("capture preparation", error))?;
        let preparation = preparation_start.elapsed();
        // Shutdown before this check prevents spawn. Shutdown after this check
        // reaches the already registered record and is honored by supervisor.
        if lock(&self.registry).closing || lock(&record.control).request.is_some() {
            return Err(RunnerError::ShuttingDown);
        }
        let spawn_start = Instant::now();
        let mut child = ManagedChild::spawn(&mut command)
            .map_err(|error| RunnerError::preparation("spawn", error))?;
        let spawned = Instant::now();
        let spawn = spawn_start.elapsed();
        let mut guard = SupervisorGuard {
            registry: self.registry.clone(),
            record: record.clone(),
            id,
            capture: output.snapshotter(),
            cause: CompletionCause::RunnerFailure,
            status: None,
            cleanup: CleanupReport::default(),
            timings: RunTimings {
                preparation,
                spawn,
                ..Default::default()
            },
            published: false,
        };
        reservation.armed = false;
        let pipes = child.take_stdout().zip(child.take_stderr());
        let (stream, setup_error) = match pipes {
            Some((stdout, stderr)) => match output.begin(stdout, stderr) {
                Ok(stream) => (stream, None),
                Err(error) => (None, Some(error)),
            },
            None => (None, Some(io::Error::other("capture pipes unavailable"))),
        };
        // A private task and its guard retain ownership even if a wait future
        // or public handle is dropped. No raw Child or abort handle is exposed.
        runtime.spawn(async move {
            supervise(child, output, spec, spawned, setup_error, &mut guard).await;
        });
        Ok(RunHandle {
            record,
            receiver,
            stream,
        })
    }

    /// Unobserved errors and incomplete ownership are reported even after a
    /// handle was dropped. Acknowledged errors with closed resources are pruned.
    pub async fn shutdown(&self) -> Result<(), RunnerError> {
        let budget = {
            let mut registry = lock(&self.registry);
            registry.closing = true;
            registry.prune_acknowledged();
            for record in registry.runs.values() {
                record.request(CancelReason::Shutdown, false);
            }
            registry
                .runs
                .values()
                .map(|record| record.shutdown_budget)
                .max()
                .unwrap_or(POLL)
        };
        let deadline = Instant::now()
            .checked_add(budget)
            .ok_or(RunnerError::TimeBudgetOutOfRange)?;
        loop {
            {
                let mut registry = lock(&self.registry);
                registry.prune_acknowledged();
                let active = registry.runs.len();
                if active == 0 {
                    return if registry.failures.is_empty() {
                        Ok(())
                    } else {
                        Err(RunnerError::ShutdownFailed {
                            runs: registry.failures.len(),
                        })
                    };
                }
                if Instant::now() >= deadline {
                    return Err(RunnerError::ShutdownIncomplete { active });
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub fn active_runs(&self) -> usize {
        lock(&self.registry).runs.len()
    }

    fn allocate_id(&self) -> Result<u64, RunnerError> {
        let mut id = self.next_id.load(Ordering::Relaxed);
        loop {
            let next = id.checked_add(1).ok_or_else(|| {
                RunnerError::preparation(
                    "run identifier allocation",
                    io::Error::other("run identifier space exhausted"),
                )
            })?;
            match self
                .next_id
                .compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(id),
                Err(actual) => id = actual,
            }
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        let mut registry = lock(&self.registry);
        registry.closing = true;
        for record in registry.runs.values() {
            record.request(CancelReason::Shutdown, false);
        }
    }
}

pub struct RunHandle {
    record: Arc<Record>,
    receiver: watch::Receiver<Option<RunOutcome>>,
    stream: Option<StreamReceiver>,
}

impl RunHandle {
    /// True means a first request was accepted before selection. Observed
    /// completion has priority and can still win the subsequent supervisor poll.
    pub fn cancel(&self, reason: CancelReason) -> bool {
        self.record.request(reason, false)
    }
    /// Escalation remains effective after cause selection; the boolean reports
    /// only whether a new first cancellation reason was recorded.
    pub fn force_cancel(&self, reason: CancelReason) -> bool {
        self.record.request(reason, true)
    }
    pub fn take_stream(&mut self) -> Option<StreamReceiver> {
        self.stream.take()
    }
    /// Repeated waits return the same shared outcome. Dropping this future
    /// leaves both the supervisor and the stored result intact.
    pub async fn wait(&mut self) -> RunOutcome {
        loop {
            if let Some(result) = self.receiver.borrow().clone() {
                self.record.observed.store(true, Ordering::Release);
                return result;
            }
            let _ = self.receiver.changed().await;
        }
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        self.record.request(CancelReason::HandleDropped, false);
    }
}

fn validate_runtime() -> Result<tokio::runtime::Handle, RunnerError> {
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| RunnerError::NoRuntime)?;
    // Tokio 1.53 has no public driver introspection. Probe the operations before
    // spawn, catch this boundary, and never replace the global panic hook.
    let probe = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        drop(tokio::time::sleep(Duration::ZERO));
        let (stream, _peer) = std::os::unix::net::UnixStream::pair()?;
        stream.set_nonblocking(true)?;
        drop(AsyncFd::with_interest(stream, Interest::READABLE)?);
        Ok(())
    }));
    match probe {
        Ok(Ok(())) => Ok(runtime),
        Ok(Err(error)) => Err(RunnerError::preparation("runtime I/O probe", error)),
        Err(_) => Err(RunnerError::NoRuntime),
    }
}

fn checked_budget(spec: &CommandSpec) -> Result<Duration, RunnerError> {
    let policy = &spec.timeout_policy;
    let budget = policy
        .grace_period
        .checked_add(policy.forced_wait)
        .and_then(|value| value.checked_add(policy.output_drain))
        .and_then(|value| value.checked_add(Duration::from_secs(1)))
        .ok_or(RunnerError::TimeBudgetOutOfRange)?;
    let now = Instant::now();
    if now.checked_add(budget).is_none()
        || policy
            .timeout
            .is_some_and(|value| now.checked_add(value).is_none())
    {
        return Err(RunnerError::TimeBudgetOutOfRange);
    }
    Ok(budget)
}

/// Exists before output setup and travels inside the private supervisor task.
/// Unwind/runtime-abort publishes bounded capture and an honest partial result.
/// ManagedChild's independent Drop owns emergency OS signalling/reaping.
struct SupervisorGuard {
    registry: Arc<Mutex<Registry>>,
    record: Arc<Record>,
    id: u64,
    capture: OutputSnapshot,
    cause: CompletionCause,
    status: Option<std::process::ExitStatus>,
    cleanup: CleanupReport,
    timings: RunTimings,
    published: bool,
}

impl SupervisorGuard {
    fn publish(&mut self, outcome: RunOutcome) {
        self.record.publish(outcome);
        self.published = true;
    }
}

impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        if !self.published {
            self.cleanup
                .errors
                .push("supervisor stopped before publishing its result".into());
            let (stdout, stderr) = self.capture.capture();
            let result = CommandResult {
                cause: self.cause,
                status: self.status,
                timings: self.timings.clone(),
                stdout,
                stderr,
                cleanup: self.cleanup.clone(),
            };
            self.publish(Err(Arc::new(RunnerError::Partial {
                operation: "supervisor interruption",
                partial: Box::new(result),
            })));
        }
        let mut registry = lock(&self.registry);
        registry.runs.remove(&self.id);
        if self.record.retain_failure() {
            registry.failures.insert(self.id, self.record.clone());
        }
    }
}

async fn supervise(
    mut child: ManagedChild,
    mut output: OutputSession,
    spec: CommandSpec,
    spawned: Instant,
    setup_error: Option<io::Error>,
    guard: &mut SupervisorGuard,
) {
    tracing::debug!(pid = child.id(), "supervisor started");
    let timeout = spec
        .timeout_policy
        .timeout
        .and_then(|value| spawned.checked_add(value));
    let mut failed = setup_error.is_some();
    if let Some(error) = setup_error {
        record_error(&mut guard.cleanup, "output setup", &error);
    }
    let cause = loop {
        let completed = match child.observe() {
            Ok(completed) => completed,
            Err(error) => {
                record_error(&mut guard.cleanup, "observe", &error);
                failed = true;
                false
            }
        };
        let output_failed = output.failure().is_some();
        let timed_out = timeout.is_some_and(|deadline| Instant::now() >= deadline);
        let selected =
            lock(&guard.record.control).select(completed, failed, output_failed, timed_out);
        if let Some(cause) = selected {
            break cause;
        }
        tokio::time::sleep(POLL).await;
    };
    guard.cause = cause;
    guard.timings.execution = spawned.elapsed();
    let cleanup_start = Instant::now();
    let initial_signal = match cause {
        CompletionCause::Cancelled(CancelReason::CtrlC) => libc::SIGINT,
        _ => libc::SIGTERM,
    };
    let mut group = cleanup_group(&child, &mut guard.cleanup);
    let mut observed = cleanup_observe(&mut child, &mut guard.cleanup);
    let needs_signal = !observed || !matches!(group, GroupState::NoLiveMembers { .. });
    if needs_signal {
        send_signal(&mut child, initial_signal, spawned, &mut guard.cleanup);
    }
    let grace_deadline = Instant::now()
        .checked_add(spec.timeout_policy.grace_period)
        .unwrap_or_else(Instant::now);
    while needs_signal && Instant::now() < grace_deadline && !lock(&guard.record.control).force {
        observed = cleanup_observe(&mut child, &mut guard.cleanup);
        group = cleanup_group(&child, &mut guard.cleanup);
        if observed && matches!(group, GroupState::NoLiveMembers { .. }) {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    observed = cleanup_observe(&mut child, &mut guard.cleanup);
    group = cleanup_group(&child, &mut guard.cleanup);
    if !observed || !matches!(group, GroupState::NoLiveMembers { .. }) {
        send_signal(&mut child, libc::SIGKILL, spawned, &mut guard.cleanup);
        let forced_deadline = Instant::now()
            .checked_add(spec.timeout_policy.forced_wait)
            .unwrap_or_else(Instant::now);
        loop {
            observed = cleanup_observe(&mut child, &mut guard.cleanup);
            group = cleanup_group(&child, &mut guard.cleanup);
            if observed && matches!(group, GroupState::NoLiveMembers { .. }) {
                break;
            }
            if Instant::now() >= forced_deadline {
                guard
                    .cleanup
                    .errors
                    .push("forced cleanup deadline expired".into());
                break;
            }
            tokio::time::sleep(POLL).await;
        }
    }
    guard.cleanup.group = group;
    // No group observation/signal can occur after reap retires the PGID.
    if observed {
        match child.reap() {
            Ok(status) => {
                guard.status = Some(status);
                guard.cleanup.child_reaped = true;
            }
            Err(error) => record_error(&mut guard.cleanup, "reap", &error),
        }
    }
    guard.timings.cleanup = cleanup_start.elapsed();
    let drain_start = Instant::now();
    let (stdout, stderr) = output.finish(spec.timeout_policy.output_drain).await;
    guard.timings.output_drain = drain_start.elapsed();
    guard.cleanup.output_finished = stdout.eof
        && stderr.eof
        && stdout.writer_finished
        && stderr.writer_finished
        && stdout.issue.is_none()
        && stderr.issue.is_none();
    failed |= stdout.issue.is_some() || stderr.issue.is_some() || !guard.cleanup.is_complete();
    let result = CommandResult {
        cause,
        status: guard.status,
        timings: guard.timings.clone(),
        stdout,
        stderr,
        cleanup: guard.cleanup.clone(),
    };
    let outcome = if failed {
        Err(Arc::new(RunnerError::Partial {
            operation: "execution or cleanup",
            partial: Box::new(result),
        }))
    } else {
        Ok(Arc::new(result))
    };
    guard.publish(outcome);
    // Keep observing/reaping the direct child while the runtime can schedule
    // its sole owner; the cleanup deadline never creates a competing waiter.
    while !child.is_retired() {
        match child.observe() {
            Ok(true) => {
                let _ = child.reap();
            }
            Err(_) if child.is_retired() => break,
            _ => {}
        }
        if !child.is_retired() {
            tokio::time::sleep(POLL).await;
        }
    }
}

fn record_error(cleanup: &mut CleanupReport, operation: &str, error: &io::Error) {
    let message = format!("{operation}: {:?}", error.kind());
    // Persistent failed probes must not append another message on every poll.
    if !cleanup.errors.contains(&message) {
        cleanup.errors.push(message);
    }
}

fn cleanup_observe(child: &mut ManagedChild, cleanup: &mut CleanupReport) -> bool {
    match child.observe() {
        Ok(observed) => observed,
        Err(error) => {
            record_error(cleanup, "observe during cleanup", &error);
            false
        }
    }
}

fn cleanup_group(child: &ManagedChild, cleanup: &mut CleanupReport) -> GroupState {
    match child.group_state() {
        Ok(group) => group,
        Err(error) => {
            record_error(cleanup, "group observation", &error);
            GroupState::Unknown
        }
    }
}

fn send_signal(
    child: &mut ManagedChild,
    signal: i32,
    spawned: Instant,
    cleanup: &mut CleanupReport,
) {
    let outcome = match child.signal(signal) {
        Ok(true) => SignalOutcome::Sent,
        Ok(false) => SignalOutcome::AlreadyGone,
        Err(error) => {
            record_error(cleanup, "signal", &error);
            SignalOutcome::Failed {
                errno: error.raw_os_error().unwrap_or(0),
            }
        }
    };
    cleanup.signals.push(SignalRecord {
        signal,
        after_spawn: spawned.elapsed(),
        outcome,
    });
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
mod tests;
