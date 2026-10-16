use super::*;
use crate::args::{Cli, Commands, ProfileCommands};
use clap::Parser;
use std::ffi::OsString;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "ff-profile-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(directory.join("installation with spaces/python/failforge_campaign"))
            .unwrap();
        fs::write(
            directory
                .join("installation with spaces/python/failforge_campaign/profile_generate.py"),
            "# test entry\n",
        )
        .unwrap();
        Self(directory)
    }

    fn installation(&self) -> PathBuf {
        self.0.join("installation with spaces")
    }

    fn entry(&self) -> PathBuf {
        self.installation()
            .join("python/failforge_campaign/profile_generate.py")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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
fn template_launch_uses_python_isolation_and_no_native_runner() {
    let fixture = Fixture::new();
    let arguments = arguments(&["--python", "custom-python", "--dry-run"]);
    let command = prepare(&arguments, &fixture.0, &fixture.installation(), None).unwrap();
    assert_eq!(command.get_program(), "custom-python");
    let expected = vec![
        OsString::from("-E"),
        fixture.entry().canonicalize().unwrap().into_os_string(),
        OsString::from("--file"),
        OsString::from("lib/source.dart"),
        OsString::from("--family"),
        OsString::from("classic"),
        OsString::from("--output"),
        OsString::from("profiles/source.yml"),
        OsString::from("--dry-run"),
        OsString::from("--template"),
        OsString::from("--installation"),
        fixture
            .installation()
            .canonicalize()
            .unwrap()
            .into_os_string(),
    ];
    let actual: Vec<_> = command.get_args().map(ToOwned::to_owned).collect();
    assert_eq!(actual, expected);
    assert_eq!(command.get_current_dir(), Some(fixture.0.as_path()));
    assert!(!actual.iter().any(|argument| argument == "--runner"));
}

#[test]
fn explicit_installation_is_relative_to_invocation_and_overrides_default() {
    let fixture = Fixture::new();
    let arguments = arguments(&["--installation", "installation with spaces"]);
    let command = prepare(&arguments, &fixture.0, Path::new("/missing-default"), None).unwrap();
    assert_eq!(
        command.get_args().nth(1).unwrap(),
        fixture.entry().canonicalize().unwrap()
    );
}

#[test]
fn missing_installation_and_non_directory_installation_are_rejected() {
    let fixture = Fixture::new();
    let arguments = arguments(&[]);
    for installation in [fixture.0.join("missing"), fixture.entry()] {
        assert_eq!(
            prepare(&arguments, &fixture.0, &installation, None).unwrap_err(),
            LaunchError::Installation
        );
    }
}

#[test]
fn missing_generator_and_directory_in_its_place_are_rejected() {
    let fixture = Fixture::new();
    let arguments = arguments(&[]);
    fs::remove_file(fixture.entry()).unwrap();
    assert_eq!(
        prepare(&arguments, &fixture.0, &fixture.installation(), None).unwrap_err(),
        LaunchError::Generator
    );
    fs::create_dir(fixture.entry()).unwrap();
    assert_eq!(
        prepare(&arguments, &fixture.0, &fixture.installation(), None).unwrap_err(),
        LaunchError::Generator
    );
}

#[test]
fn relative_invocation_is_rejected_before_installation_access() {
    let arguments = arguments(&[]);
    assert_eq!(
        prepare(
            &arguments,
            Path::new("relative"),
            Path::new("/missing"),
            None
        )
        .unwrap_err(),
        LaunchError::InvocationDirectory
    );
}

#[test]
fn invalid_language_selection_is_rejected_before_installation_access() {
    let arguments = arguments(&["--language", "dart"]);
    assert_eq!(
        prepare(
            &arguments,
            Path::new("/invocation"),
            Path::new("/missing"),
            None
        )
        .unwrap_err(),
        LaunchError::LanguageSelection
    );
}

#[test]
fn launch_diagnostics_do_not_include_untrusted_values_or_system_errors() {
    for (error, expected) in [
        (
            LaunchError::LanguageSelection,
            "--language dart is required exactly",
        ),
        (
            LaunchError::InvocationDirectory,
            "invocation directory must be absolute",
        ),
        (
            LaunchError::Installation,
            "profile installation is unavailable",
        ),
        (LaunchError::Generator, "profile generator is missing"),
        (
            LaunchError::Runner,
            "cannot locate the native Runner executable",
        ),
    ] {
        assert!(error.diagnostic().contains(expected));
        assert!(!error.diagnostic().contains("os error"));
    }
}

#[test]
fn execute_reports_invalid_language_without_starting_python() {
    let arguments = arguments(&["--language", "dart", "--python", "/missing/python"]);
    assert_eq!(
        execute(arguments, Path::new("/invocation")),
        ExitCode::from(2)
    );
}

#[test]
fn execute_reports_missing_installation_without_starting_python() {
    let fixture = Fixture::new();
    let missing = fixture.0.join("missing");
    let arguments = arguments(&["--installation", missing.to_str().unwrap()]);
    assert_eq!(execute(arguments, &fixture.0), ExitCode::from(125));
}

#[test]
fn prepare_preserves_missing_interpreter_for_the_isolated_exec_boundary() {
    let fixture = Fixture::new();
    let missing_python = fixture.0.join("missing-python");
    let arguments = arguments(&[
        "--installation",
        fixture.installation().to_str().unwrap(),
        "--python",
        missing_python.to_str().unwrap(),
    ]);
    // CommandExt::exec may change cwd and signal dispositions even on failure.
    // The integration test exercises that boundary in its own process; unit
    // tests only inspect the launch contract without mutating their test owner.
    let command = prepare(&arguments, &fixture.0, &fixture.installation(), None).unwrap();
    assert_eq!(command.get_program(), missing_python.as_os_str());
    assert_eq!(command.get_current_dir(), Some(fixture.0.as_path()));
    assert!(!missing_python.exists());
}

#[test]
fn default_analysis_forwards_canonical_runner_without_initializing_it() {
    let fixture = Fixture::new();
    let runner = fixture.0.join("native runner");
    fs::write(&runner, "test runner bytes").unwrap();
    let mut arguments = arguments(&[]);
    arguments.template = false;
    let command = prepare(
        &arguments,
        &fixture.0,
        &fixture.installation(),
        Some(&runner),
    )
    .unwrap();
    let actual: Vec<_> = command.get_args().collect();
    assert!(!actual.iter().any(|argument| *argument == "--template"));
    assert_eq!(actual[actual.len() - 2], "--runner");
    assert_eq!(actual[actual.len() - 1], runner.canonicalize().unwrap());
}

#[test]
fn default_analysis_rejects_missing_relative_and_nonfile_runner() {
    let fixture = Fixture::new();
    let mut arguments = arguments(&[]);
    arguments.template = false;
    let directory = fixture.installation();
    let missing = fixture.0.join("missing-runner");
    for runner in [
        None,
        Some(Path::new("relative-runner")),
        Some(directory.as_path()),
        Some(missing.as_path()),
    ] {
        assert_eq!(
            prepare(&arguments, &fixture.0, &fixture.installation(), runner).unwrap_err(),
            LaunchError::Runner
        );
    }
}

#[test]
fn template_ignores_unavailable_runner_capability() {
    let fixture = Fixture::new();
    let arguments = arguments(&[]);
    let command = prepare(
        &arguments,
        &fixture.0,
        &fixture.installation(),
        Some(Path::new("/missing-runner")),
    )
    .unwrap();
    assert!(!command.get_args().any(|argument| argument == "--runner"));
}
