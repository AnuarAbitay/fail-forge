#![cfg(unix)]

use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, SpecError, TimeoutPolicy};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::time::Duration;

fn command_with_env(env: EnvSpec) -> CommandSpec {
    CommandSpec {
        program: "./tool".into(),
        args: vec![],
        cwd: ".".into(),
        env,
        capture_policy: CapturePolicy::default(),
        timeout_policy: TimeoutPolicy::default(),
    }
}

fn custom_environment(clear: bool) -> EnvSpec {
    let mut env = EnvSpec {
        clear,
        ..EnvSpec::default()
    };
    env.set.insert("MODE".into(), "test".into());
    env.remove.insert("OLD_MODE".into());
    env
}

fn environment_with_nul_value(clear: bool) -> EnvSpec {
    let mut env = custom_environment(clear);
    env.set.insert("MODE".into(), "bad\0value".into());
    env
}

#[test]
fn command_accepts_custom_environment() {
    for clear in [false, true] {
        let mut env = custom_environment(clear);
        env.set.insert("EMPTY_VALUE".into(), OsString::new());
        env.set.insert("EQUALS_VALUE".into(), "a=b=c".into());
        env.set.insert(
            OsString::from_vec(b"NEW-\xff".to_vec()),
            OsString::from_vec(b"value-\xfe".to_vec()),
        );
        env.remove.insert(OsString::from_vec(b"OLD-\xff".to_vec()));
        let mut spec = command_with_env(env);
        spec.args = vec!["--mode".into(), "fast".into()];

        assert_eq!(spec.validate(), Ok(()), "clear={clear}");
    }
}

#[test]
fn command_accepts_only_environment_removals() {
    for clear in [false, true] {
        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.remove.insert("OLD_MODE".into());
        env.remove.insert(OsString::from_vec(b"OLD-\xfe".to_vec()));

        assert_eq!(command_with_env(env).validate(), Ok(()), "clear={clear}");
    }
}

#[test]
fn command_propagates_environment_key_errors() {
    for clear in [false, true] {
        for (key, error) in [
            ("", SpecError::EmptyEnvKey),
            ("BAD\0KEY", SpecError::EnvKeyContainsNul),
            ("BAD=KEY", SpecError::EnvKeyContainsEquals),
        ] {
            let expected = Err(error);
            for invalid_key_in_set in [false, true] {
                let mut env = EnvSpec {
                    clear,
                    ..EnvSpec::default()
                };
                if invalid_key_in_set {
                    env.set.insert(key.into(), "value".into());
                } else {
                    env.remove.insert(key.into());
                }

                assert_eq!(
                    command_with_env(env).validate(),
                    expected,
                    "key {key:?}, invalid_key_in_set={invalid_key_in_set}, clear={clear}"
                );
            }
        }
    }
}

#[test]
fn command_propagates_environment_value_error() {
    for clear in [false, true] {
        let mut spec = command_with_env(environment_with_nul_value(clear));
        spec.args = vec!["--mode".into(), "fast".into()];

        assert_eq!(
            spec.validate(),
            Err(SpecError::EnvValueContainsNul),
            "clear={clear}"
        );
    }
}

#[test]
fn command_propagates_environment_conflict_error() {
    for clear in [false, true] {
        let mut env = custom_environment(clear);
        env.remove.insert("MODE".into());

        assert_eq!(
            command_with_env(env).validate(),
            Err(SpecError::EnvKeyConflict),
            "clear={clear}"
        );
    }
}

#[test]
fn empty_program_precedes_environment_error() {
    for clear in [false, true] {
        let mut spec = command_with_env(environment_with_nul_value(clear));
        spec.program = OsString::new();

        assert_eq!(
            spec.validate(),
            Err(SpecError::EmptyProgram),
            "clear={clear}"
        );
    }
}

#[test]
fn program_nul_precedes_environment_error() {
    for clear in [false, true] {
        let mut spec = command_with_env(environment_with_nul_value(clear));
        spec.program = "tool\0name".into();

        assert_eq!(
            spec.validate(),
            Err(SpecError::ProgramContainsNul),
            "clear={clear}"
        );
    }
}

#[test]
fn argument_nul_precedes_environment_error() {
    for clear in [false, true] {
        let mut spec = command_with_env(environment_with_nul_value(clear));
        spec.args = vec!["--mode".into(), "bad\0argument".into()];

        assert_eq!(
            spec.validate(),
            Err(SpecError::ArgumentContainsNul { index: 1 }),
            "clear={clear}"
        );
    }
}

#[test]
fn cwd_nul_precedes_environment_error() {
    for clear in [false, true] {
        let mut spec = command_with_env(environment_with_nul_value(clear));
        spec.cwd = "work\0dir".into();

        assert_eq!(
            spec.validate(),
            Err(SpecError::CwdContainsNul),
            "clear={clear}"
        );
    }
}

#[test]
fn environment_errors_precede_timeout_error() {
    for clear in [false, true] {
        for (key, value, also_remove, expected) in [
            ("", "value", false, SpecError::EmptyEnvKey),
            ("BAD\0KEY", "value", false, SpecError::EnvKeyContainsNul),
            ("BAD=KEY", "value", false, SpecError::EnvKeyContainsEquals),
            ("MODE", "bad\0value", false, SpecError::EnvValueContainsNul),
            ("MODE", "value", true, SpecError::EnvKeyConflict),
        ] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert(key.into(), value.into());
            if also_remove {
                env.remove.insert(key.into());
            }
            let mut spec = command_with_env(env);
            spec.timeout_policy.timeout = Some(Duration::ZERO);

            assert_eq!(spec.validate(), Err(expected), "clear={clear}");
        }
    }
}

#[test]
fn valid_environment_does_not_bypass_timeout_validation() {
    for clear in [false, true] {
        for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
            let mut spec = command_with_env(custom_environment(clear));
            spec.timeout_policy = TimeoutPolicy {
                timeout: Some(Duration::ZERO),
                grace_period,
                ..TimeoutPolicy::default()
            };

            assert_eq!(
                spec.validate(),
                Err(SpecError::ZeroTimeout),
                "clear={clear}, grace_period={grace_period:?}"
            );
        }
    }
}

#[test]
fn valid_environment_with_positive_timeout_is_accepted() {
    for clear in [false, true] {
        for grace_period in [Duration::ZERO, Duration::from_secs(2)] {
            let mut spec = command_with_env(custom_environment(clear));
            spec.timeout_policy = TimeoutPolicy {
                timeout: Some(Duration::from_nanos(1)),
                grace_period,
                ..TimeoutPolicy::default()
            };

            assert_eq!(
                spec.validate(),
                Ok(()),
                "clear={clear}, grace_period={grace_period:?}"
            );
        }
    }
}
