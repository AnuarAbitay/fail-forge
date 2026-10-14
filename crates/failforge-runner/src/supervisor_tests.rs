use super::*;
use crate::{CaptureIssue, CapturePolicy, CaptureSummary, EnvSpec, TimeoutPolicy};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;

fn spec() -> CommandSpec {
    CommandSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 0".into()],
        cwd: ".".into(),
        env: EnvSpec::default(),
        timeout_policy: TimeoutPolicy {
            grace_period: Duration::ZERO,
            forced_wait: Duration::ZERO,
            output_drain: Duration::from_secs(1),
            ..Default::default()
        },
        capture_policy: CapturePolicy::default(),
    }
}

fn new_record() -> (Arc<Record>, watch::Receiver<Option<RunOutcome>>) {
    let (sender, receiver) = watch::channel(None);
    (
        Arc::new(Record {
            control: Mutex::new(Control::default()),
            result: sender,
            shutdown_budget: Duration::from_secs(1),
            observed: AtomicBool::new(false),
            resources_complete: AtomicBool::new(false),
        }),
        receiver,
    )
}

fn result() -> CommandResult {
    CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: Some(std::process::ExitStatus::from_raw(0)),
        timings: RunTimings::default(),
        stdout: CaptureSummary {
            writer_finished: true,
            ..Default::default()
        },
        stderr: CaptureSummary {
            writer_finished: true,
            ..Default::default()
        },
        cleanup: CleanupReport {
            child_reaped: true,
            group: GroupState::NoLiveMembers { zombies: 1 },
            output_finished: true,
            ..Default::default()
        },
    }
}

fn partial(result: CommandResult) -> RunOutcome {
    Err(Arc::new(RunnerError::Partial {
        operation: "injected failure",
        partial: Box::new(result),
    }))
}

fn ended_socket(bytes: &[u8]) -> OwnedFd {
    let (read, mut write) = std::os::unix::net::UnixStream::pair().unwrap();
    write.write_all(bytes).unwrap();
    drop(write);
    read.into()
}

fn guard(runner: &Runner, record: Arc<Record>, id: u64, output: &OutputSession) -> SupervisorGuard {
    lock(&runner.registry).runs.insert(id, record.clone());
    SupervisorGuard {
        registry: runner.registry.clone(),
        record,
        id,
        capture: output.snapshotter(),
        cause: CompletionCause::RunnerFailure,
        status: None,
        cleanup: CleanupReport::default(),
        timings: RunTimings::default(),
        published: false,
    }
}

#[test]
fn driver_probe_rejects_missing_drivers_before_preparation_or_spawn() {
    assert!(matches!(validate_runtime(), Err(RunnerError::NoRuntime)));
    for (io, time) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        if io {
            builder.enable_io();
        }
        if time {
            builder.enable_time();
        }
        let runtime = builder.build().unwrap();
        let runner = Runner::new();
        let entered = runtime.enter();
        let mut missing = spec();
        missing.program = "/failforge-runtime-probe-must-precede-exec".into();
        let outcome = runner.start(missing);
        if io && time {
            assert!(matches!(outcome, Err(RunnerError::Preparation { .. })));
        } else {
            assert!(matches!(outcome, Err(RunnerError::NoRuntime)));
        }
        assert_eq!(runner.active_runs(), 0);
        drop(entered);
    }
}

#[test]
fn full_runtime_runs_and_shutdown_reaps_normally() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runner = Runner::default();
    let mut handle = {
        let _entered = runtime.enter();
        runner.start(spec()).unwrap()
    };
    runtime.block_on(async {
        let result = tokio::time::timeout(Duration::from_secs(3), handle.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.status.unwrap().code(), Some(0));
        assert!(result.cleanup.is_complete());
        runner.shutdown().await.unwrap();
    });
}

#[test]
fn shutdown_without_time_driver_is_an_unsupported_context_and_keeps_admission_closed() {
    // This is a boundary probe outside the documented enabled-runtime usage.
    // Keep the owner runtime alive while polling shutdown in a different
    // runtime without a timer driver. A synthetic pre-spawn reservation makes
    // the timer path deterministic without starting an unbounded OS process.
    let owner = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let caller = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let runner = Runner::new();
    let (record, _) = new_record();
    lock(&runner.registry).runs.insert(42, record.clone());
    let outcome = catch_unwind(AssertUnwindSafe(|| caller.block_on(runner.shutdown())));
    // Tokio panics when sleep is constructed without a timer driver. This is
    // an observed unsupported-context limitation, not a supported API defect.
    assert!(outcome.is_err());
    assert!(lock(&runner.registry).closing);
    assert_eq!(lock(&record.control).request, Some(CancelReason::Shutdown));
    lock(&runner.registry).runs.clear();
    owner.block_on(runner.shutdown()).unwrap();
}

#[test]
fn every_selection_precedence_case_freezes_under_the_request_mutex() {
    for completed in [false, true] {
        for failed in [false, true] {
            for output_failed in [false, true] {
                for timed_out in [false, true] {
                    for request in [None, Some(CancelReason::CtrlC)] {
                        let mut control = Control {
                            request,
                            ..Default::default()
                        };
                        let expected = if completed && !failed {
                            Some(CompletionCause::ProcessCompleted)
                        } else if let Some(reason) = request {
                            Some(CompletionCause::Cancelled(reason))
                        } else if failed || output_failed {
                            Some(CompletionCause::RunnerFailure)
                        } else if timed_out {
                            Some(CompletionCause::TimedOut)
                        } else {
                            None
                        };
                        assert_eq!(
                            control.select(completed, failed, output_failed, timed_out),
                            expected
                        );
                        assert_eq!(control.selected, expected.is_some());
                        if expected.is_some() {
                            assert!(control.select(false, false, false, false).is_none());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn first_request_is_preserved_and_force_survives_selected_cause() {
    let (record, _) = new_record();
    assert!(record.request(CancelReason::User, false));
    assert!(!record.request(CancelReason::Shutdown, false));
    assert_eq!(
        lock(&record.control).select(false, false, false, false),
        Some(CompletionCause::Cancelled(CancelReason::User))
    );
    assert!(!record.request(CancelReason::CtrlC, true));
    assert!(lock(&record.control).force);
    assert_eq!(lock(&record.control).request, Some(CancelReason::User));
}

#[test]
fn accepted_request_can_lose_to_completion_but_no_request_is_accepted_after_freeze() {
    let (record, _) = new_record();
    assert!(record.request(CancelReason::User, false));
    assert_eq!(
        lock(&record.control).select(true, false, true, true),
        Some(CompletionCause::ProcessCompleted)
    );
    assert!(!record.request(CancelReason::Shutdown, false));
}

#[test]
fn concurrent_request_and_selection_have_only_the_documented_outcomes() {
    for _ in 0..64 {
        let (record, _) = new_record();
        let requester = record.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let other_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            other_barrier.wait();
            requester.request(CancelReason::User, false)
        });
        barrier.wait();
        let selected = lock(&record.control)
            .select(false, false, false, true)
            .unwrap();
        let accepted = worker.join().unwrap();
        match selected {
            CompletionCause::Cancelled(CancelReason::User) => assert!(accepted),
            CompletionCause::TimedOut => assert!(!accepted),
            _ => panic!("unexpected decision"),
        }
        assert!(lock(&record.control).selected);
    }
}

#[tokio::test]
async fn wait_is_cancellation_safe_repeated_and_acknowledges_a_partial_result() {
    let (record, receiver) = new_record();
    let mut handle = RunHandle {
        record: record.clone(),
        receiver,
        stream: None,
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(5), handle.wait())
            .await
            .is_err()
    );
    assert!(!record.observed.load(Ordering::Acquire));
    let outcome = partial(result());
    let expected = outcome.as_ref().err().unwrap().clone();
    record.publish(outcome);
    let first = handle.wait().await.unwrap_err();
    let second = handle.wait().await.unwrap_err();
    assert!(Arc::ptr_eq(&first, &expected));
    assert!(Arc::ptr_eq(&first, &second));
    assert!(record.observed.load(Ordering::Acquire));
    assert!(!record.retain_failure());
    assert!(handle.take_stream().is_none());
    assert!(handle.cancel(CancelReason::User));
    assert!(!handle.force_cancel(CancelReason::CtrlC));
    assert!(lock(&record.control).force);
}

#[test]
fn handle_and_runner_drop_request_cleanup_without_losing_registered_records() {
    let runner = Runner::new();
    let (record, receiver) = new_record();
    lock(&runner.registry).runs.insert(1, record.clone());
    let handle = RunHandle {
        record: record.clone(),
        receiver,
        stream: None,
    };
    drop(handle);
    assert_eq!(
        lock(&record.control).request,
        Some(CancelReason::HandleDropped)
    );
    let registry = runner.registry.clone();
    drop(runner);
    assert!(lock(&registry).closing);
    assert_eq!(lock(&registry).runs.len(), 1);
    assert_eq!(
        lock(&record.control).request,
        Some(CancelReason::HandleDropped)
    );
}

#[tokio::test]
async fn preparation_errors_release_reservations_and_success_can_follow() {
    let runner = Runner::new();
    for badcwd in [false, true] {
        let mut missing = spec();
        if badcwd {
            missing.cwd = "/failforge-no-such-working-directory".into();
        } else {
            missing.program = "/failforge-no-such-executable".into();
        }
        assert!(matches!(
            runner.start(missing),
            Err(RunnerError::Preparation { .. })
        ));
        assert_eq!(runner.active_runs(), 0);
    }
    let mut handle = runner.start(spec()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), handle.wait())
            .await
            .unwrap()
            .unwrap()
            .status
            .unwrap()
            .code(),
        Some(0)
    );
    runner.shutdown().await.unwrap();
}

struct ReleasePreparation(Option<std::sync::mpsc::Sender<()>>);
impl Drop for ReleasePreparation {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_can_request_cancellation_while_preparation_is_blocked() {
    let runner = Arc::new(Runner::new());
    let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
    let entered_sender = Mutex::new(Some(entered_sender));
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let release_receiver = Mutex::new(release_receiver);
    *lock(&runner.preparation_hook) = Some(Arc::new(move || {
        if let Some(sender) = lock(&entered_sender).take() {
            let _ = sender.send(());
        }
        let _ = lock(&release_receiver).recv();
    }));
    let mut release = ReleasePreparation(Some(release_sender));
    let starter = runner.clone();
    let runtime = tokio::runtime::Handle::current();
    let worker = std::thread::spawn(move || {
        let _entered = runtime.enter();
        starter.start(spec())
    });
    tokio::time::timeout(Duration::from_secs(1), entered_receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runner.active_runs(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), runner.shutdown())
            .await
            .is_err()
    );
    {
        let registry = lock(&runner.registry);
        assert!(registry.closing);
        assert_eq!(
            lock(&registry.runs.values().next().unwrap().control).request,
            Some(CancelReason::Shutdown)
        );
    }
    release.0.take().unwrap().send(()).unwrap();
    assert!(matches!(
        worker.join().unwrap(),
        Err(RunnerError::ShuttingDown)
    ));
    assert_eq!(runner.active_runs(), 0);
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_reports_unobserved_failures_and_acknowledgement_clears_closed_resources() {
    let runner = Runner::new();
    let (record, receiver) = new_record();
    let mut captured = result();
    captured.stdout.issue = Some(CaptureIssue::FileQuotaExceeded);
    captured.cleanup.output_finished = false;
    record.publish(partial(captured));
    lock(&runner.registry).failures.insert(1, record.clone());
    assert!(matches!(
        runner.shutdown().await,
        Err(RunnerError::ShutdownFailed { runs: 1 })
    ));
    let mut handle = RunHandle {
        record,
        receiver,
        stream: None,
    };
    assert!(handle.wait().await.is_err());
    runner.shutdown().await.unwrap();
    assert!(lock(&runner.registry).failures.is_empty());
}

#[tokio::test]
async fn acknowledgement_cannot_hide_unfinished_native_resources() {
    for missing in 0..4 {
        let runner = Runner::new();
        let (record, receiver) = new_record();
        let mut captured = result();
        match missing {
            0 => captured.cleanup.child_reaped = false,
            1 => captured.cleanup.group = GroupState::Unknown,
            2 => captured.stdout.writer_finished = false,
            _ => captured.stderr.writer_finished = false,
        }
        record.publish(partial(captured));
        lock(&runner.registry).failures.insert(1, record.clone());
        let mut handle = RunHandle {
            record,
            receiver,
            stream: None,
        };
        assert!(handle.wait().await.is_err());
        assert!(matches!(
            runner.shutdown().await,
            Err(RunnerError::ShutdownFailed { runs: 1 })
        ));
        assert_eq!(lock(&runner.registry).failures.len(), 1);
    }
}

#[tokio::test]
async fn retained_failure_capacity_is_bounded_and_acknowledged_records_are_pruned() {
    let runner = Runner::new();
    for id in 0..MAX_RECORDS as u64 {
        let (record, _) = new_record();
        record.publish(partial(result()));
        lock(&runner.registry).failures.insert(id, record);
    }
    assert!(matches!(
        runner.start(spec()),
        Err(RunnerError::RunLimitReached {
            active: MAX_RECORDS
        })
    ));
    assert_eq!(runner.active_runs(), 0);
    for record in lock(&runner.registry).failures.values() {
        record.observed.store(true, Ordering::Release);
    }
    let mut handle = runner.start(spec()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), handle.wait())
            .await
            .unwrap()
            .is_ok()
    );
    assert!(lock(&runner.registry).failures.is_empty());
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn preparation_reservation_drop_and_handoff_have_distinct_accounting() {
    let runner = Runner::new();
    let (record, _) = new_record();
    lock(&runner.registry).runs.insert(42, record);
    drop(PreparationReservation {
        registry: runner.registry.clone(),
        id: 42,
        armed: true,
    });
    assert_eq!(runner.active_runs(), 0);
    let (record, _) = new_record();
    lock(&runner.registry).runs.insert(43, record);
    drop(PreparationReservation {
        registry: runner.registry.clone(),
        id: 43,
        armed: false,
    });
    assert_eq!(runner.active_runs(), 1);
    lock(&runner.registry).runs.clear();
}

#[tokio::test]
async fn supervisor_unwind_preserves_original_cause_status_and_available_raw_capture() {
    let runner = Runner::new();
    let (record, mut receiver) = new_record();
    let mut output = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    output
        .begin(ended_socket(&[0, 255, 128]), ended_socket(b"stderr"))
        .unwrap();
    output.finish(Duration::from_secs(1)).await;
    let mut guard = guard(&runner, record.clone(), 42, &output);
    guard.cause = CompletionCause::TimedOut;
    guard.status = Some(std::process::ExitStatus::from_raw(0));
    guard.cleanup.child_reaped = true;
    guard.cleanup.group = GroupState::NoLiveMembers { zombies: 1 };
    assert!(
        catch_unwind(AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("injected supervisor panic");
        }))
        .is_err()
    );
    receiver.changed().await.unwrap();
    let outcome = receiver.borrow().clone().unwrap();
    let error = outcome.unwrap_err();
    let captured = error.partial_result().unwrap();
    assert_eq!(captured.cause, CompletionCause::TimedOut);
    assert_eq!(captured.status.unwrap().code(), Some(0));
    assert_eq!(captured.stdout.tail, [0, 255, 128]);
    assert_eq!(captured.stderr.tail, b"stderr");
    assert_eq!(runner.active_runs(), 0);
    assert_eq!(lock(&runner.registry).failures.len(), 1);
}

#[tokio::test]
async fn aborting_the_private_task_guard_publishes_a_partial_result_instead_of_hanging_wait() {
    let runner = Runner::new();
    let (record, receiver) = new_record();
    let output = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    let guard = guard(&runner, record.clone(), 42, &output);
    let task = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut handle = RunHandle {
        record,
        receiver,
        stream: None,
    };
    let error = tokio::time::timeout(Duration::from_secs(1), handle.wait())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.partial_result().unwrap().cause,
        CompletionCause::RunnerFailure
    );
    assert_eq!(runner.active_runs(), 0);
    assert!(matches!(
        runner.shutdown().await,
        Err(RunnerError::ShutdownFailed { runs: 1 })
    ));
}

#[tokio::test]
async fn normal_guard_publication_is_not_overwritten_on_drop() {
    let runner = Runner::new();
    let (record, receiver) = new_record();
    let output = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    let mut guard = guard(&runner, record, 1, &output);
    let captured = Arc::new(result());
    guard.publish(Ok(captured.clone()));
    drop(guard);
    assert!(Arc::ptr_eq(
        receiver.borrow().as_ref().unwrap().as_ref().unwrap(),
        &captured
    ));
    assert_eq!(runner.active_runs(), 0);
    assert!(lock(&runner.registry).failures.is_empty());
}

#[test]
fn time_budget_addition_and_clock_overflow_are_rejected() {
    let mut configuration = spec();
    assert_eq!(
        checked_budget(&configuration).unwrap(),
        Duration::from_secs(2)
    );
    configuration.timeout_policy.grace_period = Duration::MAX;
    assert!(matches!(
        checked_budget(&configuration),
        Err(RunnerError::TimeBudgetOutOfRange)
    ));
    configuration.timeout_policy.grace_period = Duration::ZERO;
    configuration.timeout_policy.forced_wait = Duration::MAX;
    assert!(checked_budget(&configuration).is_err());
    configuration.timeout_policy.forced_wait = Duration::ZERO;
    configuration.timeout_policy.output_drain = Duration::MAX;
    assert!(checked_budget(&configuration).is_err());
    configuration.timeout_policy.output_drain = Duration::ZERO;
    configuration.timeout_policy.timeout = Some(Duration::MAX);
    assert!(checked_budget(&configuration).is_err());
}

#[test]
fn repeated_probe_errors_are_bounded_and_do_not_include_secret_text() {
    let mut cleanup = CleanupReport::default();
    for _ in 0..1000 {
        record_error(&mut cleanup, "group", &io::Error::other("SECRET_ARG"));
    }
    assert_eq!(cleanup.errors.len(), 1);
    record_error(
        &mut cleanup,
        "observe",
        &io::Error::from(io::ErrorKind::Interrupted),
    );
    assert_eq!(cleanup.errors.len(), 2);
    assert!(!format!("{:?}", cleanup.errors).contains("SECRET"));
}

#[test]
fn identifier_allocation_is_checked_and_never_overwrites_an_existing_record() {
    let runner = Runner::new();
    let (record, _) = new_record();
    lock(&runner.registry).runs.insert(1, record.clone());
    runner.next_id.store(u64::MAX - 1, Ordering::Relaxed);
    assert_eq!(runner.allocate_id().unwrap(), u64::MAX - 1);
    assert!(matches!(
        runner.start(spec()),
        Err(RunnerError::Preparation {
            operation: "run identifier allocation",
            kind: io::ErrorKind::Other,
            os_error: None,
        })
    ));
    assert_eq!(runner.next_id.load(Ordering::Relaxed), u64::MAX);
    assert_eq!(runner.active_runs(), 1);
    assert!(Arc::ptr_eq(
        lock(&runner.registry).runs.get(&1).unwrap(),
        &record
    ));
}

#[test]
fn concurrent_identifier_allocations_are_unique() {
    let runner = Arc::new(Runner::new());
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let runner = runner.clone();
            std::thread::spawn(move || {
                (0..128)
                    .map(|_| runner.allocate_id().unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut ids: Vec<_> = threads
        .into_iter()
        .flat_map(|thread| thread.join().unwrap())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 1024);
    assert_eq!(ids.first(), Some(&1));
    assert_eq!(ids.last(), Some(&1024));
}
