use super::super::file::{FILE_WORKERS, MAX_FILE_WORKERS};
use super::super::test_support::{CAPTURE_TESTS, TestDirectory, ended_pipe, pipe};
use super::*;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn prepare_resolves_invocation_directory_and_never_overwrites_capture() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let policy = CapturePolicy {
        capture_dir: Some("logs".into()),
        ..Default::default()
    };
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let session = OutputSession::prepare(&policy, &dir.0).unwrap();
    assert_eq!(
        lock(&session.state[0]).summary.file_path,
        Some(dir.0.join("logs/stdout.bin"))
    );
    assert_eq!(
        lock(&session.state[1]).summary.file_path,
        Some(dir.0.join("logs/stderr.bin"))
    );
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline + 2);
    drop(session);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    std::fs::write(dir.0.join("logs/stdout.bin"), b"keep stdout").unwrap();
    std::fs::write(dir.0.join("logs/stderr.bin"), b"keep stderr").unwrap();
    assert_eq!(
        OutputSession::prepare(&policy, &dir.0)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        std::fs::read(dir.0.join("logs/stdout.bin")).unwrap(),
        b"keep stdout"
    );
    assert_eq!(
        std::fs::read(dir.0.join("logs/stderr.bin")).unwrap(),
        b"keep stderr"
    );
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
}

#[tokio::test]
async fn prepare_rejects_zero_read_chunk_and_bad_directory() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let policy = CapturePolicy {
        read_chunk_bytes: 0,
        ..Default::default()
    };
    assert_eq!(
        OutputSession::prepare(&policy, &dir.0)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
    std::fs::write(dir.0.join("file"), b"existing").unwrap();
    let policy = CapturePolicy {
        capture_dir: Some("file/subdir".into()),
        ..Default::default()
    };
    assert!(OutputSession::prepare(&policy, &dir.0).is_err());
    assert_eq!(std::fs::read(dir.0.join("file")).unwrap(), b"existing");
}

#[tokio::test]
async fn second_file_worker_permit_failure_releases_first_reservation() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    assert!(baseline < MAX_FILE_WORKERS - 1);
    let mut held = Vec::new();
    for _ in baseline..MAX_FILE_WORKERS - 1 {
        held.push(WorkerPermit::acquire().unwrap());
    }
    let before = FILE_WORKERS.load(Ordering::Acquire);
    assert_eq!(before, MAX_FILE_WORKERS - 1);
    let path = dir.0.join("not-created");
    let policy = CapturePolicy {
        capture_dir: Some(path.clone()),
        ..Default::default()
    };
    // stdout can reserve the final permit; stderr cannot. Failed prepare
    // must release stdout's reservation before returning to its caller.
    let error = OutputSession::prepare(&policy, Path::new("/"))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), before);
    assert!(!path.exists());
    drop(held);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
}

#[tokio::test]
async fn second_capture_file_failure_releases_permits_and_preserves_existing_stderr() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let existing = dir.0.join("stderr.bin");
    std::fs::write(&existing, b"keep existing stderr").unwrap();
    let policy = CapturePolicy {
        capture_dir: Some(dir.0.clone()),
        ..Default::default()
    };
    let error = OutputSession::prepare(&policy, Path::new("/"))
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    assert_eq!(std::fs::read(existing).unwrap(), b"keep existing stderr");
    // Preparation is nontransactional: the newly created stdout artifact
    // remains, while the pre-existing stderr file is never overwritten.
    assert!(std::fs::read(dir.0.join("stdout.bin")).unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_pipe_capture_preserves_binary_bytes_and_bounded_tails() {
    let policy = CapturePolicy {
        read_chunk_bytes: 2,
        tail_bytes_per_stream: 3,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
    assert!(
        session
            .begin(ended_pipe(&[0, 255, 128, 13, 10]), ended_pipe(b"abcdef"))
            .unwrap()
            .is_none()
    );
    let (stdout, stderr) = session.finish(Duration::from_secs(1)).await;
    assert_eq!(stdout.bytes_read, 5);
    assert_eq!(
        stdout.sha256,
        Some(Sha256::digest([0, 255, 128, 13, 10]).into())
    );
    assert_eq!(stdout.tail, [128, 13, 10]);
    assert_eq!(stderr.bytes_read, 6);
    assert_eq!(stderr.sha256, Some(Sha256::digest(b"abcdef").into()));
    assert_eq!(stderr.tail, b"def");
    for summary in [&stdout, &stderr] {
        assert!(summary.eof);
        assert!(summary.tail_truncated);
        assert!(summary.writer_finished);
        assert!(summary.issue.is_none());
    }
    assert!(session.failure().is_none());
    let (again, _) = session.finish(Duration::ZERO).await;
    assert_eq!(again.tail, stdout.tail);
    assert_eq!(again.sha256, stdout.sha256);
    assert_eq!(
        session
            .begin(ended_pipe(b""), ended_pipe(b""))
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn file_capture_preserves_raw_bytes_exact_quota_and_reports_overflow() {
    let _serial = CAPTURE_TESTS.lock().await;
    for quota in [0, 3, 4, 5] {
        let dir = TestDirectory::new();
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            read_chunk_bytes: 3,
            tail_bytes_per_stream: 4,
            file_quota_bytes_per_stream: quota,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        session
            .begin(ended_pipe(&[0, 255, 128, 10]), ended_pipe(b"abc"))
            .unwrap();
        let (stdout, stderr) = session.finish(Duration::from_secs(1)).await;
        assert_eq!(stdout.bytes_read, 4);
        assert_eq!(stderr.bytes_read, 3);
        // The stream digest includes bytes omitted from a quota-limited
        // file, so truncated or replaced storage cannot authenticate them.
        assert_eq!(
            stdout.sha256,
            Some(Sha256::digest([0, 255, 128, 10]).into())
        );
        assert_eq!(stderr.sha256, Some(Sha256::digest(b"abc").into()));
        assert_eq!(stdout.tail, [0, 255, 128, 10]);
        assert_eq!(stdout.file_bytes_written, quota.min(4));
        assert_eq!(stderr.file_bytes_written, quota.min(3));
        assert_eq!(
            std::fs::read(dir.0.join("stdout.bin")).unwrap(),
            [0, 255, 128, 10][..quota.min(4) as usize]
        );
        assert_eq!(
            std::fs::read(dir.0.join("stderr.bin")).unwrap(),
            b"abc"[..quota.min(3) as usize]
        );
        assert_eq!(
            stdout.issue,
            (quota < 4).then_some(CaptureIssue::FileQuotaExceeded)
        );
        assert_eq!(
            stderr.issue,
            (quota < 3).then_some(CaptureIssue::FileQuotaExceeded)
        );
        assert!(stdout.eof && stderr.eof && stdout.writer_finished && stderr.writer_finished);
        assert_eq!(session.failure().is_some(), quota < 4);
    }
}

#[tokio::test]
async fn undrained_pipe_expires_without_waiting_for_eof() {
    let mut session = OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
    let (read, _held_writer) = pipe();
    session.begin(read, ended_pipe(b"stderr bytes")).unwrap();
    let (stdout, stderr) = tokio::time::timeout(
        Duration::from_secs(1),
        session.finish(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    assert!(!stdout.eof);
    assert_eq!(stdout.sha256, Some(Sha256::digest(b"").into()));
    assert_eq!(stdout.issue, Some(CaptureIssue::DrainExpired));
    assert_eq!(stderr.tail, b"stderr bytes");
    assert_eq!(stderr.sha256, Some(Sha256::digest(b"stderr bytes").into()));
    assert!(stderr.eof);
    assert_eq!(session.failure(), Some(CaptureIssue::DrainExpired));
}

#[tokio::test]
async fn session_drop_aborts_pipe_readers_and_closes_display() {
    let policy = CapturePolicy {
        mode: OutputMode::StreamAndCapture,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
    let (read, _writer) = pipe();
    let mut receiver = session.begin(read, ended_pipe(b"")).unwrap().unwrap();
    drop(session);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn stream_slow_consumer_is_bounded_and_losses_are_visible() {
    let policy = CapturePolicy {
        mode: OutputMode::StreamAndCapture,
        read_chunk_bytes: 3,
        stream_queue_bytes: 3,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
    let _receiver = session
        .begin(ended_pipe(b"abcdef"), ended_pipe(b"xyz"))
        .unwrap()
        .unwrap();
    let (stdout, stderr) = tokio::time::timeout(
        Duration::from_secs(1),
        session.finish(Duration::from_millis(10)),
    )
    .await
    .unwrap();
    assert_eq!(stdout.bytes_read + stderr.bytes_read, 9);
    assert_eq!(stdout.stream_dropped_bytes + stderr.stream_dropped_bytes, 9);
    assert_eq!(
        stdout.stream_unconfirmed_bytes + stderr.stream_unconfirmed_bytes,
        0
    );
    assert!(stdout.issue.is_none() && stderr.issue.is_none());
    assert!(stdout.eof && stderr.eof);
}

#[tokio::test]
async fn stream_inflight_unacknowledged_bytes_remain_explicit() {
    let policy = CapturePolicy {
        mode: OutputMode::StreamAndCapture,
        read_chunk_bytes: 3,
        stream_queue_bytes: 6,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
    let mut receiver = session
        .begin(ended_pipe(b"abc"), ended_pipe(b""))
        .unwrap()
        .unwrap();
    let mut chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    chunk.mark_displayed(1);
    let (stdout, _) = session.finish(Duration::from_millis(5)).await;
    assert_eq!(stdout.stream_dropped_bytes, 0);
    assert_eq!(stdout.stream_unconfirmed_bytes, 2);
    assert!(stdout.issue.is_none());
    drop(chunk);
    assert_eq!(lock(&receiver.queue.state).dropped[0], 2);
    // The returned result is an immutable snapshot, not a live view.
    assert_eq!(stdout.stream_unconfirmed_bytes, 2);
}

#[tokio::test]
async fn acknowledged_held_chunk_has_no_loss_or_capture_error_after_finalization() {
    let policy = CapturePolicy {
        mode: OutputMode::StreamAndCapture,
        read_chunk_bytes: 3,
        stream_queue_bytes: 3,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
    let mut receiver = session
        .begin(ended_pipe(b"abc"), ended_pipe(b""))
        .unwrap()
        .unwrap();
    let mut chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    chunk.mark_displayed(chunk.bytes().len());
    tokio::time::timeout(Duration::from_secs(1), async {
        while !session
            .readers
            .iter()
            .all(|reader| reader.as_ref().is_some_and(JoinHandle::is_finished))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The chunk still reserves its memory budget. No latency claim is
    // made, but complete acknowledgment must prevent false display loss.
    let (stdout, stderr) = tokio::time::timeout(
        Duration::from_secs(1),
        session.finish(Duration::from_millis(5)),
    )
    .await
    .unwrap();
    assert_eq!(stdout.bytes_read, 3);
    assert_eq!(stdout.tail, b"abc");
    for summary in [&stdout, &stderr] {
        assert_eq!(summary.stream_dropped_bytes, 0);
        assert_eq!(summary.stream_unconfirmed_bytes, 0);
        assert!(summary.issue.is_none());
        assert!(summary.eof && summary.writer_finished);
    }
    chunk.report_error(io::Error::from(io::ErrorKind::BrokenPipe));
    assert!(lock(&receiver.queue.state).error.is_none());
    drop(chunk);
    receiver.report_error(
        OutputStream::Stdout,
        io::Error::from(io::ErrorKind::BrokenPipe),
    );
    let (again, _) = session.finish(Duration::ZERO).await;
    assert_eq!(again.tail, stdout.tail);
    assert_eq!(again.stream_dropped_bytes, stdout.stream_dropped_bytes);
    assert_eq!(
        again.stream_unconfirmed_bytes,
        stdout.stream_unconfirmed_bytes
    );
    assert!(again.issue.is_none());
    assert!(session.failure().is_none());
}

#[tokio::test]
async fn stream_error_propagates_through_session_failure_and_snapshot() {
    let policy = CapturePolicy {
        mode: OutputMode::StreamAndCapture,
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
    let mut receiver = session
        .begin(ended_pipe(b""), ended_pipe(b"error"))
        .unwrap()
        .unwrap();
    let chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    chunk.report_error(io::Error::from(io::ErrorKind::BrokenPipe));
    drop(chunk);
    assert!(matches!(
        session.failure(),
        Some(CaptureIssue::StreamError(_))
    ));
    let (stdout, stderr) = session.finish(Duration::from_secs(1)).await;
    assert!(stdout.issue.is_none());
    assert!(matches!(stderr.issue, Some(CaptureIssue::StreamError(_))));
    assert_eq!(stderr.stream_dropped_bytes, 5);
    receiver.report_error(OutputStream::Stderr, io::Error::other("late failure"));
    assert_eq!(
        lock(&receiver.queue.state).error.as_ref().unwrap().1,
        stderr.issue.unwrap()
    );
}

#[tokio::test]
async fn successful_file_and_display_capture_finishes_at_eof_for_small_read_partitions() {
    let _serial = CAPTURE_TESTS.lock().await;
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let stdout_bytes: Vec<_> = (0..2003).map(|index| index as u8).collect();
    let stderr_bytes: Vec<_> = (0..1999).map(|index| (255 - index % 256) as u8).collect();
    for read_chunk_bytes in [1, 3, 32, 32 * 1024] {
        let dir = TestDirectory::new();
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            read_chunk_bytes,
            tail_bytes_per_stream: 0,
            mode: OutputMode::StreamAndCapture,
            stream_queue_bytes: 4096,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        let mut receiver = session
            .begin(ended_pipe(&stdout_bytes), ended_pipe(&stderr_bytes))
            .unwrap()
            .unwrap();
        let consumer = tokio::spawn(async move {
            let mut displayed = [Vec::new(), Vec::new()];
            while let Some(mut chunk) = receiver.recv().await {
                displayed[chunk.stream.index()].extend_from_slice(chunk.bytes());
                chunk.mark_displayed(chunk.bytes().len());
            }
            displayed
        });
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(3),
            session.finish(Duration::from_secs(2)),
        )
        .await
        .unwrap();
        let displayed = tokio::time::timeout(Duration::from_secs(1), consumer)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(displayed, [stdout_bytes.clone(), stderr_bytes.clone()]);
        for (summary, expected) in [(&stdout, &stdout_bytes), (&stderr, &stderr_bytes)] {
            assert!(summary.eof && summary.writer_finished);
            assert!(summary.issue.is_none());
            assert!(summary.tail.is_empty() && summary.tail_truncated);
            assert_eq!(summary.bytes_read, expected.len() as u64);
            assert_eq!(summary.sha256, Some(Sha256::digest(expected).into()));
            assert_eq!(summary.file_bytes_written, expected.len() as u64);
            assert_eq!(
                summary.stream_dropped_bytes + summary.stream_unconfirmed_bytes,
                0
            );
            assert_eq!(
                std::fs::read(summary.file_path.as_ref().unwrap()).unwrap(),
                *expected
            );
        }
        assert!(session.failure().is_none());
        let (frozen, _) = session.finish(Duration::ZERO).await;
        assert!(frozen.eof && frozen.writer_finished && frozen.issue.is_none());
        assert_eq!(frozen.bytes_read, stdout.bytes_read);
        assert_eq!(frozen.sha256, stdout.sha256);
        assert_eq!(frozen.file_bytes_written, stdout.file_bytes_written);
        drop(session);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    }
}

#[tokio::test]
async fn unstarted_session_finish_releases_prepared_files_without_workers() {
    let _serial = CAPTURE_TESTS.lock().await;
    let dir = TestDirectory::new();
    let baseline = FILE_WORKERS.load(Ordering::Acquire);
    let policy = CapturePolicy {
        capture_dir: Some(dir.0.clone()),
        ..Default::default()
    };
    let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
    let (stdout, stderr) = session.finish(Duration::ZERO).await;
    assert!(stdout.writer_finished && stderr.writer_finished);
    assert_eq!(stdout.sha256, Some(Sha256::digest(b"").into()));
    assert_eq!(stderr.sha256, Some(Sha256::digest(b"").into()));
    assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
}
