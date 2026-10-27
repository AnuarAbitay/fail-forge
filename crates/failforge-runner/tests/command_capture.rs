#![cfg(unix)]

use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, OutputMode, SpecError, TimeoutPolicy};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Duration;

fn command_with_capture(capture_policy: CapturePolicy) -> CommandSpec {
    CommandSpec {
        program: "./tool".into(),
        args: vec![],
        cwd: ".".into(),
        env: EnvSpec::default(),
        timeout_policy: TimeoutPolicy::default(),
        capture_policy,
    }
}

fn custom_capture_policy() -> CapturePolicy {
    CapturePolicy {
        mode: OutputMode::StreamAndCapture,
        read_chunk_bytes: 1,
        tail_bytes_per_stream: 3,
        stream_queue_bytes: 17,
        file_quota_bytes_per_stream: 23,
        capture_dir: Some(PathBuf::from(OsString::from_vec(b"capture-\xff".to_vec()))),
    }
}

fn invalid_capture_cases() -> [(CapturePolicy, SpecError); 4] {
    [
        (
            CapturePolicy {
                mode: OutputMode::Capture,
                read_chunk_bytes: 0,
                ..CapturePolicy::default()
            },
            SpecError::ZeroReadChunk,
        ),
        (
            CapturePolicy {
                mode: OutputMode::StreamAndCapture,
                read_chunk_bytes: 0,
                capture_dir: Some("artifacts".into()),
                ..CapturePolicy::default()
            },
            SpecError::ZeroReadChunk,
        ),
        (
            CapturePolicy {
                mode: OutputMode::Capture,
                capture_dir: Some("artifacts\0capture".into()),
                ..CapturePolicy::default()
            },
            SpecError::CaptureDirContainsNul,
        ),
        (
            CapturePolicy {
                mode: OutputMode::StreamAndCapture,
                read_chunk_bytes: 1,
                capture_dir: Some(PathBuf::from(OsString::from_vec(
                    b"artifacts-\xff\0capture".to_vec(),
                ))),
                ..CapturePolicy::default()
            },
            SpecError::CaptureDirContainsNul,
        ),
    ]
}

#[test]
fn command_propagates_capture_errors() {
    for timeout in [None, Some(Duration::from_nanos(1))] {
        for (policy, error) in invalid_capture_cases() {
            let mut spec = command_with_capture(policy);
            spec.timeout_policy.timeout = timeout;

            assert_eq!(
                spec.validate(),
                Err(error),
                "mode={:?}, timeout={timeout:?}",
                spec.capture_policy.mode
            );
        }
    }
}

#[test]
fn command_accepts_custom_capture_policy() {
    for timeout in [None, Some(Duration::from_nanos(1))] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let mut spec = command_with_capture(custom_capture_policy());
            spec.capture_policy.mode = mode;
            spec.timeout_policy.timeout = timeout;

            assert_eq!(
                spec.validate(),
                Ok(()),
                "mode={:?}, timeout={timeout:?}",
                spec.capture_policy.mode
            );
        }
    }
}

#[test]
fn command_field_errors_precede_capture_errors() {
    for (program, args, cwd, error) in [
        (
            OsString::new(),
            vec![],
            PathBuf::from("."),
            SpecError::EmptyProgram,
        ),
        (
            OsString::from("tool\0name"),
            vec![],
            PathBuf::from("."),
            SpecError::ProgramContainsNul,
        ),
        (
            OsString::from("./tool"),
            vec![OsString::from("valid"), OsString::from("bad\0argument")],
            PathBuf::from("."),
            SpecError::ArgumentContainsNul { index: 1 },
        ),
        (
            OsString::from("./tool"),
            vec![],
            PathBuf::from("work\0dir"),
            SpecError::CwdContainsNul,
        ),
    ] {
        let expected = Err(error);
        for (policy, capture_error) in invalid_capture_cases() {
            let mut spec = command_with_capture(policy);
            spec.program = program.clone();
            spec.args = args.clone();
            spec.cwd = cwd.clone();

            assert_eq!(
                spec.validate(),
                expected,
                "capture_error={capture_error:?}, mode={:?}",
                spec.capture_policy.mode
            );
        }
    }
}

#[test]
fn environment_key_errors_precede_capture_errors() {
    for clear in [false, true] {
        for (key, error) in [
            ("", SpecError::EmptyEnvKey),
            ("BAD\0KEY", SpecError::EnvKeyContainsNul),
            ("BAD=KEY", SpecError::EnvKeyContainsEquals),
        ] {
            let expected = Err(error);
            for key_in_set in [false, true] {
                for (policy, capture_error) in invalid_capture_cases() {
                    let mut spec = command_with_capture(policy);
                    spec.env.clear = clear;
                    if key_in_set {
                        spec.env.set.insert(key.into(), "value".into());
                    } else {
                        spec.env.remove.insert(key.into());
                    }

                    assert_eq!(
                        spec.validate(),
                        expected,
                        "key={key:?}, key_in_set={key_in_set}, clear={clear}, \
                         capture_error={capture_error:?}, mode={:?}",
                        spec.capture_policy.mode
                    );
                }
            }
        }
    }
}

#[test]
fn environment_value_and_conflict_errors_precede_capture_errors() {
    for clear in [false, true] {
        for (conflict, error) in [
            (false, SpecError::EnvValueContainsNul),
            (true, SpecError::EnvKeyConflict),
        ] {
            let expected = Err(error);
            for (policy, capture_error) in invalid_capture_cases() {
                let mut spec = command_with_capture(policy);
                spec.env.clear = clear;
                if conflict {
                    spec.env.set.insert("KEY".into(), "value".into());
                    spec.env.remove.insert("KEY".into());
                } else {
                    spec.env.set.insert("KEY".into(), "bad\0value".into());
                }

                assert_eq!(
                    spec.validate(),
                    expected,
                    "clear={clear}, capture_error={capture_error:?}, mode={:?}",
                    spec.capture_policy.mode
                );
            }
        }
    }
}

#[test]
fn cleared_environment_path_error_precedes_capture_errors() {
    for (policy, capture_error) in invalid_capture_cases() {
        let mut spec = command_with_capture(policy);
        spec.program = "tool".into();
        spec.env.clear = true;

        assert_eq!(
            spec.validate(),
            Err(SpecError::ClearEnvRequiresPath),
            "capture_error={capture_error:?}, mode={:?}",
            spec.capture_policy.mode
        );
    }
}

#[test]
fn zero_timeout_precedes_capture_errors() {
    for clear in [false, true] {
        for (policy, capture_error) in invalid_capture_cases() {
            let mut spec = command_with_capture(policy);
            spec.env.clear = clear;
            spec.timeout_policy.timeout = Some(Duration::ZERO);

            assert_eq!(
                spec.validate(),
                Err(SpecError::ZeroTimeout),
                "clear={clear}, capture_error={capture_error:?}, mode={:?}",
                spec.capture_policy.mode
            );
        }
    }
}

#[test]
fn custom_capture_policy_does_not_skip_timeout_validation() {
    for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let mut spec = command_with_capture(custom_capture_policy());
            spec.capture_policy.mode = mode;
            spec.timeout_policy.timeout = Some(Duration::ZERO);
            spec.timeout_policy.grace_period = grace_period;

            assert_eq!(
                spec.validate(),
                Err(SpecError::ZeroTimeout),
                "mode={:?}, grace_period={grace_period:?}",
                spec.capture_policy.mode
            );
        }
    }
}

#[test]
fn command_preserves_capture_error_priority() {
    for timeout in [None, Some(Duration::from_nanos(1))] {
        for mode in [OutputMode::Capture, OutputMode::StreamAndCapture] {
            let policy = CapturePolicy {
                mode,
                read_chunk_bytes: 0,
                capture_dir: Some(PathBuf::from(OsString::from_vec(
                    b"capture-\xff\0dir".to_vec(),
                ))),
                ..CapturePolicy::default()
            };
            let mut spec = command_with_capture(policy);
            spec.timeout_policy.timeout = timeout;

            assert_eq!(
                spec.validate(),
                Err(SpecError::ZeroReadChunk),
                "mode={:?}, timeout={timeout:?}",
                spec.capture_policy.mode
            );
        }
    }
}
