use super::super::test_support::{CAPTURE_TESTS, state};
use super::*;
use std::time::Duration;

#[tokio::test]
async fn file_queue_budget_includes_worker_chunk_and_cancellation_is_safe() {
    let queue = FileQueue::new(3);
    assert!(queue.push(b"abc").await);
    let chunk = queue.pop().unwrap();
    assert_eq!(chunk, b"abc");
    assert_eq!(lock(&queue.state).used, 3);
    assert!(
        tokio::time::timeout(Duration::from_millis(5), queue.push(b"x"))
            .await
            .is_err()
    );
    assert_eq!(lock(&queue.state).used, 3);
    queue.release(chunk.len());
    assert!(queue.push(b"xy").await);
    queue.close(true);
    queue.close(true);
    assert_eq!(lock(&queue.state).used, 0);
    assert!(queue.pop().is_none());
    assert!(!queue.push(b"z").await);
    assert!(queue.push(&[]).await);
    let queue = FileQueue::new(2);
    assert!(!queue.push(b"oversized").await);
    assert_eq!(lock(&queue.state).used, 0);
}

#[tokio::test]
async fn file_queue_wakes_producer_and_keeps_order_when_closing() {
    let queue = FileQueue::new(2);
    assert!(queue.push(b"ab").await);
    let worker_queue = queue.clone();
    let worker = thread::spawn(move || {
        let first = worker_queue.pop().unwrap();
        assert_eq!(first, b"ab");
        worker_queue.release(first.len());
        let second = worker_queue.pop().unwrap();
        assert_eq!(second, b"cd");
        worker_queue.release(second.len());
        assert!(worker_queue.pop().is_none());
    });
    assert!(
        tokio::time::timeout(Duration::from_secs(1), queue.push(b"cd"))
            .await
            .unwrap()
    );
    queue.close(false);
    worker.join().unwrap();
    assert_eq!(lock(&queue.state).used, 0);
}

enum WriteAction {
    Count(usize),
    Error(io::ErrorKind),
}

struct ScriptedWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    actions: VecDeque<WriteAction>,
    flush_error: bool,
}

impl Write for ScriptedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = match self.actions.pop_front() {
            Some(WriteAction::Count(count)) => count.min(bytes.len()),
            Some(WriteAction::Error(kind)) => return Err(io::Error::new(kind, "SECRET_SINK")),
            None => bytes.len(),
        };
        lock(&self.bytes).extend_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.flush_error {
            Err(io::Error::other("SECRET_FLUSH"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn file_writer_retries_interrupted_and_partial_writes_without_reordering() {
    let queue = FileQueue::new(20);
    queue.push(&[0, 255, 128]).await;
    queue.push(b"xyz").await;
    queue.close(false);
    let output = Arc::new(Mutex::new(Vec::new()));
    let state = state(20);
    write_file(
        ScriptedWriter {
            bytes: output.clone(),
            actions: [
                WriteAction::Error(io::ErrorKind::Interrupted),
                WriteAction::Count(1),
                WriteAction::Count(1),
            ]
            .into(),
            flush_error: false,
        },
        &queue,
        &state,
    );
    assert_eq!(*lock(&output), [0, 255, 128, b'x', b'y', b'z']);
    assert_eq!(lock(&state).summary.file_bytes_written, 6);
    assert!(lock(&state).summary.writer_finished);
    assert!(lock(&state).summary.issue.is_none());
    assert_eq!(lock(&queue.state).used, 0);
}

#[tokio::test]
async fn file_writer_preserves_partial_count_on_zero_error_and_flush_failure() {
    for action in [
        WriteAction::Count(0),
        WriteAction::Error(io::ErrorKind::BrokenPipe),
    ] {
        let queue = FileQueue::new(20);
        queue.push(b"abcdef").await;
        queue.push(b"queued").await;
        queue.close(false);
        let output = Arc::new(Mutex::new(Vec::new()));
        let state = state(20);
        write_file(
            ScriptedWriter {
                bytes: output.clone(),
                actions: [WriteAction::Count(2), action].into(),
                flush_error: false,
            },
            &queue,
            &state,
        );
        assert_eq!(*lock(&output), b"ab");
        assert_eq!(lock(&state).summary.file_bytes_written, 2);
        assert!(lock(&state).summary.writer_finished);
        assert!(matches!(
            lock(&state).summary.issue,
            Some(CaptureIssue::WriteError(_))
        ));
        assert!(!format!("{:?}", lock(&state).summary.issue).contains("SECRET"));
        assert_eq!(lock(&queue.state).used, 0);
    }
    let queue = FileQueue::new(3);
    queue.push(b"abc").await;
    queue.close(false);
    let state = state(3);
    write_file(
        ScriptedWriter {
            bytes: Arc::new(Mutex::new(Vec::new())),
            actions: VecDeque::new(),
            flush_error: true,
        },
        &queue,
        &state,
    );
    assert_eq!(lock(&state).summary.file_bytes_written, 3);
    assert!(matches!(
        lock(&state).summary.issue,
        Some(CaptureIssue::WriteError(_))
    ));
}

#[tokio::test]
async fn file_written_counter_overflow_is_explicit() {
    let queue = FileQueue::new(1);
    queue.push(b"x").await;
    queue.close(false);
    let state = state(1);
    lock(&state).summary.file_bytes_written = u64::MAX;
    write_file(Vec::new(), &queue, &state);
    assert_eq!(
        lock(&state).summary.issue,
        Some(CaptureIssue::CounterOverflow)
    );
}

#[tokio::test]
async fn worker_permits_are_limited_and_released_by_real_ownership() {
    let _serial = CAPTURE_TESTS.lock().await;
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let mut permits = Vec::new();
    for _ in baseline..MAX_FILE_WORKERS {
        permits.push(WorkerPermit::acquire().unwrap());
    }
    assert_eq!(
        WorkerPermit::acquire().err().unwrap().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(permits.pop());
    permits.push(WorkerPermit::acquire().unwrap());
    drop(permits);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
}

#[tokio::test]
async fn file_writer_unwind_preserves_partial_count_and_first_error_at_write_and_flush() {
    struct PanicWriter {
        flush_panics: bool,
        writes: usize,
    }
    impl Write for PanicWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if !self.flush_panics && self.writes > 1 {
                panic!("SECRET_WRITE_PANIC");
            }
            Ok(if self.flush_panics { bytes.len() } else { 2 })
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("SECRET_FLUSH_PANIC");
        }
    }

    for flush_panics in [false, true] {
        for prior_issue in [None, Some(CaptureIssue::FileQuotaExceeded)] {
            let queue = FileQueue::new(20);
            assert!(queue.push(b"abcdef").await);
            assert!(queue.push(b"xyz").await);
            queue.close(false);
            let state = state(20);
            lock(&state).summary.issue = prior_issue.clone();
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_file(
                    PanicWriter {
                        flush_panics,
                        writes: 0,
                    },
                    &queue,
                    &state,
                );
            }))
            .is_err();
            assert!(panicked);
            let state = lock(&state);
            assert_eq!(
                state.summary.file_bytes_written,
                if flush_panics { 9 } else { 2 }
            );
            assert!(!state.summary.writer_finished);
            assert_eq!(
                state.summary.issue,
                prior_issue.or_else(|| Some(CaptureIssue::WriteError(
                    "capture worker stopped unexpectedly".into(),
                )))
            );
            assert!(!format!("{:?}", state.summary.issue).contains("SECRET"));
            let queued = lock(&queue.state);
            assert!(queued.closed && queued.chunks.is_empty());
            assert_eq!(queued.used, 0);
        }
    }
}
