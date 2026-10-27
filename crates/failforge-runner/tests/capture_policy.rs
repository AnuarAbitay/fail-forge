#![cfg(unix)]

use failforge_runner::{CapturePolicy, OutputMode, SpecError};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

#[test]
fn default_output_mode_is_capture() {
    assert_eq!(OutputMode::default(), OutputMode::Capture);
}

#[test]
fn default_capture_policy_matches_contract() {
    let policy = CapturePolicy::default();

    assert_eq!(policy.mode, OutputMode::Capture);
    assert_eq!(policy.read_chunk_bytes, 32_768);
    assert_eq!(policy.tail_bytes_per_stream, 1_048_576);
    assert_eq!(policy.stream_queue_bytes, 1_048_576);
    assert_eq!(policy.file_quota_bytes_per_stream, 268_435_456);
    assert!(policy.capture_dir.is_none());
}

#[test]
fn default_capture_policy_is_valid() {
    assert_eq!(CapturePolicy::default().validate(), Ok(()));
}

#[test]
fn zero_read_chunk_is_rejected() {
    for capture_dir in [None, Some(PathBuf::from("artifacts"))] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let policy = CapturePolicy {
                mode,
                read_chunk_bytes: 0,
                capture_dir: capture_dir.clone(),
                ..CapturePolicy::default()
            };

            assert_eq!(
                policy.validate(),
                Err(SpecError::ZeroReadChunk),
                "mode={:?}, capture_dir={:?}",
                policy.mode,
                policy.capture_dir
            );
        }
    }
}

#[test]
fn positive_read_chunk_sizes_are_accepted() {
    for read_chunk_bytes in [1, 32_768, 65_536] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let policy = CapturePolicy {
                mode,
                read_chunk_bytes,
                ..CapturePolicy::default()
            };

            assert_eq!(
                policy.validate(),
                Ok(()),
                "mode={:?}, read_chunk_bytes={read_chunk_bytes}",
                policy.mode
            );
        }
    }
}

#[test]
fn capture_directory_without_nul_is_accepted() {
    for directory in [
        "",
        "artifacts",
        "./artifacts",
        "/tmp/failforge-capture",
        "with spaces",
    ] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let policy = CapturePolicy {
                mode,
                capture_dir: Some(PathBuf::from(directory)),
                ..CapturePolicy::default()
            };

            assert_eq!(
                policy.validate(),
                Ok(()),
                "mode={:?}, directory={directory:?}",
                policy.mode
            );
        }
    }
}

#[test]
fn non_utf8_capture_directory_is_accepted() {
    for bytes in [b"\xff".as_slice(), b"artifacts-\xff".as_slice()] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let policy = CapturePolicy {
                mode,
                capture_dir: Some(PathBuf::from(OsString::from_vec(bytes.to_vec()))),
                ..CapturePolicy::default()
            };

            assert_eq!(
                policy.validate(),
                Ok(()),
                "mode={:?}, directory bytes={bytes:?}",
                policy.mode
            );
        }
    }
}

#[test]
fn capture_directory_with_nul_is_rejected() {
    for bytes in [
        b"\0".as_slice(),
        b"\0artifacts".as_slice(),
        b"art\0ifacts".as_slice(),
        b"artifacts\0".as_slice(),
        b"\0\xff".as_slice(),
        b"\xff\0".as_slice(),
        b"art-\xff\0dir".as_slice(),
    ] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let policy = CapturePolicy {
                mode,
                capture_dir: Some(PathBuf::from(OsString::from_vec(bytes.to_vec()))),
                ..CapturePolicy::default()
            };

            assert_eq!(
                policy.validate(),
                Err(SpecError::CaptureDirContainsNul),
                "mode={:?}, directory bytes={bytes:?}",
                policy.mode
            );
        }
    }
}

#[test]
fn zero_read_chunk_precedes_capture_directory_error() {
    for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
        let policy = CapturePolicy {
            mode,
            read_chunk_bytes: 0,
            capture_dir: Some(PathBuf::from("artifacts\0dir")),
            ..CapturePolicy::default()
        };

        assert_eq!(
            policy.validate(),
            Err(SpecError::ZeroReadChunk),
            "mode={:?}",
            policy.mode
        );
    }
}

#[test]
fn capture_policy_errors_have_expected_messages() {
    for (read_chunk_bytes, capture_dir, expected_message) in [
        (0, None, "read chunk size must be greater than zero"),
        (
            1,
            Some(PathBuf::from("capture\0private-dir")),
            "capture directory contains a NUL byte",
        ),
    ] {
        let policy = CapturePolicy {
            read_chunk_bytes,
            capture_dir,
            ..CapturePolicy::default()
        };
        let error = policy
            .validate()
            .expect_err("invalid capture policy must be rejected");

        assert_eq!(error.to_string(), expected_message);
    }
}
