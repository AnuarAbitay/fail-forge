use super::*;
use crate::args::{Cli, Commands};
use clap::Parser;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "ff-mutate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(directory.join("installation with spaces/python/failforge_campaign"))
            .unwrap();
        fs::write(
            directory.join("installation with spaces/python/failforge_campaign/cli.py"),
            "# test entry\n",
        )
        .unwrap();
        fs::write(directory.join("runner"), "test runner bytes").unwrap();
        Self(directory)
    }

    fn installation(&self) -> PathBuf {
        self.0.join("installation with spaces")
    }
    fn runner(&self) -> PathBuf {
        self.0.join("runner")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn arguments(values: &[&str]) -> MutateArgs {
    let Commands::Mutate(arguments) = Cli::try_parse_from(values).unwrap().command else {
        panic!("expected mutate");
    };
    *arguments
}

#[test]
fn dispatch_uses_isolated_environment_flag_and_canonical_runtime_paths() {
    let fixture = Fixture::new();
    let arguments = arguments(&[
        "failforge",
        "mutate",
        "--family",
        "classic",
        "--python",
        "custom-python",
    ]);
    let command = prepare(
        &arguments,
        &fixture.0,
        &fixture.installation(),
        &fixture.runner(),
    )
    .unwrap();
    assert_eq!(command.get_program(), "custom-python");
    let actual: Vec<_> = command.get_args().collect();
    assert_eq!(
        actual,
        [
            std::ffi::OsStr::new("-E"),
            fixture
                .installation()
                .canonicalize()
                .unwrap()
                .join("python/failforge_campaign/cli.py")
                .as_os_str(),
            std::ffi::OsStr::new("--family"),
            std::ffi::OsStr::new("classic"),
            std::ffi::OsStr::new("--installation"),
            fixture.installation().canonicalize().unwrap().as_os_str(),
            std::ffi::OsStr::new("--runner"),
            fixture.runner().canonicalize().unwrap().as_os_str(),
        ]
    );
    assert_eq!(command.get_current_dir(), Some(fixture.0.as_path()));
}

#[test]
fn explicit_installation_is_resolved_from_the_invocation_directory() {
    let fixture = Fixture::new();
    let arguments = arguments(&[
        "failforge",
        "mutate",
        "--config",
        "config.yaml",
        "--installation",
        "installation with spaces",
    ]);
    let command = prepare(
        &arguments,
        &fixture.0,
        Path::new("/missing-default"),
        &fixture.runner(),
    )
    .unwrap();
    assert_eq!(
        command.get_args().nth(1).unwrap(),
        fixture
            .installation()
            .canonicalize()
            .unwrap()
            .join("python/failforge_campaign/cli.py")
    );
}

#[test]
fn missing_installation_entry_and_runner_are_distinct_failures() {
    let fixture = Fixture::new();
    let arguments = arguments(&["failforge", "mutate", "--family", "classic"]);
    assert_eq!(
        prepare(
            &arguments,
            &fixture.0,
            Path::new("/missing-installation"),
            &fixture.runner()
        )
        .unwrap_err(),
        LaunchError::Installation
    );
    fs::remove_file(
        fixture
            .installation()
            .join("python/failforge_campaign/cli.py"),
    )
    .unwrap();
    assert_eq!(
        prepare(
            &arguments,
            &fixture.0,
            &fixture.installation(),
            &fixture.runner()
        )
        .unwrap_err(),
        LaunchError::Controller
    );
    fs::write(
        fixture
            .installation()
            .join("python/failforge_campaign/cli.py"),
        "# entry\n",
    )
    .unwrap();
    assert_eq!(
        prepare(
            &arguments,
            &fixture.0,
            &fixture.installation(),
            &fixture.0.join("missing-runner")
        )
        .unwrap_err(),
        LaunchError::Runner
    );
}

#[test]
fn relative_invocation_directory_is_rejected() {
    let fixture = Fixture::new();
    let arguments = arguments(&["failforge", "mutate", "--family", "classic"]);
    assert_eq!(
        prepare(
            &arguments,
            Path::new("relative"),
            &fixture.installation(),
            &fixture.runner()
        )
        .unwrap_err(),
        LaunchError::InvocationDirectory
    );
}
