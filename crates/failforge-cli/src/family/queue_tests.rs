use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{Barrier, mpsc, oneshot};

#[derive(Default)]
struct Counts {
    active: AtomicUsize,
    peak: AtomicUsize,
    cleaned: AtomicUsize,
}
struct Active(Arc<Counts>);
impl Active {
    fn new(counts: Arc<Counts>) -> Self {
        let active = counts.active.fetch_add(1, Ordering::SeqCst) + 1;
        counts.peak.fetch_max(active, Ordering::SeqCst);
        Self(counts)
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.cleaned.fetch_add(1, Ordering::SeqCst);
    }
}

async fn started(receiver: &mut mpsc::UnboundedReceiver<usize>) -> usize {
    tokio::time::timeout(Duration::from_secs(3), receiver.recv())
        .await
        .expect("worker did not start")
        .expect("worker start channel closed")
}
async fn finished(task: tokio::task::JoinHandle<QueueSummary>) -> QueueSummary {
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("queue did not drain")
        .expect("queue owner panicked")
}

#[tokio::test]
async fn queue_admission_is_bounded_and_sink_observes_completion_order() {
    let counts = Arc::new(Counts::default());
    let records = Arc::new(Mutex::new(Vec::new()));
    let (start_tx, mut start_rx) = mpsc::unbounded_channel();
    let (senders, receivers): (Vec<_>, Vec<_>) = (0..5).map(|_| oneshot::channel()).unzip();
    let mut senders = senders.into_iter().map(Some).collect::<Vec<_>>();
    let observed = Arc::clone(&records);
    let counted = Arc::clone(&counts);
    let task = tokio::spawn(async move {
        run(
            receivers,
            2,
            &Cancellation::default(),
            move |index, gate| {
                let counts = Arc::clone(&counted);
                let started = start_tx.clone();
                async move {
                    let _active = Active::new(counts);
                    started.send(index).unwrap();
                    gate.await.unwrap();
                    Ok(index * 10)
                }
            },
            move |index, value| {
                observed.lock().unwrap().push((index, value));
                Ok(())
            },
        )
        .await
    });
    let mut first = vec![started(&mut start_rx).await, started(&mut start_rx).await];
    first.sort_unstable();
    assert_eq!(first, vec![0, 1]);
    assert!(start_rx.try_recv().is_err());
    senders[1].take().unwrap().send(()).unwrap();
    assert_eq!(started(&mut start_rx).await, 2);
    senders[2].take().unwrap().send(()).unwrap();
    assert_eq!(started(&mut start_rx).await, 3);
    senders[0].take().unwrap().send(()).unwrap();
    assert_eq!(started(&mut start_rx).await, 4);
    senders[3].take().unwrap().send(()).unwrap();
    senders[4].take().unwrap().send(()).unwrap();
    let summary = finished(task).await;
    assert!(summary.error.is_none());
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (5, 5, 5, 5)
    );
    let observed = records.lock().unwrap();
    assert_eq!(&observed[..3], &[(1, 10), (2, 20), (0, 0)]);
    let mut ids = observed.iter().map(|(index, _)| *index).collect::<Vec<_>>();
    ids.sort_unstable();
    assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    assert_eq!(counts.peak.load(Ordering::SeqCst), 2);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn an_already_queued_failure_blocks_refill_when_success_is_consumed_first() {
    for panicked in [false, true] {
        let cancel = Cancellation::default();
        let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
        let (release, wait) = oneshot::channel::<()>();
        let mut wait = Some(wait);
        let mut queue = Box::pin(run(
            0..5,
            2,
            &cancel,
            move |index, _| {
                let finished = finished_tx.clone();
                let wait = (index == 1).then(|| wait.take().unwrap());
                async move {
                    if let Some(wait) = wait {
                        wait.await.unwrap();
                    }
                    finished.send(index).unwrap();
                    if index == 1 {
                        assert!(!panicked, "injected queued panic");
                        Err(invalid("queued worker error"))
                    } else {
                        Ok(index)
                    }
                }
            },
            |_, _| Ok(()),
        ));
        // Dispatch once, then deliberately leave the owner unpolled until the
        // successful task and the failing task are both queued for joining.
        std::future::poll_fn(|context| {
            assert!(queue.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(started(&mut finished_rx).await, 0);
        release.send(()).unwrap();
        assert_eq!(started(&mut finished_rx).await, 1);
        let summary = queue.await;
        assert!(matches!(summary.error, Some(NamespaceError::Invalid(_))));
        assert_eq!(
            (
                summary.dispatched,
                summary.joined,
                summary.completed,
                summary.emitted
            ),
            (2, 2, 1, 1),
            "queued failure allowed another admission; panic={panicked}"
        );
    }
}

#[tokio::test]
async fn cancellation_retains_late_sealed_results_without_admitting_pending_work() {
    let cancel = Cancellation::default();
    let owning_cancel = cancel.clone();
    let counts = Arc::new(Counts::default());
    let counted = Arc::clone(&counts);
    let records = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&records);
    let (start_tx, mut start_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let child_cancel = owning_cancel.clone();
        run(
            0..7,
            2,
            &owning_cancel,
            move |index, _| {
                let cancel = child_cancel.clone();
                let started = start_tx.clone();
                let counts = Arc::clone(&counted);
                async move {
                    let _active = Active::new(counts);
                    started.send(index).unwrap();
                    cancel.cancelled().await;
                    Ok(index)
                }
            },
            move |index, value| {
                assert_eq!(index, value);
                observed.lock().unwrap().push(value);
                Ok(())
            },
        )
        .await
    });
    started(&mut start_rx).await;
    started(&mut start_rx).await;
    cancel.request(libc::SIGTERM);
    let summary = finished(task).await;
    assert!(matches!(
        summary.error,
        Some(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (2, 2, 2, 2)
    );
    assert_eq!(records.lock().unwrap().len(), 2);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn infrastructure_failure_wins_over_an_earlier_cancelled_worker() {
    let cancel = Cancellation::default();
    let counts = Arc::new(Counts::default());
    let counted = Arc::clone(&counts);
    let records = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&records);
    let (start_tx, mut start_rx) = mpsc::unbounded_channel();
    let (release, wait) = oneshot::channel::<()>();
    let mut wait = Some(wait);
    let task = tokio::spawn(async move {
        let child_cancel = cancel.clone();
        run(
            0..8,
            3,
            &cancel,
            move |index, _| {
                let cancel = child_cancel.clone();
                let started = start_tx.clone();
                let counts = Arc::clone(&counted);
                let wait = (index == 0).then(|| wait.take().unwrap());
                async move {
                    let _active = Active::new(counts);
                    started.send(index).unwrap();
                    match index {
                        0 => {
                            wait.unwrap().await.unwrap();
                            Err(NamespaceError::Cancelled(libc::SIGTERM))
                        }
                        1 => {
                            cancel.cancelled().await;
                            Err(invalid("worker failed while stopping"))
                        }
                        _ => {
                            cancel.cancelled().await;
                            Ok(index)
                        }
                    }
                }
            },
            move |index, _| {
                observed.lock().unwrap().push(index);
                Ok(())
            },
        )
        .await
    });
    for _ in 0..3 {
        started(&mut start_rx).await;
    }
    release.send(()).unwrap();
    let summary = finished(task).await;
    assert!(
        matches!(&summary.error, Some(NamespaceError::Invalid(message)) if message == "worker failed while stopping")
    );
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (3, 3, 1, 1)
    );
    assert_eq!(*records.lock().unwrap(), vec![2]);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn failed_sink_is_not_retried_but_every_active_task_is_drained() {
    let counts = Arc::new(Counts::default());
    let counted = Arc::clone(&counts);
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&calls);
    let (start_tx, mut start_rx) = mpsc::unbounded_channel();
    let (release, wait) = oneshot::channel::<()>();
    let mut wait = Some(wait);
    let task = tokio::spawn(async move {
        let cancel = Cancellation::default();
        let child_cancel = cancel.clone();
        run(
            0..8,
            3,
            &cancel,
            move |index, _| {
                let cancel = child_cancel.clone();
                let started = start_tx.clone();
                let counts = Arc::clone(&counted);
                let wait = (index == 0).then(|| wait.take().unwrap());
                async move {
                    let _active = Active::new(counts);
                    started.send(index).unwrap();
                    if let Some(wait) = wait {
                        wait.await.unwrap();
                    } else {
                        cancel.cancelled().await;
                    }
                    Ok(index)
                }
            },
            move |_, _| {
                called.fetch_add(1, Ordering::SeqCst);
                Err(invalid("stream closed"))
            },
        )
        .await
    });
    for _ in 0..3 {
        started(&mut start_rx).await;
    }
    release.send(()).unwrap();
    let summary = finished(task).await;
    assert!(
        matches!(&summary.error, Some(NamespaceError::Invalid(message)) if message == "stream closed")
    );
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (3, 3, 3, 0)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn panicked_worker_is_infrastructure_failure_and_other_workers_are_joined() {
    let counts = Arc::new(Counts::default());
    let counted = Arc::clone(&counts);
    let barrier = Arc::new(Barrier::new(2));
    let cancel = Cancellation::default();
    let child_cancel = cancel.clone();
    let summary = run(
        0..5,
        2,
        &cancel,
        move |index, _| {
            let cancel = child_cancel.clone();
            let counts = Arc::clone(&counted);
            let barrier = Arc::clone(&barrier);
            async move {
                let _active = Active::new(counts);
                barrier.wait().await;
                if index == 0 {
                    panic!("injected worker failure");
                }
                cancel.cancelled().await;
                Ok(index)
            }
        },
        |_, _| Ok(()),
    )
    .await;
    assert!(
        matches!(&summary.error, Some(NamespaceError::Invalid(message)) if message == "native family worker panicked")
    );
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (2, 2, 1, 1)
    );
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn panicked_factory_cannot_unwind_past_already_dispatched_work() {
    let counts = Arc::new(Counts::default());
    let counted = Arc::clone(&counts);
    let cancel = Cancellation::default();
    let child_cancel = cancel.clone();
    let summary = run(
        0..5,
        3,
        &cancel,
        move |index, _| {
            assert_ne!(index, 1, "injected factory failure");
            let cancel = child_cancel.clone();
            let counts = Arc::clone(&counted);
            async move {
                let _active = Active::new(counts);
                cancel.cancelled().await;
                Ok(index)
            }
        },
        |_, _| Ok(()),
    )
    .await;
    assert!(
        matches!(&summary.error, Some(NamespaceError::Invalid(message)) if message == "native family task factory panicked")
    );
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (1, 1, 1, 1)
    );
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn panicked_sink_stops_delivery_and_drains_remaining_tasks() {
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&calls);
    let counts = Arc::new(Counts::default());
    let counted = Arc::clone(&counts);
    let cancel = Cancellation::default();
    let child_cancel = cancel.clone();
    let barrier = Arc::new(Barrier::new(2));
    let summary = run(
        0..5,
        2,
        &cancel,
        move |index, _| {
            let cancel = child_cancel.clone();
            let counts = Arc::clone(&counted);
            let barrier = Arc::clone(&barrier);
            async move {
                let _active = Active::new(counts);
                barrier.wait().await;
                if index != 0 {
                    cancel.cancelled().await;
                }
                Ok(index)
            }
        },
        move |_, _| {
            called.fetch_add(1, Ordering::SeqCst);
            panic!("injected sink failure")
        },
    )
    .await;
    assert!(
        matches!(&summary.error, Some(NamespaceError::Invalid(message)) if message == "native family result sink panicked")
    );
    assert_eq!(
        (
            summary.dispatched,
            summary.joined,
            summary.completed,
            summary.emitted
        ),
        (2, 2, 2, 0)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert_eq!(counts.cleaned.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn empty_input_zero_workers_and_preexisting_cancellation_start_no_tasks() {
    let empty = run(
        Vec::<usize>::new(),
        2,
        &Cancellation::default(),
        |_, _| async { Ok::<_, NamespaceError>(()) },
        |_, _| Ok(()),
    )
    .await;
    assert!(empty.error.is_none());
    assert_eq!(
        (
            empty.dispatched,
            empty.joined,
            empty.completed,
            empty.emitted
        ),
        (0, 0, 0, 0)
    );
    let zero = run(
        0..3,
        0,
        &Cancellation::default(),
        |_, _| async {
            panic!("invalid queue started worker");
            #[allow(unreachable_code)]
            Ok::<_, NamespaceError>(())
        },
        |_, _| Ok(()),
    )
    .await;
    assert!(matches!(zero.error, Some(NamespaceError::Invalid(_))));
    assert_eq!(zero.dispatched, 0);
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    let stopped = run(
        0..3,
        2,
        &cancel,
        |_, _| async {
            panic!("cancelled queue started worker");
            #[allow(unreachable_code)]
            Ok::<_, NamespaceError>(())
        },
        |_, _| Ok(()),
    )
    .await;
    assert!(matches!(
        stopped.error,
        Some(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    assert_eq!(
        (
            stopped.dispatched,
            stopped.joined,
            stopped.completed,
            stopped.emitted
        ),
        (0, 0, 0, 0)
    );
}
