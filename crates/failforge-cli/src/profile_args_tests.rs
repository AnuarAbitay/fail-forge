use super::*;

fn generate(values: &[&str]) -> ProfileGenerateArgs {
    let Commands::Profile(arguments) = Cli::try_parse_from(values).unwrap().command else {
        panic!("expected profile subcommand");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    *arguments
}

#[test]
fn generation_requires_a_source_family_and_output() {
    for values in [
        vec!["failforge", "profile"],
        vec!["failforge", "profile", "generate"],
        vec![
            "failforge",
            "profile",
            "generate",
            "--family",
            "classic",
            "--output",
            "profile.yml",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--output",
            "profile.yml",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "classic",
        ],
    ] {
        assert!(Cli::try_parse_from(values).is_err());
    }
}

#[test]
fn minimal_generation_only_forwards_profile_options() {
    let arguments = generate(&[
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/a.dart",
        "--family",
        "classic",
        "--output",
        "profiles/a.yml",
    ]);
    assert_eq!(arguments.python, "python3");
    assert!(arguments.installation.is_none());
    assert!(!arguments.template);
    assert!(arguments.has_valid_language_selection());
    assert_eq!(
        arguments.engine_arguments(),
        [
            "--file",
            "lib/a.dart",
            "--family",
            "classic",
            "--output",
            "profiles/a.yml",
        ]
    );
}

#[test]
fn generation_forwards_all_families_filters_and_flags_in_order() {
    let arguments = generate(&[
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/file with spaces.dart",
        "--family",
        "failure_mode",
        "--family",
        "classic",
        "--family",
        "language_specific",
        "--language",
        "dart",
        "--operator",
        "NSM-001",
        "--operator",
        "BOOLEAN_LITERAL_FLIP",
        "--output",
        "profiles/file with spaces.yml",
        "--force",
        "--dry-run",
        "--python",
        "python in venv",
        "--installation",
        "installed tree",
    ]);
    assert!(arguments.has_valid_language_selection());
    assert_eq!(arguments.python, "python in venv");
    assert_eq!(
        arguments.installation,
        Some(PathBuf::from("installed tree"))
    );
    assert_eq!(
        arguments.engine_arguments(),
        [
            "--file",
            "lib/file with spaces.dart",
            "--family",
            "failure_mode",
            "--family",
            "classic",
            "--family",
            "language_specific",
            "--language",
            "dart",
            "--operator",
            "NSM-001",
            "--operator",
            "BOOLEAN_LITERAL_FLIP",
            "--output",
            "profiles/file with spaces.yml",
            "--force",
            "--dry-run",
        ]
    );
}

#[test]
fn language_specific_always_requires_an_explicit_language() {
    for families in [
        vec!["--family", "language_specific"],
        vec!["--family", "classic", "--family", "language_specific"],
        vec!["--family", "language_specific", "--family", "classic"],
    ] {
        let mut values = vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--output",
            "profile.yml",
        ];
        values.extend(families);
        assert!(Cli::try_parse_from(values).is_err());
    }
}

#[test]
fn language_is_not_accepted_without_its_family() {
    for family in ["classic", "failure_mode"] {
        let arguments = generate(&[
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            family,
            "--language",
            "dart",
            "--output",
            "profile.yml",
        ]);
        assert!(!arguments.has_valid_language_selection());
    }
}

#[test]
fn unknown_subcommands_families_and_languages_are_rejected() {
    for values in [
        vec!["failforge", "profile", "init"],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "unknown",
            "--output",
            "profile.yml",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "failure-mode",
            "--output",
            "profile.yml",
        ],
        vec![
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "language_specific",
            "--language",
            "rust",
            "--output",
            "profile.yml",
        ],
    ] {
        assert!(Cli::try_parse_from(values).is_err());
    }
}

#[test]
fn duplicate_selection_is_preserved_for_generator_validation() {
    let arguments = generate(&[
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/a.dart",
        "--family",
        "classic",
        "--family",
        "classic",
        "--operator",
        "BOOLEAN_LITERAL_FLIP",
        "--operator",
        "BOOLEAN_LITERAL_FLIP",
        "--output",
        "profile.yml",
    ]);
    assert_eq!(
        arguments.engine_arguments(),
        [
            "--file",
            "lib/a.dart",
            "--family",
            "classic",
            "--family",
            "classic",
            "--operator",
            "BOOLEAN_LITERAL_FLIP",
            "--operator",
            "BOOLEAN_LITERAL_FLIP",
            "--output",
            "profile.yml",
        ]
    );
}

#[test]
fn source_and_output_native_bytes_are_forwarded_without_loss() {
    let source = OsString::from_vec(b"lib/source-\xff.dart".to_vec());
    let output = OsString::from_vec(b"profiles/output-\xfe.yml".to_vec());
    let cli = Cli::try_parse_from([
        OsString::from("failforge"),
        OsString::from("profile"),
        OsString::from("generate"),
        OsString::from("--file"),
        source.clone(),
        OsString::from("--family"),
        OsString::from("classic"),
        OsString::from("--output"),
        output.clone(),
    ])
    .unwrap();
    let Commands::Profile(arguments) = cli.command else {
        panic!("expected profile");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    assert_eq!(arguments.engine_arguments()[1], source);
    assert_eq!(arguments.engine_arguments()[5], output);
}

#[test]
fn shell_metacharacters_are_literal_argument_values() {
    let arguments = generate(&[
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/$(touch forbidden).dart",
        "--family",
        "classic",
        "--operator",
        "`touch forbidden`",
        "--output",
        "profiles/a;echo.yml",
    ]);
    assert_eq!(
        arguments.engine_arguments(),
        [
            "--file",
            "lib/$(touch forbidden).dart",
            "--family",
            "classic",
            "--operator",
            "`touch forbidden`",
            "--output",
            "profiles/a;echo.yml",
        ]
    );
}

#[test]
fn leading_dash_values_remain_values_at_the_python_parser_boundary() {
    let arguments = generate(&[
        "failforge",
        "profile",
        "generate",
        "--file=-calc.dart",
        "--family",
        "classic",
        "--operator=-INVALID",
        "--output=-profile.yml",
    ]);
    assert_eq!(
        arguments.engine_arguments(),
        [
            "--file=-calc.dart",
            "--family",
            "classic",
            "--operator=-INVALID",
            "--output=-profile.yml",
        ]
    );
}

#[test]
fn leading_dash_native_path_bytes_survive_joined_forwarding() {
    let source = OsString::from_vec(b"--file=-source-\xff.dart".to_vec());
    let output = OsString::from_vec(b"--output=-output-\xfe.yml".to_vec());
    let cli = Cli::try_parse_from([
        OsString::from("failforge"),
        OsString::from("profile"),
        OsString::from("generate"),
        source.clone(),
        OsString::from("--family"),
        OsString::from("classic"),
        output.clone(),
    ])
    .unwrap();
    let Commands::Profile(arguments) = cli.command else {
        panic!("expected profile");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    assert_eq!(arguments.engine_arguments()[0], source);
    assert_eq!(arguments.engine_arguments()[3], output);
}

#[test]
fn analysis_tool_paths_are_forwarded_without_shell_or_leading_dash_ambiguity() {
    for (flag, value) in [
        ("--dart-sdk", "Dart SDK with spaces"),
        ("--flutter-sdk", "Flutter SDK"),
    ] {
        let arguments = generate(&[
            "failforge",
            "profile",
            "generate",
            "--file",
            "lib/a.dart",
            "--family",
            "classic",
            "--output",
            "a.yml",
            flag,
            value,
            "--adapter-root=-adapter",
        ]);
        assert!(!arguments.template);
        assert_eq!(
            arguments.engine_arguments(),
            [
                "--file",
                "lib/a.dart",
                "--family",
                "classic",
                "--output",
                "a.yml",
                flag,
                value,
                "--adapter-root=-adapter",
            ]
        );
    }
}

#[test]
fn template_is_explicit_and_conflicts_with_every_analysis_tool_option() {
    let arguments = generate(&[
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
    ]);
    assert!(arguments.template);
    assert_eq!(arguments.engine_arguments().last().unwrap(), "--template");
    for flag in ["--dart-sdk", "--flutter-sdk", "--adapter-root"] {
        assert!(
            Cli::try_parse_from([
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
                "toolchain",
            ])
            .is_err()
        );
    }
}

#[test]
fn analysis_sdk_options_are_mutually_exclusive() {
    assert!(
        Cli::try_parse_from([
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
            "dart",
            "--flutter-sdk",
            "flutter",
        ])
        .is_err()
    );
}

#[test]
fn analysis_tool_path_native_bytes_are_preserved() {
    let sdk = OsString::from_vec(b"--dart-sdk=-sdk-\xff".to_vec());
    let cli = Cli::try_parse_from([
        OsString::from("failforge"),
        OsString::from("profile"),
        OsString::from("generate"),
        OsString::from("--file"),
        OsString::from("lib/a.dart"),
        OsString::from("--family"),
        OsString::from("classic"),
        OsString::from("--output"),
        OsString::from("a.yml"),
        sdk.clone(),
    ])
    .unwrap();
    let Commands::Profile(arguments) = cli.command else {
        panic!("expected profile");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    assert_eq!(arguments.engine_arguments().last().unwrap(), &sdk);
}
