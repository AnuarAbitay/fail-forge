use super::*;

fn replay_arguments() -> Vec<&'static str> {
    vec![
        "failforge",
        "replay",
        "--project",
        "subject",
        "--framework",
        "upstream",
        "--transcript",
        "retained.jsonl",
        "--baseline",
        "baseline.json",
        "--output",
        "report",
    ]
}

#[test]
fn replay_requires_every_retained_input_and_rejects_campaign_filters() {
    let all = replay_arguments();
    assert!(Cli::try_parse_from(&all).is_ok());
    for index in (2..all.len()).step_by(2) {
        let mut missing = all.clone();
        missing.drain(index..index + 2);
        assert!(Cli::try_parse_from(missing).is_err(), "{}", all[index]);
    }
    for (flag, value) in [
        ("--family", "failure_mode"),
        ("--operator", "BOOLEAN_LITERAL_FLIP"),
        ("--config", "profile.yml"),
        ("--workers", "0"),
        ("--workers", "5"),
        ("--timeout", "0s"),
        ("--test-timeout", "no"),
        ("--python", "python3"),
    ] {
        let mut arguments = all.clone();
        arguments.extend([flag, value]);
        assert!(Cli::try_parse_from(arguments).is_err(), "{flag} {value}");
    }
}

#[test]
fn replay_preserves_paths_and_uses_native_classic_campaign_configuration() {
    let mut arguments = replay_arguments();
    arguments.extend(["--timeout", "40s", "--test-timeout", "3s", "--workers", "2"]);
    let Commands::Replay(arguments) = Cli::try_parse_from(arguments).unwrap().command else {
        panic!("expected replay");
    };
    let (campaign, retained) = arguments.into_campaign(Path::new("/invocation"));
    assert_eq!(retained.transcript, Path::new("/invocation/retained.jsonl"));
    assert_eq!(retained.baseline, Path::new("/invocation/baseline.json"));
    assert_eq!(retained.framework, Path::new("/invocation/upstream"));
    assert_eq!(
        campaign.configuration_values().unwrap(),
        serde_json::json!({"family":"classic","project":"subject","output":"report",
            "workers":2,"max_mutants":10000,"command_timeout":"40s","test_timeout":"3s"})
    );
}

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
    assert_eq!(
        arguments.configuration_values().unwrap(),
        serde_json::json!({"family":"classic"})
    );
}

#[test]
fn config_accepts_runtime_selection_without_campaign_overrides() {
    let arguments = mutate(&[
        "failforge",
        "mutate",
        "--config",
        "config with spaces.yaml",
        "--installation",
        "installed tree",
    ]);
    assert_eq!(
        arguments.installation,
        Some(PathBuf::from("installed tree"))
    );
    assert_eq!(
        arguments.configuration_values().unwrap(),
        serde_json::json!({"config":"config with spaces.yaml"})
    );
}

#[test]
fn config_conflicts_with_every_campaign_option() {
    for (name, value) in [
        ("--project", "project"),
        ("--family", "classic"),
        ("--language", "dart"),
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
fn language_specific_dart_and_semantic_id_are_forwarded_literally() {
    let arguments = mutate(&[
        "failforge",
        "mutate",
        "--family",
        "language_specific",
        "--language",
        "dart",
        "--operator",
        "NSM-001",
    ]);
    assert_eq!(arguments.family, Some(MutationFamily::LanguageSpecific));
    assert_eq!(
        arguments.configuration_values().unwrap(),
        serde_json::json!({
            "family":"language_specific", "language":"dart", "operators":["NSM-001"]
        })
    );
    assert!(
        Cli::try_parse_from([
            "failforge",
            "mutate",
            "--family",
            "language_specific",
            "--language",
            "other"
        ])
        .is_err()
    );
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
        arguments.configuration_values().unwrap(),
        serde_json::json!({
            "project":"project with spaces","family":"classic",
            "operators":["BOOLEAN_LITERAL_FLIP","COMPARISON_NEGATION"],
            "include":["lib/**"],"exclude":["$(touch forbidden)"],
            "methods":["first","second"],"classes":["Service"]
        })
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
fn native_project_path_bytes_reach_explicit_unicode_validation() {
    let path = crate::test_support::raw(b"project-\xff");
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
    assert_eq!(arguments.project.as_ref().unwrap().as_os_str(), path);
    assert!(arguments.configuration_values().is_err());
}
