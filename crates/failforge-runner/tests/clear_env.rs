#![cfg(unix)]

use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, SpecError, TimeoutPolicy};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::time::Duration;

fn command(program: OsString, clear: bool) -> CommandSpec {
    CommandSpec {
        program,
        args: vec![],
        cwd: ".".into(),
        env: EnvSpec {
            clear,
            ..EnvSpec::default()
        },
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    }
}

#[test]
fn uncleared_environment_does_not_require_explicit_path() {
    for program in ["tool", "tool.exe", "dir\\tool", "dir／tool"] {
        for remove_path in [false, true] {
            let mut spec = command(program.into(), false);
            if remove_path {
                spec.env.remove.insert("PATH".into());
            }

            assert_eq!(
                spec.validate(),
                Ok(()),
                "program {program:?}, remove_path={remove_path}"
            );
        }
    }
}

#[test]
fn cleared_environment_rejects_bare_program_without_path() {
    for program in ["tool", "tool.exe", "dir\\tool", "dir／tool"] {
        assert_eq!(
            command(program.into(), true).validate(),
            Err(SpecError::ClearEnvRequiresPath),
            "program {program:?}"
        );
    }
}

#[test]
fn cleared_environment_accepts_program_with_slash() {
    for program in ["./tool", "../tool", "tools/tool", "/usr/bin/tool", "tool/"] {
        assert_eq!(
            command(program.into(), true).validate(),
            Ok(()),
            "program {program:?}"
        );
    }
}

#[test]
fn cleared_environment_accepts_non_utf8_program_with_slash() {
    for bytes in [
        b"/\xff".as_slice(),
        b"\xff/tool".as_slice(),
        b"\xff/".as_slice(),
        b"./tool-\xfe".as_slice(),
    ] {
        assert_eq!(
            command(OsString::from_vec(bytes.to_vec()), true).validate(),
            Ok(()),
            "program bytes {bytes:?}"
        );
    }
}

#[test]
fn cleared_environment_rejects_non_utf8_bare_program_without_path() {
    for bytes in [b"\xff".as_slice(), b"tool-\xff".as_slice()] {
        assert_eq!(
            command(OsString::from_vec(bytes.to_vec()), true).validate(),
            Err(SpecError::ClearEnvRequiresPath),
            "program bytes {bytes:?}"
        );
    }
}

#[test]
fn explicit_path_accepts_empty_and_non_utf8_values() {
    for program in ["tool", "./tool"] {
        for value in [
            OsString::new(),
            OsString::from("/usr/bin:/bin"),
            OsString::from_vec(b"/tools-\xff".to_vec()),
        ] {
            let mut spec = command(program.into(), true);
            spec.env.set.insert("PATH".into(), value);

            assert_eq!(spec.validate(), Ok(()), "program {program:?}");
        }
    }
}

#[test]
fn path_lookalike_keys_do_not_satisfy_requirement() {
    for key in [
        OsString::from("path"),
        OsString::from("Path"),
        OsString::from("PATH_SUFFIX"),
        OsString::from("SOME_PATH"),
        OsString::from(" PATH"),
        OsString::from("PATH "),
        OsString::from_vec(b"PATH-\xff".to_vec()),
        OsString::from_vec(b"\xffPATH".to_vec()),
    ] {
        let mut spec = command("tool".into(), true);
        spec.env.set.insert(key.clone(), "/usr/bin:/bin".into());

        assert_eq!(
            spec.validate(),
            Err(SpecError::ClearEnvRequiresPath),
            "key {key:?}"
        );
    }
}

#[test]
fn path_removal_does_not_supply_explicit_path() {
    for (program, expected) in [
        ("tool", Err(SpecError::ClearEnvRequiresPath)),
        ("./tool", Ok(())),
    ] {
        let mut spec = command(program.into(), true);
        spec.env.remove.insert("PATH".into());

        assert_eq!(spec.validate(), expected, "program {program:?}");
    }
}

#[test]
fn environment_key_errors_are_not_masked_by_path_rule() {
    for has_explicit_path in [false, true] {
        for (key, error) in [
            ("", SpecError::EmptyEnvKey),
            ("BAD\0KEY", SpecError::EnvKeyContainsNul),
            ("BAD=KEY", SpecError::EnvKeyContainsEquals),
        ] {
            let expected = Err(error);
            for invalid_key_in_set in [false, true] {
                let mut spec = command("tool".into(), true);
                if has_explicit_path {
                    spec.env.set.insert("PATH".into(), "/usr/bin:/bin".into());
                }
                if invalid_key_in_set {
                    spec.env.set.insert(key.into(), "value".into());
                } else {
                    spec.env.remove.insert(key.into());
                }

                assert_eq!(
                    spec.validate(),
                    expected,
                    "key {key:?}, invalid_key_in_set={invalid_key_in_set}, has_explicit_path={has_explicit_path}"
                );
            }
        }
    }
}

#[test]
fn environment_value_errors_are_not_masked_by_path_rule() {
    for has_explicit_path in [false, true] {
        let mut spec = command("tool".into(), true);
        spec.env.set.insert("MODE".into(), "bad\0value".into());
        if has_explicit_path {
            spec.env.set.insert("PATH".into(), "/usr/bin:/bin".into());
        }

        assert_eq!(
            spec.validate(),
            Err(SpecError::EnvValueContainsNul),
            "has_explicit_path={has_explicit_path}"
        );
    }

    let mut spec = command("tool".into(), true);
    spec.env.set.insert("PATH".into(), "/tools\0/bin".into());

    assert_eq!(spec.validate(), Err(SpecError::EnvValueContainsNul));
}

#[test]
fn environment_conflicts_are_not_masked_by_path_rule() {
    for key in ["OTHER", "PATH"] {
        let mut spec = command("tool".into(), true);
        spec.env.set.insert(key.into(), "/usr/bin:/bin".into());
        spec.env.remove.insert(key.into());

        assert_eq!(spec.validate(), Err(SpecError::EnvKeyConflict), "key {key}");
    }
}

#[test]
fn command_field_errors_are_not_masked_by_path_rule() {
    for (program, args, cwd, error) in [
        ("", vec![], ".", SpecError::EmptyProgram),
        ("tool\0name", vec![], ".", SpecError::ProgramContainsNul),
        (
            "tool",
            vec!["--mode", "bad\0argument"],
            ".",
            SpecError::ArgumentContainsNul { index: 1 },
        ),
        ("tool", vec![], "work\0dir", SpecError::CwdContainsNul),
    ] {
        let expected = Err(error);
        for has_explicit_path in [false, true] {
            let mut spec = command(program.into(), true);
            spec.args = args.iter().map(|arg| OsString::from(*arg)).collect();
            spec.cwd = cwd.into();
            if has_explicit_path {
                spec.env.set.insert("PATH".into(), "/usr/bin:/bin".into());
            }

            assert_eq!(
                spec.validate(),
                expected,
                "has_explicit_path={has_explicit_path}"
            );
        }
    }
}

#[test]
fn missing_path_precedes_zero_timeout() {
    for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
        let mut spec = command("tool".into(), true);
        spec.timeout_policy = TimeoutPolicy {
            timeout: Some(Duration::ZERO),
            grace_period,
            ..TimeoutPolicy::default()
        };

        assert_eq!(
            spec.validate(),
            Err(SpecError::ClearEnvRequiresPath),
            "grace_period={grace_period:?}"
        );
    }
}

#[test]
fn allowed_path_settings_do_not_bypass_timeout_validation() {
    for (program, path_value) in [
        ("./tool", None),
        ("tool", Some("/usr/bin:/bin")),
        ("tool", Some("")),
        ("./tool", Some("/usr/bin:/bin")),
    ] {
        for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
            let mut spec = command(program.into(), true);
            if let Some(value) = path_value {
                spec.env.set.insert("PATH".into(), value.into());
            }
            spec.timeout_policy = TimeoutPolicy {
                timeout: Some(Duration::ZERO),
                grace_period,
                ..TimeoutPolicy::default()
            };

            assert_eq!(
                spec.validate(),
                Err(SpecError::ZeroTimeout),
                "program {program:?}, path_value={path_value:?}, grace_period={grace_period:?}"
            );
        }
    }
}

#[test]
fn allowed_path_settings_accept_positive_timeout() {
    for (program, path_value) in [
        ("./tool", None),
        ("tool", Some("/usr/bin:/bin")),
        ("tool", Some("")),
        ("./tool", Some("/usr/bin:/bin")),
    ] {
        for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
            let mut spec = command(program.into(), true);
            if let Some(value) = path_value {
                spec.env.set.insert("PATH".into(), value.into());
            }
            spec.timeout_policy = TimeoutPolicy {
                timeout: Some(Duration::from_nanos(1)),
                grace_period,
                ..TimeoutPolicy::default()
            };

            assert_eq!(
                spec.validate(),
                Ok(()),
                "program {program:?}, path_value={path_value:?}, grace_period={grace_period:?}"
            );
        }
    }
}

#[test]
fn clear_env_requirement_error_has_expected_message() {
    let error = command("tool".into(), true)
        .validate()
        .expect_err("bare program with cleared environment needs an explicit PATH");

    assert_eq!(error, SpecError::ClearEnvRequiresPath);
    assert_eq!(
        error.to_string(),
        "cleared environment requires PATH or a program path"
    );
}
