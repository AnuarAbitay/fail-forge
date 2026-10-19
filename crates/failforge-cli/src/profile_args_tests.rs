use super::*;
use clap::Parser;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

fn parse(extra: &[&str]) -> ProfileGenerateArgs {
    let mut values = vec![
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/a.dart",
        "--family",
        "classic",
        "--output",
        "profiles/a.yml",
    ];
    values.extend_from_slice(extra);
    let Commands::Profile(arguments) = Cli::try_parse_from(values).unwrap().command else {
        panic!("expected profile");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    *arguments
}

#[test]
fn required_arguments_and_family_enum_are_enforced() {
    for values in [
        vec!["failforge", "profile"],
        vec!["failforge", "profile", "init"],
        vec!["failforge", "profile", "generate"],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "a.dart",
            "--family",
            "classic",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "a.dart",
            "--output",
            "a.yml",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--family",
            "classic",
            "--output",
            "a.yml",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "a.dart",
            "--output",
            "a.yml",
            "--family",
            "other",
        ],
    ] {
        assert!(Cli::try_parse_from(values).is_err());
    }
}

#[test]
fn native_options_preserve_selection_and_paths_without_shell_parsing() {
    let arguments = parse(&[
        "--family",
        "language_specific",
        "--language",
        "dart",
        "--operator",
        "BOOLEAN_LITERAL_FLIP",
        "--operator",
        "NSM-001",
        "--force",
        "--dry-run",
        "--dart-sdk",
        "sdk with spaces",
        "--adapter-root",
        "adapter;echo",
    ]);
    let options = arguments.generate_options();
    assert_eq!(options.source, PathBuf::from("lib/a.dart"));
    assert_eq!(options.output, PathBuf::from("profiles/a.yml"));
    assert_eq!(options.families, ["classic", "language_specific"]);
    assert_eq!(options.operators, ["BOOLEAN_LITERAL_FLIP", "NSM-001"]);
    assert_eq!(options.language.as_deref(), Some("dart"));
    assert!(options.force && arguments.dry_run && !arguments.template);
    assert_eq!(options.dart_sdk, Some(PathBuf::from("sdk with spaces")));
    assert_eq!(options.adapter_root, Some(PathBuf::from("adapter;echo")));
    assert!(arguments.has_valid_language_selection());
}

#[test]
fn template_and_analysis_sdk_overrides_conflict() {
    for flag in ["--dart-sdk", "--flutter-sdk", "--adapter-root"] {
        let values = [
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "classic",
            "--output",
            "a.yml",
            "--template",
            flag,
            "path",
        ];
        assert!(Cli::try_parse_from(values).is_err());
    }
    let values = [
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/a.dart",
        "--family",
        "classic",
        "--output",
        "a.yml",
        "--dart-sdk",
        "a",
        "--flutter-sdk",
        "b",
    ];
    assert!(Cli::try_parse_from(values).is_err());
}

#[test]
fn template_is_explicit_and_default_analysis_needs_no_python_option() {
    assert!(!parse(&[]).template);
    assert!(parse(&["--template"]).template);
    // Legacy scripts remain accepted, but the interpreter is never executed.
    assert!(parse(&["--python", "/missing/python", "--template"]).template);
}

#[test]
fn language_must_match_the_selected_family() {
    assert!(!parse(&["--language", "dart"]).has_valid_language_selection());
    assert!(
        parse(&["--family", "language_specific", "--language", "dart"])
            .has_valid_language_selection()
    );
    assert!(
        Cli::try_parse_from([
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "language_specific",
            "--output",
            "a.yml"
        ])
        .is_err()
    );
}

#[test]
fn non_unicode_and_dash_prefixed_paths_reach_native_validation_without_loss() {
    let source = OsString::from_vec(b"--file=-source-\xff.dart".to_vec());
    let output = OsString::from_vec(b"--output=-profile-\xfe.yml".to_vec());
    let values = vec![
        OsString::from("failforge"),
        "profile".into(),
        "generate".into(),
        source,
        "--family".into(),
        "classic".into(),
        output,
    ];
    let Commands::Profile(arguments) = Cli::try_parse_from(values).unwrap().command else {
        panic!("expected profile");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    let options = arguments.generate_options();
    assert_eq!(options.source.as_os_str().as_bytes(), b"-source-\xff.dart");
    assert_eq!(options.output.as_os_str().as_bytes(), b"-profile-\xfe.yml");
}
