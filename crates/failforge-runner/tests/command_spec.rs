#![cfg(unix)]

use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, SpecError, TimeoutPolicy};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn empty_program_is_rejected() {
    let spec = CommandSpec {
        program: OsString::new(),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    let result = spec.validate();

    assert_eq!(result, Err(SpecError::EmptyProgram));
}

#[test]
fn nonempty_program_is_accepted() {
    let spec = CommandSpec {
        program: OsString::from("failforge-test-program"),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    let result = spec.validate();

    assert_eq!(result, Ok(()));
}

#[test]
fn program_with_nul_is_rejected() {
    let spec = CommandSpec {
        program: OsString::from("tool\0name"),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Err(SpecError::ProgramContainsNul));
}

#[test]
fn non_utf8_program_is_accepted() {
    let spec = CommandSpec {
        program: OsString::from_vec(b"tool-\xff".to_vec()),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Ok(()));
}

#[test]
fn argument_with_nul_is_rejected() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![OsString::from("--mode"), OsString::from("fast\0debug")],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(
        spec.validate(),
        Err(SpecError::ArgumentContainsNul { index: 1 })
    );
}

#[test]
fn empty_argument_is_accepted() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![OsString::from("--mode"), OsString::from("")],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Ok(()));
}

#[test]
fn non_utf8_argument_is_accepted() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![
            OsString::from("--mode"),
            OsString::from_vec(b"fast-\xff".to_vec()),
        ],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Ok(()));
}

#[test]
fn cwd_with_nul_is_rejected() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![],
        cwd: PathBuf::from("work\0dir"),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Err(SpecError::CwdContainsNul));
}

#[test]
fn non_utf8_cwd_is_accepted() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![],
        cwd: PathBuf::from(OsString::from_vec(b"work-\xff".to_vec())),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Ok(()));
}

#[test]
fn argument_error_message_includes_index() {
    for (args, expected_message) in [
        (
            vec![OsString::from("bad\0value")],
            "argument at index 0 contains a NUL byte",
        ),
        (
            vec![
                OsString::from("--mode"),
                OsString::from("fast"),
                OsString::from("bad\0value"),
            ],
            "argument at index 2 contains a NUL byte",
        ),
    ] {
        let spec = CommandSpec {
            program: OsString::from("tool"),
            args,
            cwd: PathBuf::from("."),
            env: EnvSpec::default(),
            capture_policy: CapturePolicy::default(),
            timeout_policy: TimeoutPolicy::default(),
        };

        let error = spec
            .validate()
            .expect_err("argument with NUL must be rejected");

        assert_eq!(format!("{error}"), expected_message);
    }
}

#[test]
fn first_argument_with_nul_is_reported() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![
            OsString::from("valid"),
            OsString::from("first\0bad"),
            OsString::from("second\0bad"),
        ],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(
        spec.validate(),
        Err(SpecError::ArgumentContainsNul { index: 1 })
    );
}

#[test]
fn empty_program_takes_priority_over_argument_and_cwd_errors() {
    let spec = CommandSpec {
        program: OsString::new(),
        args: vec![OsString::from("bad\0argument")],
        cwd: PathBuf::from("work\0dir"),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Err(SpecError::EmptyProgram));
}

#[test]
fn program_nul_takes_priority_over_argument_and_cwd_errors() {
    let spec = CommandSpec {
        program: OsString::from("tool\0name"),
        args: vec![OsString::from("bad\0argument")],
        cwd: PathBuf::from("work\0dir"),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Err(SpecError::ProgramContainsNul));
}

#[test]
fn argument_nul_takes_priority_over_cwd_error() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![OsString::from("\0")],
        cwd: PathBuf::from("work\0dir"),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(
        spec.validate(),
        Err(SpecError::ArgumentContainsNul { index: 0 })
    );
}

#[test]
fn non_utf8_program_with_leading_nul_is_rejected() {
    let spec = CommandSpec {
        program: OsString::from_vec(b"\0\xff".to_vec()),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Err(SpecError::ProgramContainsNul));
}

#[test]
fn first_non_utf8_argument_with_trailing_nul_is_rejected() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![OsString::from_vec(b"\xff\0".to_vec())],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(
        spec.validate(),
        Err(SpecError::ArgumentContainsNul { index: 0 })
    );
}

#[test]
fn non_utf8_cwd_with_nul_is_rejected() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![],
        cwd: PathBuf::from(OsString::from_vec(b"work-\xff\0dir".to_vec())),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    };

    assert_eq!(spec.validate(), Err(SpecError::CwdContainsNul));
}

#[test]
fn command_rejects_zero_timeout() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            ..TimeoutPolicy::default()
        },
    };

    assert_eq!(spec.validate(), Err(SpecError::ZeroTimeout));
}

#[test]
fn command_accepts_positive_timeout() {
    for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
        let spec = CommandSpec {
            program: OsString::from("tool"),
            args: vec![],
            cwd: PathBuf::from("."),
            env: EnvSpec::default(),
            capture_policy: CapturePolicy::default(),
            timeout_policy: TimeoutPolicy {
                timeout: Some(Duration::from_nanos(1)),
                grace_period,
                ..TimeoutPolicy::default()
            },
        };

        assert_eq!(spec.validate(), Ok(()));
    }
}

#[test]
fn empty_program_precedes_timeout_error() {
    let spec = CommandSpec {
        program: OsString::new(),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            ..TimeoutPolicy::default()
        },
    };

    assert_eq!(spec.validate(), Err(SpecError::EmptyProgram));
}

#[test]
fn program_nul_precedes_timeout_error() {
    let spec = CommandSpec {
        program: OsString::from("tool\0name"),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            ..TimeoutPolicy::default()
        },
    };

    assert_eq!(spec.validate(), Err(SpecError::ProgramContainsNul));
}

#[test]
fn argument_nul_precedes_timeout_error() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![OsString::from("bad\0argument")],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            ..TimeoutPolicy::default()
        },
    };

    assert_eq!(
        spec.validate(),
        Err(SpecError::ArgumentContainsNul { index: 0 })
    );
}

#[test]
fn cwd_nul_precedes_timeout_error() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![],
        cwd: PathBuf::from("work\0dir"),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            ..TimeoutPolicy::default()
        },
    };

    assert_eq!(spec.validate(), Err(SpecError::CwdContainsNul));
}

#[test]
fn command_rejects_zero_timeout_with_zero_grace() {
    let spec = CommandSpec {
        program: OsString::from("tool"),
        args: vec![],
        cwd: PathBuf::from("."),
        env: EnvSpec::default(),
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            grace_period: Duration::ZERO,
            ..TimeoutPolicy::default()
        },
    };

    assert_eq!(spec.validate(), Err(SpecError::ZeroTimeout));
}
