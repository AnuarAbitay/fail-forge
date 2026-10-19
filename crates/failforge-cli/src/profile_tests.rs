use super::*;
use crate::args::{Cli, Commands, ProfileCommands};
use clap::Parser;

fn arguments(extra: &[&str]) -> ProfileGenerateArgs {
    let mut values = vec![
        "failforge",
        "profile",
        "generate",
        "--file",
        "lib/source.dart",
        "--family",
        "classic",
        "--output",
        "profiles/source.yml",
        "--template",
    ];
    values.extend_from_slice(extra);
    let Commands::Profile(arguments) = Cli::try_parse_from(values).unwrap().command else {
        panic!("expected profile");
    };
    let ProfileCommands::Generate(arguments) = arguments.command;
    *arguments
}

#[test]
fn language_validation_precedes_filesystem_access() {
    let error = context(&arguments(&["--language", "dart"]), Path::new("/missing")).unwrap_err();
    assert_eq!(error.exit_code(), 2);
    assert!(
        error
            .to_string()
            .contains("--language dart is required exactly")
    );
}

#[test]
fn relative_invocation_is_rejected() {
    let error = context(&arguments(&[]), Path::new("relative")).unwrap_err();
    assert_eq!(error.exit_code(), 125);
    assert!(error.to_string().contains("must be absolute"));
}

#[test]
fn installation_is_checked_without_a_python_generator() {
    let cwd = std::env::current_dir().unwrap();
    let selected = context(&arguments(&["--installation", "."]), &cwd).unwrap();
    assert_eq!(selected.installation, cwd.canonicalize().unwrap());
    assert!(
        !selected
            .installation
            .join("python/failforge_campaign/profile_generate.py")
            .exists()
    );
}

#[test]
fn unavailable_installation_has_redacted_error() {
    let cwd = std::env::current_dir().unwrap();
    let error = context(
        &arguments(&["--installation", "/missing-secret-installation"]),
        &cwd,
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 125);
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn printed_command_quotes_paths_as_single_literal_arguments() {
    for (input, expected) in [
        ("a/b.yml", "a/b.yml"),
        ("", "''"),
        ("a b", "'a b'"),
        ("a'b", "'a'\"'\"'b'"),
        ("$(touch x);echo", "'$(touch x);echo'"),
        ("профиль.yml", "'профиль.yml'"),
    ] {
        assert_eq!(shell_quote(input), expected);
    }
}
