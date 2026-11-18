use super::super::file::FILE_WORKERS;
use super::super::test_support::{CAPTURE_TESTS, TestDirectory, ended_pipe, pipe};
use super::*;
use std::fs::File;
use std::io::Write;
use std::sync::atomic::Ordering;

struct BlockedWriter {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
}

impl Write for BlockedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
            self.release.recv().unwrap();
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn blocked_file_sink_has_bounded_drain_and_keeps_worker_permit() {
    let _serial = CAPTURE_TESTS.lock().await;
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let permit = WorkerPermit::acquire().unwrap();
    let mut session = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    session.started = true;
    for state in &session.state {
        lock(state).summary.eof = true;
    }
    lock(&session.state[0]).summary.writer_finished = false;
    let queue = FileQueue::new(3);
    queue.push(b"abc").await;
    queue.close(false);
    let worker_queue = queue.clone();
    let worker_state = session.state[0].clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let thread = thread::spawn(move || {
        let _permit = permit;
        write_file(
            BlockedWriter {
                entered: Some(entered_tx),
                release: release_rx,
            },
            &worker_queue,
            &worker_state,
        );
    });
    session.workers[0] = Some(FileWorker { queue, thread });
    tokio::time::timeout(Duration::from_secs(1), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let (stdout, _) = tokio::time::timeout(
        Duration::from_secs(1),
        session.finish(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    assert!(!stdout.writer_finished);
    assert_eq!(stdout.issue, Some(CaptureIssue::DrainExpired));
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline + 1);
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while FILE_WORKERS.load(Ordering::Acquire) != baseline {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(lock(&session.state[0]).summary.file_bytes_written, 3);
}

#[tokio::test]
async fn adversarial_reader_panic_closes_its_file_queue_without_orphaning_workers() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let policy = CapturePolicy {
        capture_dir: Some(dir.0.clone()),
        // Capacity overflow is deterministic and attempts no physical
        // allocation. This executes the production reader's real panic.
        read_chunk_bytes: usize::MAX,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
    session.begin(ended_pipe(b""), ended_pipe(b"")).unwrap();
    let readers_completed = tokio::time::timeout(Duration::from_secs(1), async {
        while !session
            .readers
            .iter()
            .all(|reader| reader.as_ref().is_some_and(JoinHandle::is_finished))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let detected_failure = session.failure();
    let producer_queues_closed: Vec<_> = session
        .workers
        .iter()
        .flatten()
        .map(|worker| lock(&worker.queue.state).closed)
        .collect();

    // No producer survives and both files are empty. Observe the actual
    // drain behavior as well as queue state: an open orphaned queue makes
    // these idle workers wait for the large drain deadline. Cancelling this
    // finish future leaves the session available for independent cleanup.
    let drain_started = tokio::time::Instant::now();
    let finished_within_idle_budget = tokio::time::timeout(
        Duration::from_millis(100),
        session.finish(Duration::from_secs(30)),
    )
    .await
    .is_ok();
    let idle_drain_elapsed = drain_started.elapsed();

    // Cleanup is independent of the asserted producer behavior. Manually
    // close both queues before any assertion; these empty regular files
    // have no blocked write. finish can then join their actual OS workers,
    // and the test does not leave permits behind even when the probe fails.
    for worker in session.workers.iter().flatten() {
        worker.queue.close(true);
    }
    let (stdout, stderr) = tokio::time::timeout(
        Duration::from_secs(2),
        session.finish(Duration::from_secs(1)),
    )
    .await
    .unwrap();
    drop(session);
    assert!(stdout.writer_finished && stderr.writer_finished);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    assert!(readers_completed, "real reader panic did not complete");
    assert!(matches!(detected_failure, Some(CaptureIssue::ReadError(_))));
    assert!(
        finished_within_idle_budget,
        "empty capture workers failed to finish after their producers stopped: queues_closed={producer_queues_closed:?}, elapsed={idle_drain_elapsed:?}; idle workers remain blocked until a separate drain deadline or manual queue closure"
    );
    assert_eq!(
        producer_queues_closed,
        [true, true],
        "completed readers left producer queues open; workers cannot discover EOF and retain their permits until a separate drain deadline closes them"
    );
}

#[tokio::test]
async fn real_reader_error_closes_its_file_queue_and_releases_workers() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let policy = CapturePolicy {
        capture_dir: Some(dir.0.clone()),
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
    let (invalid_reader, write_end) = pipe();
    drop(invalid_reader);
    // A write-only pipe endpoint makes the real libc::read fail with
    // EBADF, contrasting the normal error epilogue with a task unwind.
    session.begin(write_end, ended_pipe(b"")).unwrap();
    let readers_completed = tokio::time::timeout(Duration::from_secs(1), async {
        while !session
            .readers
            .iter()
            .all(|reader| reader.as_ref().is_some_and(JoinHandle::is_finished))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let detected_failure = session.failure();
    let producer_queues_closed: Vec<_> = session
        .workers
        .iter()
        .flatten()
        .map(|worker| lock(&worker.queue.state).closed)
        .collect();
    // As in the panic probe, explicit queue closure precedes assertions
    // and independently prevents leaked test workers or global permits.
    for worker in session.workers.iter().flatten() {
        worker.queue.close(true);
    }
    let (stdout, stderr) = tokio::time::timeout(
        Duration::from_secs(2),
        session.finish(Duration::from_secs(1)),
    )
    .await
    .unwrap();
    drop(session);
    assert!(stdout.writer_finished && stderr.writer_finished);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    assert!(readers_completed, "real read error did not complete");
    assert!(matches!(detected_failure, Some(CaptureIssue::ReadError(_))));
    assert_eq!(producer_queues_closed, [true, true]);
}

#[tokio::test]
async fn aborted_readers_close_file_queues_before_first_poll_and_after_reading() {
    let _serial = CAPTURE_TESTS.lock().await;
    for after_reading in [false, true] {
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            read_chunk_bytes: 3,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        let (stdout, stdout_writer) = pipe();
        let (stderr, _stderr_writer) = pipe();
        let mut stdout_writer = File::from(stdout_writer);
        session.begin(stdout, stderr).unwrap();
        let read_completed = if after_reading {
            stdout_writer.write_all(b"abc").unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                while lock(&session.state[0]).summary.bytes_read != 3 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok()
        } else {
            // The current-thread test runtime has not yielded after begin,
            // so neither read_pipe future has been polled in this branch.
            true
        };
        for reader in session.readers.iter().flatten() {
            reader.abort();
        }
        // OutputSession::finish remains the sole consumer of JoinHandles.
        // Observe task termination here without polling a completed handle
        // twice; cancellation drops the producer before is_finished.
        let all_stopped = tokio::time::timeout(Duration::from_secs(1), async {
            while !session
                .readers
                .iter()
                .flatten()
                .all(JoinHandle::is_finished)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let producer_queues_closed: Vec<_> = session
            .workers
            .iter()
            .flatten()
            .map(|worker| lock(&worker.queue.state).closed)
            .collect();
        let detected_failure = session.failure();
        // Independent cleanup precedes assertions, retaining the worker
        // permit only until its actual thread can be joined.
        for worker in session.workers.iter().flatten() {
            worker.queue.close(false);
        }
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(2),
            session.finish(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        drop(session);
        assert!(read_completed && all_stopped);
        assert_eq!(producer_queues_closed, [true, true]);
        assert!(matches!(detected_failure, Some(CaptureIssue::ReadError(_))));
        assert!(stdout.writer_finished && stderr.writer_finished);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        assert!(!stdout.eof && !stderr.eof);
        let expected = if after_reading {
            b"abc".as_slice()
        } else {
            b""
        };
        assert_eq!(std::fs::read(dir.0.join("stdout.bin")).unwrap(), expected);
        assert_eq!(stdout.file_bytes_written, expected.len() as u64);
        assert_eq!(stdout.sha256, Some(Sha256::digest(expected).into()));
        assert_eq!(stderr.sha256, Some(Sha256::digest(b"").into()));
        assert!(std::fs::read(dir.0.join("stderr.bin")).unwrap().is_empty());
    }
}

#[tokio::test]
async fn dropping_unpolled_file_session_closes_queues_and_releases_actual_workers() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let policy = CapturePolicy {
        capture_dir: Some(dir.0.clone()),
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
    let (stdout, _stdout_writer) = pipe();
    let (stderr, _stderr_writer) = pipe();
    session.begin(stdout, stderr).unwrap();
    let readers: Vec<_> = session
        .readers
        .iter()
        .flatten()
        .map(JoinHandle::abort_handle)
        .collect();
    let queues: Vec<_> = session
        .workers
        .iter()
        .flatten()
        .map(|worker| worker.queue.clone())
        .collect();
    let snapshot = session.snapshotter();
    // No yield: OutputSession itself must close its workers' queues even
    // if the Tokio tasks have not begun executing read_pipe's body.
    drop(session);
    assert!(queues.iter().all(|queue| lock(&queue.state).closed));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !readers.iter().all(tokio::task::AbortHandle::is_finished)
            || FILE_WORKERS.load(Ordering::Acquire) != baseline
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (stdout, stderr) = snapshot.capture();
    assert!(stdout.writer_finished && stderr.writer_finished);
    assert!(!stdout.eof && !stderr.eof);
    assert_eq!(stdout.bytes_read + stderr.bytes_read, 0);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
}

#[tokio::test]
async fn worker_or_reader_panics_become_capture_failure() {
    let mut session = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    session.started = true;
    let reader = tokio::spawn(async { panic!("injected reader panic") });
    while !reader.is_finished() {
        tokio::task::yield_now().await;
    }
    session.readers[0] = Some(reader);
    assert!(matches!(
        session.failure(),
        Some(CaptureIssue::ReadError(_))
    ));
    let (stdout, _) = session.finish(Duration::ZERO).await;
    assert!(matches!(stdout.issue, Some(CaptureIssue::ReadError(_))));

    let mut session = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    session.started = true;
    lock(&session.state[1]).summary.writer_finished = false;
    let thread = thread::spawn(|| panic!("injected writer panic"));
    while !thread.is_finished() {
        tokio::task::yield_now().await;
    }
    session.workers[1] = Some(FileWorker {
        queue: FileQueue::new(3),
        thread,
    });
    assert!(matches!(
        session.failure(),
        Some(CaptureIssue::WriteError(_))
    ));
    let (_, stderr) = session.finish(Duration::ZERO).await;
    assert!(matches!(stderr.issue, Some(CaptureIssue::WriteError(_))));
}

#[tokio::test]
async fn panicked_file_writer_closes_queue_and_releases_inflight_reservation() {
    struct PanicWriter;
    impl Write for PanicWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!("injected actual file writer unwind");
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let _serial = CAPTURE_TESTS.lock().await;
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let mut session = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    session.started = true;
    lock(&session.state[0]).summary.writer_finished = false;
    let queue = FileQueue::new(3);
    let reader = tokio::spawn(read_pipe(
        nonblocking_pipe(ended_pipe(b"abcdef")).unwrap(),
        session.state[0].clone(),
        Some(FileProducer {
            queue: queue.clone(),
        }),
        None,
        OutputStream::Stdout,
        3,
        20,
    ));
    session.readers[0] = Some(reader);
    let permit = WorkerPermit::acquire().unwrap();
    let worker_queue = queue.clone();
    let state = session.state[0].clone();
    let worker = thread::spawn(move || {
        let _permit = permit;
        write_file(PanicWriter, &worker_queue, &state);
    });
    session.workers[0] = Some(FileWorker {
        queue: queue.clone(),
        thread: worker,
    });
    let worker_finished = tokio::time::timeout(Duration::from_secs(1), async {
        while !session.workers[0].as_ref().unwrap().thread.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    let detected_failure = session.failure();
    let (closed_after_unwind, reserved_after_unwind) = {
        let state = lock(&queue.state);
        (state.closed, state.used)
    };
    let finished_without_orphan_wait = tokio::time::timeout(
        Duration::from_millis(100),
        session.finish(Duration::from_secs(30)),
    )
    .await
    .is_ok();

    // Independent cleanup precedes assertions even when the consumer left
    // its queue open: the real producer task and worker handle are retained.
    queue.close(true);
    let (stdout, _) = tokio::time::timeout(
        Duration::from_secs(2),
        session.finish(Duration::from_secs(1)),
    )
    .await
    .unwrap();
    drop(session);
    assert!(worker_finished);
    assert!(matches!(
        detected_failure,
        Some(CaptureIssue::WriteError(_))
    ));
    assert!(matches!(stdout.issue, Some(CaptureIssue::WriteError(_))));
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    assert!(
        closed_after_unwind,
        "dead file consumer left its producer queue open"
    );
    assert_eq!(
        reserved_after_unwind, 0,
        "unwound writer retained its dropped chunk reservation"
    );
    assert!(
        finished_without_orphan_wait,
        "producer waited for a dead file consumer until drain expiration"
    );
}
