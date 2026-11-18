use super::super::lock;
use super::super::test_support::state;
use super::*;

#[test]
fn empty_tail_has_no_truncation() {
    for limit in [0, 1, 32_768] {
        let mut tail = TailBuffer::new(limit);
        tail.push(&[]);
        assert!(tail.snapshot().is_empty());
        assert!(!tail.truncated);
    }
}

#[test]
fn zero_tail_retains_nothing_and_reports_real_output() {
    let mut tail = TailBuffer::new(0);
    tail.push(b"abc");
    assert!(tail.snapshot().is_empty());
    assert!(tail.truncated);
}

#[test]
fn tail_preserves_raw_bytes_at_exact_boundary() {
    let mut tail = TailBuffer::new(4);
    tail.push(&[0, 0xff]);
    tail.push(&[b'\n', 0x80]);
    assert_eq!(tail.snapshot(), [0, 0xff, b'\n', 0x80]);
    assert!(!tail.truncated);
}

#[test]
fn oversized_read_keeps_only_its_suffix() {
    let mut tail = TailBuffer::new(3);
    tail.push(b"a");
    tail.push(b"0123456789");
    assert_eq!(tail.snapshot(), b"789");
    assert!(tail.truncated);
}

#[test]
fn replacing_full_tail_marks_prior_bytes_lost() {
    let mut tail = TailBuffer::new(3);
    tail.push(b"abc");
    assert!(!tail.truncated);
    tail.push(b"def");
    assert_eq!(tail.snapshot(), b"def");
    assert!(tail.truncated);
}

#[test]
fn every_read_partition_produces_the_same_bounded_suffix() {
    let bytes: Vec<u8> = (0..10_000).map(|index| (index % 256) as u8).collect();
    for limit in [0, 1, 7, 256, 10_000, 10_001] {
        for read_size in [1, 3, 257, 4096, 10_000] {
            let mut tail = TailBuffer::new(limit);
            for read in bytes.chunks(read_size) {
                tail.push(read);
                assert!(tail.bytes.len() <= limit);
            }
            assert_eq!(tail.snapshot(), bytes[bytes.len().saturating_sub(limit)..]);
            assert_eq!(tail.truncated, bytes.len() > limit);
        }
    }
}

#[test]
fn capture_state_counts_all_raw_bytes_and_preserves_first_issue() {
    let mut state = CaptureState {
        summary: CaptureSummary::default(),
        tail: TailBuffer::new(3),
        sha256: Sha256::new(),
    };
    state.record(&[0, 255, 128]);
    state.record(b"xy");
    state.issue(CaptureIssue::FileQuotaExceeded);
    state.issue(CaptureIssue::DrainExpired);
    let summary = state.snapshot();
    assert_eq!(summary.bytes_read, 5);
    assert_eq!(summary.tail, [128, b'x', b'y']);
    assert!(summary.tail_truncated);
    assert_eq!(summary.issue, Some(CaptureIssue::FileQuotaExceeded));
    assert_eq!(
        summary.sha256,
        Some(Sha256::digest([0, 255, 128, b'x', b'y']).into())
    );
}

#[test]
fn pipe_digest_matches_known_vectors_independent_of_read_partition_and_tail() {
    let binary: Vec<u8> = (0..=255).collect();
    for (bytes, expected) in [
        (
            b"".as_slice(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
        ),
        (
            binary.as_slice(),
            "40aff2e9d2d8922e47afd4648e6967497158785fbd1da870e7110266bf944880",
        ),
    ] {
        for read_size in [1, 3, 64, 1024] {
            let capture = state(0);
            let mut capture = lock(&capture);
            for chunk in bytes.chunks(read_size) {
                capture.record(chunk);
            }
            let snapshot = capture.snapshot();
            let hex: String = snapshot
                .sha256
                .unwrap()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            assert_eq!(hex, expected, "read partition {read_size}");
            assert_eq!(snapshot.bytes_read, bytes.len() as u64);
            assert!(snapshot.tail.is_empty());
        }
    }
    // Default caller-provided metadata is distinguishable from an actual
    // native snapshot of an empty pipe.
    assert_eq!(CaptureSummary::default().sha256, None);
}

#[test]
fn pipe_digest_snapshot_is_immutable_and_records_failed_capture_prefixes() {
    let capture = state(1);
    let mut capture = lock(&capture);
    capture.record(b"abc");
    let first = capture.snapshot();
    capture.record(b"def");
    capture.issue(CaptureIssue::DrainExpired);
    let partial = capture.snapshot();
    assert_eq!(first.sha256, Some(Sha256::digest(b"abc").into()));
    assert_eq!(first.bytes_read, 3);
    assert_eq!(partial.sha256, Some(Sha256::digest(b"abcdef").into()));
    assert_eq!(partial.bytes_read, 6);
    assert!(!partial.eof);
    assert_eq!(partial.issue, Some(CaptureIssue::DrainExpired));
}

#[test]
fn capture_counter_overflow_is_explicit() {
    let state = state(2);
    lock(&state).summary.bytes_read = u64::MAX;
    lock(&state).record(b"x");
    assert_eq!(lock(&state).summary.bytes_read, u64::MAX);
    assert_eq!(
        lock(&state).summary.issue,
        Some(CaptureIssue::CounterOverflow)
    );
}
