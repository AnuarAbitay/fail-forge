use super::*;

fn mutate(values: &[&str]) -> MutateArgs {
    let cli = Cli::try_parse_from(values).unwrap();
    let Commands::Mutate(arguments) = cli.command else {
        panic!("expected mutate subcommand");
    };
    *arguments
}

#[test]
fn family_only_uses_the_invocation_project_default() {
    let arguments = mutate(&["failforge", "mutate", "--family", "classic"]);
    assert!(arguments.project.is_none());
    assert_eq!(arguments.family, Some(MutationFamily::Classic));
    assert_eq!(arguments.python, "python3");
    assert_eq!(arguments.engine_arguments(), ["--family", "classic"]);
}

#[test]
fn config_accepts_runtime_selection_without_campaign_overrides() {
    let arguments = mutate(&[
        "failforge",
        "mutate",
        "--config",
        "config with spaces.yaml",
        "--python",
        "venv/bin/python",
        "--installation",
        "installed tree",
    ]);
    assert_eq!(arguments.python, "venv/bin/python");
    assert_eq!(
        arguments.installation,
        Some(PathBuf::from("installed tree"))
    );
    assert_eq!(
        arguments.engine_arguments(),
        ["--config", "config with spaces.yaml"]
    );
}

#[test]
fn config_conflicts_with_every_campaign_option() {
    for (name, value) in [
        ("--project", "project"),
        ("--family", "classic"),
        ("--workers", "1"),
        ("--test-timeout", "1s"),
        ("--command-timeout", "10s"),
        ("--output", "out"),
        ("--dart-sdk", "dart"),
        ("--flutter-sdk", "flutter"),
        ("--adapter-root", "adapter"),
        ("--operator", "BOOLEAN_LITERAL_FLIP"),
        ("--include", "lib/**"),
        ("--exclude", "generated/**"),
        ("--method", "execute"),
        ("--class", "Service"),
        ("--max-mutants", "1"),
    ] {
        assert!(
            Cli::try_parse_from([
                "failforge",
                "mutate",
                "--config",
                "config.yaml",
                name,
                value
            ])
            .is_err(),
            "accepted config with {name}"
        );
    }
}

#[test]
fn explicit_mode_requires_a_known_family() {
    for values in [
        vec!["failforge", "mutate"],
        vec!["failforge", "mutate", "--project", "."],
        vec!["failforge", "mutate", "--family", "unknown"],
        vec!["failforge", "mutate", "--family", "failure-mode"],
    ] {
        assert!(Cli::try_parse_from(values).is_err());
    }
    let arguments = mutate(&[
        "failforge",
        "mutate",
        "--project",
        ".",
        "--family",
        "failure_mode",
    ]);
    assert_eq!(arguments.family, Some(MutationFamily::FailureMode));
}

#[test]
fn repeated_filters_and_literal_values_are_forwarded_without_shell_processing() {
    let arguments = mutate(&[
        "failforge",
        "mutate",
        "--project",
        "project with spaces",
        "--family",
        "classic",
        "--operator",
        "BOOLEAN_LITERAL_FLIP",
        "--operator",
        "COMPARISON_NEGATION",
        "--include",
        "lib/**",
        "--exclude",
        "$(touch forbidden)",
        "--method",
        "first",
        "--method",
        "second",
        "--class",
        "Service",
    ]);
    assert_eq!(
        arguments.engine_arguments(),
        [
            "--project",
            "project with spaces",
            "--family",
            "classic",
            "--operator",
            "BOOLEAN_LITERAL_FLIP",
            "--operator",
            "COMPARISON_NEGATION",
            "--include",
            "lib/**",
            "--exclude",
            "$(touch forbidden)",
            "--method",
            "first",
            "--method",
            "second",
            "--class",
            "Service",
        ]
    );
}

#[test]
fn worker_and_candidate_boundaries_are_checked() {
    for value in ["0", "5", "256", "-1"] {
        assert!(
            Cli::try_parse_from([
                "failforge",
                "mutate",
                "--family",
                "classic",
                "--workers",
                value
            ])
            .is_err()
        );
    }
    for value in ["1", "4"] {
        mutate(&[
            "failforge",
            "mutate",
            "--family",
            "classic",
            "--workers",
            value,
        ]);
    }
    for value in ["0", "10000"] {
        mutate(&[
            "failforge",
            "mutate",
            "--family",
            "classic",
            "--max-mutants",
            value,
        ]);
    }
    assert!(
        Cli::try_parse_from([
            "failforge",
            "mutate",
            "--family",
            "classic",
            "--max-mutants",
            "10001"
        ])
        .is_err()
    );
}

#[test]
fn both_timeout_options_reject_invalid_or_zero_durations() {
    for name in ["--test-timeout", "--command-timeout"] {
        for value in [
            "0ms",
            "0s",
            "-1s",
            "1.5s",
            "1",
            "١s",
            "18446744073709551616s",
        ] {
            assert!(
                Cli::try_parse_from(["failforge", "mutate", "--family", "classic", name, value])
                    .is_err(),
                "accepted {name}={value}"
            );
        }
        for value in ["1ms", "2s", "3m"] {
            mutate(&["failforge", "mutate", "--family", "classic", name, value]);
        }
    }
}

#[test]
fn sdk_options_are_mutually_exclusive() {
    assert!(
        Cli::try_parse_from([
            "failforge",
            "mutate",
            "--family",
            "classic",
            "--dart-sdk",
            "dart",
            "--flutter-sdk",
            "flutter"
        ])
        .is_err()
    );
}

#[test]
fn native_project_path_bytes_survive_forwarding() {
    let path = OsString::from_vec(b"project-\xff".to_vec());
    let cli = Cli::try_parse_from([
        OsString::from("failforge"),
        OsString::from("mutate"),
        OsString::from("--project"),
        path.clone(),
        OsString::from("--family"),
        OsString::from("classic"),
    ])
    .unwrap();
    let Commands::Mutate(arguments) = cli.command else {
        panic!("expected mutate");
    };
    assert_eq!(arguments.engine_arguments()[1], path);
}
