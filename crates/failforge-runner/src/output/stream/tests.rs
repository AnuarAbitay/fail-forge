use super::super::session::OutputSnapshot;
use super::super::test_support::{state, stream_receiver};
use super::*;
use std::io::Write;
use std::thread;
use std::time::Duration;

#[test]
fn stream_budget_is_shared_and_includes_inflight_chunks() {
    let queue = StreamQueue::new(6);
    queue.push(OutputStream::Stdout, b"abcdef");
    let mut chunk = queue.pop().unwrap();
    assert_eq!(chunk.stream, OutputStream::Stdout);
    assert_eq!(chunk.bytes(), b"abcdef");
    queue.push(OutputStream::Stderr, b"xy");
    assert!(queue.pop().is_none());
    assert_eq!(lock(&queue.state).dropped, [0, 2]);
    chunk.mark_displayed(2);
    assert_eq!(lock(&queue.state).unconfirmed, [4, 0]);
    queue.push(OutputStream::Stderr, b"z");
    assert_eq!(lock(&queue.state).dropped, [0, 3]);
    drop(chunk);
    assert_eq!(lock(&queue.state).used, 0);
    assert_eq!(lock(&queue.state).dropped, [4, 3]);
    assert_eq!(lock(&queue.state).unconfirmed, [0, 0]);
    queue.push(OutputStream::Stderr, &[255, 0]);
    let mut chunk = queue.pop().unwrap();
    chunk.mark_displayed(usize::MAX);
    chunk.mark_displayed(1);
    drop(chunk);
    assert_eq!(lock(&queue.state).dropped, [4, 3]);
}

#[test]
fn changing_the_public_display_label_cannot_corrupt_source_byte_accounting() {
    let queue = StreamQueue::new(4);
    queue.push(OutputStream::Stdout, b"abcd");
    let mut chunk = queue.pop().unwrap();
    chunk.stream = OutputStream::Stderr;
    chunk.mark_displayed(2);
    assert_eq!(lock(&queue.state).unconfirmed, [2, 0]);
    drop(chunk);
    assert_eq!(lock(&queue.state).unconfirmed, [0, 0]);
    assert_eq!(lock(&queue.state).dropped, [2, 0]);
    assert_eq!(lock(&queue.state).used, 0);
    assert!(lock(&queue.state).error.is_none());
    queue.push(OutputStream::Stderr, b"next");
    let mut next = queue.pop().unwrap();
    next.mark_displayed(4);
    drop(next);
    assert_eq!(lock(&queue.state).dropped, [2, 0]);
}

#[test]
fn receiver_loss_snapshot_observes_queue_and_unacknowledged_chunk_losses() {
    let queue = StreamQueue::new(4);
    let receiver = StreamReceiver {
        queue: queue.clone(),
    };
    assert_eq!(receiver.dropped_bytes(), (0, 0));
    queue.push(OutputStream::Stdout, b"abcd");
    queue.push(OutputStream::Stderr, b"x");
    assert_eq!(receiver.dropped_bytes(), (0, 1));
    let mut chunk = queue.pop().unwrap();
    chunk.mark_displayed(2);
    drop(chunk);
    assert_eq!(receiver.dropped_bytes(), (2, 1));
}

#[test]
fn stream_zero_budget_and_oversized_chunks_are_accounted() {
    for limit in [0, 1, 2] {
        let queue = StreamQueue::new(limit);
        queue.push(OutputStream::Stdout, &[]);
        queue.push(OutputStream::Stderr, &[0, 255, 10]);
        assert_eq!(lock(&queue.state).used, 0);
        assert_eq!(lock(&queue.state).dropped, [0, 3]);
        assert!(queue.pop().is_none());
    }
}

#[test]
fn stream_dropped_counter_overflow_is_explicit() {
    let queue = StreamQueue::new(0);
    lock(&queue.state).dropped[1] = u64::MAX;
    queue.push(OutputStream::Stderr, b"x");
    assert_eq!(lock(&queue.state).dropped[1], u64::MAX);
    assert_eq!(
        lock(&queue.state).error,
        Some((OutputStream::Stderr, CaptureIssue::CounterOverflow))
    );
}

#[test]
fn stream_close_preserves_or_discards_queued_bytes_explicitly() {
    let queue = StreamQueue::new(8);
    queue.push(OutputStream::Stdout, b"abc");
    queue.push(OutputStream::Stderr, b"def");
    queue.close(false);
    assert_eq!(queue.pop().unwrap().bytes(), b"abc");
    queue.close(true);
    queue.close(true);
    queue.push(OutputStream::Stdout, b"x");
    assert_eq!(lock(&queue.state).dropped, [4, 3]);
    assert_eq!(lock(&queue.state).used, 0);
    assert!(queue.pop().is_none());
}

#[tokio::test]
async fn async_stream_receive_and_receiver_drop_account_every_byte() {
    let queue = StreamQueue::new(20);
    let mut receiver = stream_receiver(&queue);
    let producer = queue.clone();
    tokio::spawn(async move {
        producer.push(OutputStream::Stderr, &[255, 0, 128]);
        producer.close(false);
    });
    let mut chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk.stream, OutputStream::Stderr);
    chunk.mark_displayed(1);
    drop(chunk);
    assert!(receiver.recv().await.is_none());
    assert_eq!(lock(&queue.state).dropped, [0, 2]);

    let queue = StreamQueue::new(20);
    let receiver = stream_receiver(&queue);
    queue.push(OutputStream::Stdout, b"abc");
    drop(receiver);
    queue.push(OutputStream::Stderr, b"de");
    assert_eq!(lock(&queue.state).dropped, [3, 2]);
    assert_eq!(lock(&queue.state).used, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_async_stream_delivers_all_final_queued_chunks_before_eof() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..128 {
            let queue = StreamQueue::new(64);
            let mut receiver = stream_receiver(&queue);
            let producer = queue.clone();
            let task = tokio::spawn(async move {
                producer.push(OutputStream::Stdout, b"first");
                tokio::task::yield_now().await;
                producer.push(OutputStream::Stderr, b"final");
                producer.close(false);
            });
            let mut received = Vec::new();
            while let Some(mut chunk) = receiver.recv().await {
                received.push((chunk.stream, chunk.bytes().to_vec()));
                chunk.mark_displayed(chunk.bytes().len());
            }
            task.await.unwrap();
            assert_eq!(
                received,
                vec![
                    (OutputStream::Stdout, b"first".to_vec()),
                    (OutputStream::Stderr, b"final".to_vec()),
                ]
            );
            assert_eq!(lock(&queue.state).dropped, [0, 0]);
            assert_eq!(lock(&queue.state).used, 0);
        }
    })
    .await
    .unwrap();
}

#[test]
fn blocking_stream_receiver_wakes_on_bytes_and_close() {
    let queue = StreamQueue::new(20);
    let mut receiver = stream_receiver(&queue);
    let worker = thread::spawn(move || {
        let mut chunk = receiver.blocking_recv().unwrap();
        assert_eq!(chunk.bytes(), b"bytes");
        chunk.mark_displayed(chunk.bytes().len());
        drop(chunk);
        assert!(receiver.blocking_recv().is_none());
    });
    queue.push(OutputStream::Stdout, b"bytes");
    queue.close(false);
    worker.join().unwrap();
    assert_eq!(lock(&queue.state).dropped, [0, 0]);
    assert_eq!(lock(&queue.state).unconfirmed, [0, 0]);
}

#[tokio::test]
async fn sink_error_identifies_stream_and_omits_secret_text() {
    let queue = StreamQueue::new(20);
    let mut receiver = stream_receiver(&queue);
    queue.push(OutputStream::Stderr, b"abc");
    let chunk = receiver.recv().await.unwrap();
    chunk.report_error(io::Error::new(io::ErrorKind::BrokenPipe, "SECRET_ENV"));
    receiver.report_error(OutputStream::Stdout, io::Error::other("second failure"));
    assert!(matches!(
        lock(&queue.state).error,
        Some((OutputStream::Stderr, CaptureIssue::StreamError(_)))
    ));
    assert!(!format!("{:?}", lock(&queue.state).error).contains("SECRET_ENV"));
    drop(chunk);
    assert_eq!(lock(&queue.state).dropped, [0, 3]);
    assert!(receiver.recv().await.is_none());
}

// Regression: a later receive must not change the failed chunk's source.
#[tokio::test]
async fn adversarial_sink_error_belongs_to_failed_chunk_with_two_inflight_chunks() {
    struct BrokenSink;
    impl Write for BrokenSink {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let queue = StreamQueue::new(6);
    let mut receiver = stream_receiver(&queue);
    queue.push(OutputStream::Stdout, b"out");
    queue.push(OutputStream::Stderr, b"err");
    let first = receiver.recv().await.unwrap();
    let second = receiver.recv().await.unwrap();
    assert_eq!(first.stream, OutputStream::Stdout);
    assert_eq!(second.stream, OutputStream::Stderr);
    assert_eq!(lock(&queue.state).used, 6);

    let error = BrokenSink.write_all(first.bytes()).unwrap_err();
    first.report_error(error);
    let snapshot = OutputSnapshot {
        state: [state(0), state(0)],
        stream: Some(queue.clone()),
    };
    let (stdout, stderr) = snapshot.capture();
    assert!(
        matches!(stdout.issue, Some(CaptureIssue::StreamError(_))),
        "stdout sink failed, but issues are stdout={:?}, stderr={:?}",
        stdout.issue,
        stderr.issue
    );
    assert!(stderr.issue.is_none());
}

#[tokio::test]
async fn chunk_errors_keep_original_source_in_both_orders_and_preserve_first_error() {
    for (first_source, second_source) in [
        (OutputStream::Stdout, OutputStream::Stderr),
        (OutputStream::Stderr, OutputStream::Stdout),
    ] {
        let queue = StreamQueue::new(6);
        let mut receiver = stream_receiver(&queue);
        queue.push(first_source, b"one");
        queue.push(second_source, b"two");
        let mut first = receiver.recv().await.unwrap();
        let second = receiver.blocking_recv().unwrap();
        first.stream = second_source;
        first.mark_displayed(1);
        first.report_error(io::Error::new(io::ErrorKind::BrokenPipe, "SECRET_FIRST"));
        let original = lock(&queue.state).error.clone().unwrap();
        second.report_error(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SECRET_SECOND",
        ));
        receiver.report_error(second_source, io::Error::other("SECRET_THIRD"));
        assert_eq!(lock(&queue.state).error.as_ref(), Some(&original));
        assert_eq!(original.0, first_source);
        assert!(
            matches!(&original.1, CaptureIssue::StreamError(text) if text.contains("BrokenPipe") && !text.contains("SECRET"))
        );
        drop(first);
        drop(second);
        let state = lock(&queue.state);
        assert_eq!(state.used, 0);
        assert_eq!(state.unconfirmed, [0, 0]);
        assert_eq!(state.dropped[first_source.index()], 2);
        assert_eq!(state.dropped[second_source.index()], 3);
    }
}

#[test]
fn receiver_error_without_a_chunk_uses_explicit_source_and_finalization_cutoff() {
    let queue = StreamQueue::new(6);
    let receiver = stream_receiver(&queue);
    receiver.report_error(
        OutputStream::Stderr,
        io::Error::new(io::ErrorKind::BrokenPipe, "SECRET_ENV"),
    );
    let snapshot = OutputSnapshot {
        state: [state(0), state(0)],
        stream: Some(queue.clone()),
    };
    let (stdout, stderr) = snapshot.capture();
    assert!(stdout.issue.is_none());
    assert!(
        matches!(&stderr.issue, Some(CaptureIssue::StreamError(text)) if text.contains("BrokenPipe") && !text.contains("SECRET_ENV"))
    );
    let original = lock(&queue.state).error.clone();
    lock(&queue.state).finalized = true;
    receiver.report_error(OutputStream::Stdout, io::Error::other("late failure"));
    assert_eq!(lock(&queue.state).error, original);

    let already_finalized = StreamQueue::new(6);
    lock(&already_finalized.state).finalized = true;
    stream_receiver(&already_finalized)
        .report_error(OutputStream::Stderr, io::Error::other("late failure"));
    assert!(lock(&already_finalized.state).error.is_none());
}
