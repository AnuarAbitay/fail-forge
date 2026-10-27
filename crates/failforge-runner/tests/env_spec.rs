#![cfg(unix)]

use failforge_runner::{EnvSpec, SpecError};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

#[test]
fn default_env_spec_inherits_without_overrides() {
    let env = EnvSpec::default();

    assert!(!env.clear);
    assert!(env.set.is_empty());
    assert!(env.remove.is_empty());
}

fn assert_key_validation(key: OsString, expected: Result<(), SpecError>) {
    for clear in [false, true] {
        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.set.insert(key.clone(), OsString::new());

        assert_eq!(env.validate(), expected, "set key {key:?}, clear={clear}");

        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.remove.insert(key.clone());

        assert_eq!(
            env.validate(),
            expected,
            "remove key {key:?}, clear={clear}"
        );
    }
}

#[test]
fn default_env_spec_is_valid() {
    assert_eq!(EnvSpec::default().validate(), Ok(()));
}

#[test]
fn valid_environment_keys_are_accepted() {
    for key in [
        "A",
        "lowercase",
        "123",
        "WITH SPACE",
        " ",
        "DASH-KEY",
        "DOT.KEY",
        "СЕРВИС",
    ] {
        assert_key_validation(key.into(), Ok(()));
    }
}

#[test]
fn non_utf8_environment_keys_are_accepted() {
    for bytes in [
        b"\xff".as_slice(),
        b"key-\xff".as_slice(),
        b"\xfe-tail".as_slice(),
    ] {
        assert_key_validation(OsString::from_vec(bytes.to_vec()), Ok(()));
    }
}

#[test]
fn empty_environment_key_is_rejected() {
    assert_key_validation(OsString::new(), Err(SpecError::EmptyEnvKey));
}

#[test]
fn environment_key_with_nul_is_rejected() {
    for key in ["\0", "\0KEY", "K\0EY", "KEY\0"] {
        assert_key_validation(key.into(), Err(SpecError::EnvKeyContainsNul));
    }
}

#[test]
fn non_utf8_environment_key_with_nul_is_rejected() {
    for bytes in [
        b"\0\xff".as_slice(),
        b"\xff\0".as_slice(),
        b"k\xff\0ey".as_slice(),
    ] {
        assert_key_validation(
            OsString::from_vec(bytes.to_vec()),
            Err(SpecError::EnvKeyContainsNul),
        );
    }
}

#[test]
fn environment_key_with_equals_is_rejected() {
    for key in ["=", "=KEY", "K=EY", "KEY="] {
        assert_key_validation(key.into(), Err(SpecError::EnvKeyContainsEquals));
    }
}

#[test]
fn non_utf8_environment_key_with_equals_is_rejected() {
    for bytes in [
        b"=\xff".as_slice(),
        b"\xff=".as_slice(),
        b"k\xff=ey".as_slice(),
    ] {
        assert_key_validation(
            OsString::from_vec(bytes.to_vec()),
            Err(SpecError::EnvKeyContainsEquals),
        );
    }
}

#[test]
fn nul_in_environment_key_precedes_equals_error() {
    for bytes in [
        b"=\0".as_slice(),
        b"\0=".as_slice(),
        b"BAD=\0KEY".as_slice(),
        b"BAD\0=KEY".as_slice(),
        b"\xff=\0".as_slice(),
    ] {
        assert_key_validation(
            OsString::from_vec(bytes.to_vec()),
            Err(SpecError::EnvKeyContainsNul),
        );
    }
}

#[test]
fn multiple_valid_assignments_and_removals_are_accepted() {
    for clear in [false, true] {
        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.set.insert("MODE".into(), "test".into());
        env.set.insert("EMPTY_VALUE".into(), OsString::new());
        env.remove.insert("OLD_MODE".into());
        env.remove.insert("OLD_OPTION".into());

        assert_eq!(env.validate(), Ok(()), "clear={clear}");
    }
}

#[test]
fn every_assignment_key_is_validated() {
    for clear in [false, true] {
        for (key, expected) in [
            ("A\0", SpecError::EnvKeyContainsNul),
            ("Z\0", SpecError::EnvKeyContainsNul),
            ("A=", SpecError::EnvKeyContainsEquals),
            ("Z=", SpecError::EnvKeyContainsEquals),
        ] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert("M_VALID".into(), "value".into());
            env.set.insert(key.into(), "value".into());

            assert_eq!(
                env.validate(),
                Err(expected),
                "set key {key:?}, clear={clear}"
            );
        }
    }
}

#[test]
fn every_removal_key_is_validated() {
    for clear in [false, true] {
        for (key, expected) in [
            ("A\0", SpecError::EnvKeyContainsNul),
            ("Z\0", SpecError::EnvKeyContainsNul),
            ("A=", SpecError::EnvKeyContainsEquals),
            ("Z=", SpecError::EnvKeyContainsEquals),
        ] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.remove.insert("M_VALID".into());
            env.remove.insert(key.into());

            assert_eq!(
                env.validate(),
                Err(expected),
                "remove key {key:?}, clear={clear}"
            );
        }
    }
}

#[test]
fn invalid_removal_key_is_rejected_after_valid_assignment() {
    for clear in [false, true] {
        for (key, expected) in [
            ("", SpecError::EmptyEnvKey),
            ("BAD\0KEY", SpecError::EnvKeyContainsNul),
            ("BAD=KEY", SpecError::EnvKeyContainsEquals),
        ] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert("MODE".into(), "test".into());
            env.remove.insert(key.into());

            assert_eq!(
                env.validate(),
                Err(expected),
                "remove key {key:?}, clear={clear}"
            );
        }
    }
}

#[test]
fn environment_key_errors_have_expected_messages() {
    for (error, expected) in [
        (
            SpecError::EmptyEnvKey,
            "environment variable key must not be empty",
        ),
        (
            SpecError::EnvKeyContainsNul,
            "environment variable key contains a NUL byte",
        ),
        (
            SpecError::EnvKeyContainsEquals,
            "environment variable key contains '='",
        ),
    ] {
        assert_eq!(error.to_string(), expected);
    }
}

fn assert_value_validation(value: OsString, expected: Result<(), SpecError>) {
    for clear in [false, true] {
        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.set.insert("KEY".into(), value.clone());

        assert_eq!(env.validate(), expected, "value {value:?}, clear={clear}");
    }
}

#[test]
fn valid_environment_values_are_accepted() {
    for value in ["", "plain", "=", "a=b=c", " ", "line\nnext", "значение"] {
        assert_value_validation(value.into(), Ok(()));
    }
}

#[test]
fn non_utf8_environment_values_are_accepted() {
    for bytes in [b"\xff".as_slice(), b"value-\xfe".as_slice()] {
        assert_value_validation(OsString::from_vec(bytes.to_vec()), Ok(()));
    }
}

#[test]
fn environment_value_with_nul_is_rejected() {
    for value in ["\0", "\0value", "va\0lue", "value\0"] {
        assert_value_validation(value.into(), Err(SpecError::EnvValueContainsNul));
    }
}

#[test]
fn non_utf8_environment_value_with_nul_is_rejected() {
    for bytes in [
        b"\0\xff".as_slice(),
        b"\xff\0".as_slice(),
        b"va\xff\0lue".as_slice(),
    ] {
        assert_value_validation(
            OsString::from_vec(bytes.to_vec()),
            Err(SpecError::EnvValueContainsNul),
        );
    }
}

#[test]
fn every_environment_value_is_validated() {
    for clear in [false, true] {
        for key in ["A_INVALID_VALUE", "Z_INVALID_VALUE"] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert("M_VALID_VALUE".into(), "valid".into());
            env.set.insert(key.into(), "invalid\0value".into());

            assert_eq!(
                env.validate(),
                Err(SpecError::EnvValueContainsNul),
                "value at key {key}, clear={clear}"
            );
        }
    }
}

#[test]
fn environment_key_errors_precede_value_errors() {
    for clear in [false, true] {
        for (key, error) in [
            ("", SpecError::EmptyEnvKey),
            ("Z\0KEY", SpecError::EnvKeyContainsNul),
            ("Z=KEY", SpecError::EnvKeyContainsEquals),
        ] {
            let expected = Err(error);
            for invalid_key_in_set in [false, true] {
                let mut env = EnvSpec {
                    clear,
                    ..EnvSpec::default()
                };
                env.set.insert("A_VALUE".into(), "bad\0value".into());
                if invalid_key_in_set {
                    env.set.insert(key.into(), "valid".into());
                } else {
                    env.remove.insert(key.into());
                }

                assert_eq!(
                    env.validate(),
                    expected,
                    "key {key:?}, invalid_key_in_set={invalid_key_in_set}, clear={clear}"
                );
            }
        }
    }
}

#[test]
fn assigned_and_removed_environment_key_is_rejected() {
    for clear in [false, true] {
        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.set.insert("CI".into(), "true".into());
        env.remove.insert("CI".into());

        assert_eq!(
            env.validate(),
            Err(SpecError::EnvKeyConflict),
            "clear={clear}"
        );
    }
}

#[test]
fn assigned_and_removed_non_utf8_key_is_rejected() {
    for clear in [false, true] {
        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        let key = OsString::from_vec(b"KEY-\xff".to_vec());
        env.set.insert(key.clone(), "value".into());
        env.remove.insert(key);

        assert_eq!(
            env.validate(),
            Err(SpecError::EnvKeyConflict),
            "clear={clear}"
        );
    }
}

#[test]
fn distinct_environment_keys_do_not_conflict() {
    for clear in [false, true] {
        for (set_key, remove_key) in [
            (OsString::from("KEY"), OsString::from("KEY_SUFFIX")),
            (OsString::from("PATH"), OsString::from("path")),
            (
                OsString::from_vec(b"KEY-\xff".to_vec()),
                OsString::from_vec(b"KEY-\xfe".to_vec()),
            ),
        ] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert(set_key, "value".into());
            env.remove.insert(remove_key);

            assert_eq!(env.validate(), Ok(()), "clear={clear}");
        }
    }
}

#[test]
fn every_assigned_key_is_checked_for_conflicts() {
    for clear in [false, true] {
        for key in ["A_MATCH", "Z_MATCH"] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert("M_VALID".into(), "value".into());
            env.set.insert(key.into(), "value".into());
            env.remove.insert(key.into());

            assert_eq!(
                env.validate(),
                Err(SpecError::EnvKeyConflict),
                "key {key}, clear={clear}"
            );
        }
    }
}

#[test]
fn conflict_is_found_among_multiple_removal_keys() {
    for clear in [false, true] {
        for key in ["A_MATCH", "Z_MATCH"] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert(key.into(), "value".into());
            env.remove.insert("M_REMOVE".into());
            env.remove.insert(key.into());

            assert_eq!(
                env.validate(),
                Err(SpecError::EnvKeyConflict),
                "key {key}, clear={clear}"
            );
        }

        let mut env = EnvSpec {
            clear,
            ..EnvSpec::default()
        };
        env.set.insert("A_SET".into(), "value".into());
        env.set.insert("Z_MATCH".into(), "value".into());
        env.remove.insert("A_REMOVE".into());
        env.remove.insert("B_REMOVE".into());
        env.remove.insert("Z_MATCH".into());

        assert_eq!(
            env.validate(),
            Err(SpecError::EnvKeyConflict),
            "clear={clear}"
        );
    }
}

#[test]
fn environment_key_errors_precede_conflict_error() {
    for clear in [false, true] {
        for (key, error) in [
            ("", SpecError::EmptyEnvKey),
            ("Z\0KEY", SpecError::EnvKeyContainsNul),
            ("Z=KEY", SpecError::EnvKeyContainsEquals),
        ] {
            let expected = Err(error);
            for invalid_key_in_set in [false, true] {
                let mut env = EnvSpec {
                    clear,
                    ..EnvSpec::default()
                };
                env.set.insert("A_CONFLICT".into(), "value".into());
                env.remove.insert("A_CONFLICT".into());
                if invalid_key_in_set {
                    env.set.insert(key.into(), "value".into());
                } else {
                    env.remove.insert(key.into());
                }

                assert_eq!(
                    env.validate(),
                    expected,
                    "key {key:?}, invalid_key_in_set={invalid_key_in_set}, clear={clear}"
                );
            }
        }
    }
}

#[test]
fn environment_value_error_precedes_conflict_error() {
    for clear in [false, true] {
        for value_key in ["A_CONFLICT", "Z_VALUE"] {
            let mut env = EnvSpec {
                clear,
                ..EnvSpec::default()
            };
            env.set.insert("A_CONFLICT".into(), "value".into());
            env.remove.insert("A_CONFLICT".into());
            env.set.insert(value_key.into(), "bad\0value".into());

            assert_eq!(
                env.validate(),
                Err(SpecError::EnvValueContainsNul),
                "value at key {value_key}, clear={clear}"
            );
        }
    }
}

#[test]
fn environment_value_and_conflict_errors_have_expected_messages() {
    for (error, expected) in [
        (
            SpecError::EnvValueContainsNul,
            "environment variable value contains a NUL byte",
        ),
        (
            SpecError::EnvKeyConflict,
            "environment variable key is both set and removed",
        ),
    ] {
        assert_eq!(error.to_string(), expected);
    }
}
