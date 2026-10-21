use super::*;
use serde_json::json;

fn parse(extra: &[&str]) -> DoctorArgs {
    let mut arguments = vec!["failforge", "doctor"];
    arguments.extend_from_slice(extra);
    let Commands::Doctor(arguments) = Cli::try_parse_from(arguments).unwrap().command else {
        panic!("expected doctor command");
    };
    *arguments
}

#[test]
fn defaults_need_no_campaign_selection_or_paths() {
    let args = parse(&[]);
    assert_eq!(
        args.configuration_values().unwrap(),
        json!({"family":"classic"})
    );
    assert!(args.installation.is_none());
}

#[test]
fn explicit_paths_are_forwarded_literally_without_installation_semantics() {
    let args = parse(&[
        "--project",
        "project 'quoted'",
        "--dart-sdk",
        "sdk with spaces",
        "--adapter-root",
        "adapter;echo",
        "--installation",
        "installation",
    ]);
    assert_eq!(
        args.configuration_values().unwrap(),
        json!({
            "family":"classic", "project":"project 'quoted'", "dart_sdk":"sdk with spaces",
            "adapter_root":"adapter;echo",
        })
    );
    assert_eq!(args.installation, Some("installation".into()));
    assert_eq!(
        parse(&["--flutter-sdk", "flutter"])
            .configuration_values()
            .unwrap(),
        json!({"family":"classic", "flutter_sdk":"flutter"})
    );
}

#[test]
fn profile_does_not_inject_a_family_and_allows_installation_override() {
    let args = parse(&[
        "--config",
        "profiles/check.yml",
        "--installation",
        "install",
    ]);
    assert_eq!(
        args.configuration_values().unwrap(),
        json!({"config":"profiles/check.yml"})
    );
    assert_eq!(args.installation, Some("install".into()));
}

#[test]
fn config_conflicts_with_each_semantic_override() {
    for flag in ["--project", "--dart-sdk", "--flutter-sdk", "--adapter-root"] {
        assert!(
            Cli::try_parse_from([
                "failforge",
                "doctor",
                "--config",
                "profile.yml",
                flag,
                "path"
            ])
            .is_err()
        );
    }
    assert!(
        Cli::try_parse_from([
            "failforge",
            "doctor",
            "--dart-sdk",
            "dart",
            "--flutter-sdk",
            "flutter"
        ])
        .is_err()
    );
}

#[test]
fn doctor_rejects_campaign_and_mutating_options() {
    for extra in [
        vec!["--family", "classic"],
        vec!["--force"],
        vec!["--fix"],
        vec!["--output", "reports"],
    ] {
        let mut arguments = vec!["failforge", "doctor"];
        arguments.extend(extra);
        assert!(Cli::try_parse_from(arguments).is_err());
    }
}

#[test]
fn non_unicode_paths_reach_explicit_validation_without_lossy_conversion() {
    let values = [
        OsString::from("failforge"),
        "doctor".into(),
        "--project".into(),
        OsString::from_vec(b"private-\xff".to_vec()),
    ];
    let Commands::Doctor(args) = Cli::try_parse_from(values).unwrap().command else {
        panic!("expected doctor");
    };
    assert_eq!(
        args.configuration_values().unwrap_err(),
        "configuration paths must contain valid Unicode"
    );
}
