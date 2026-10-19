//! Native configuration and execution handoff; no Dart SDK or campaign.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff-mutate-integration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let entry = root.join("installed tree/python/failforge_campaign/cli.py");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(entry, "import json, os, sys\nprint(os.getpid(), flush=True)\nprint(sys.flags.ignore_environment, flush=True)\nargs = sys.argv[1:]\nindex = args.index('--effective-config-fd')\nfd = int(args[index + 1])\nassert fd >= 3\nwith os.fdopen(fd, 'rb') as stream:\n effective = json.load(stream)\nfor value in args[:index]:\n print(os.fsencode(value).hex(), flush=True)\nprint('effective=' + json.dumps(effective, sort_keys=True), flush=True)\nsys.exit(29)\n").unwrap();
        fs::write(root.join("pubspec.yaml"), "name: native_config_fixture\n").unwrap();
        fs::write(root.join("campaign file.yaml"), "mutations:\n  classic:\n    enabled: true\n    operators: {BOOLEAN_LITERAL_FLIP: true}\n").unwrap();
        Self { root, child: None }
    }

    fn spawn(&mut self, arguments: &[OsString]) -> u32 {
        self.spawn_with_closed_stdin(arguments, false)
    }

    fn spawn_with_closed_stdin(&mut self, arguments: &[OsString], close_stdin: bool) -> u32 {
        let stdout = File::create(self.root.join("stdout")).unwrap();
        let stderr = File::create(self.root.join("stderr")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_failforge"));
        command
            .arg("mutate")
            .args(arguments)
            .arg("--installation")
            .arg(self.root.join("installed tree"))
            .env("PYTHONPATH", self.root.join("untrusted python path"))
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        if close_stdin {
            // SAFETY: the post-fork callback calls only async-signal-safe close.
            unsafe {
                command.pre_exec(|| {
                    libc::close(0);
                    Ok(())
                });
            }
        }
        let child = command.spawn().unwrap();
        let pid = child.id();
        self.child = Some(child);
        pid
    }

    fn finish(&mut self) -> (ExitStatus, String, String) {
        let deadline = Instant::now() + Duration::from_secs(8);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "dispatch process exceeded independent test deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        let read = |name| {
            let mut value = String::new();
            File::open(self.root.join(name))
                .unwrap()
                .take(64 * 1024)
                .read_to_string(&mut value)
                .unwrap();
            value
        };
        (status, read("stdout"), read("stderr"))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn hex(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn mutate_replaces_its_pid_preserves_arguments_and_returns_python_exit_status() {
    let mut fixture = Fixture::new();
    let literal = format!("$(touch {})", fixture.root.join("must-not-exist").display());
    let pid = fixture.spawn(&[
        "--project".into(),
        "project with spaces".into(),
        "--family".into(),
        "classic".into(),
        "--operator".into(),
        "BOOLEAN_LITERAL_FLIP".into(),
        "--exclude".into(),
        literal.clone().into(),
    ]);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(29), "{stderr}");
    let values: Vec<_> = stdout.lines().collect();
    assert_eq!(
        values[0],
        pid.to_string(),
        "exec must replace the CLI, not create an outer owner"
    );
    assert_eq!(
        values[1], "1",
        "Python must ignore PYTHON* environment settings"
    );
    let installation = fixture.root.join("installed tree").canonicalize().unwrap();
    let runner = PathBuf::from(env!("CARGO_BIN_EXE_failforge"))
        .canonicalize()
        .unwrap();
    let expected: Vec<String> = [
        "--project",
        "project with spaces",
        "--family",
        "classic",
        "--operator",
        "BOOLEAN_LITERAL_FLIP",
        "--exclude",
        &literal,
        "--installation",
        installation.to_str().unwrap(),
        "--runner",
        runner.to_str().unwrap(),
    ]
    .iter()
    .map(hex)
    .collect();
    assert_eq!(values[2..2 + expected.len()], expected);
    let effective: serde_json::Value =
        serde_json::from_str(values.last().unwrap().strip_prefix("effective=").unwrap()).unwrap();
    assert_eq!(
        effective["configuration"]["rules"]["exclude"],
        serde_json::json!([literal])
    );
    assert_eq!(
        effective["configuration"]["project"],
        fixture
            .root
            .canonicalize()
            .unwrap()
            .join("project with spaces")
            .to_str()
            .unwrap()
    );
    assert!(stderr.is_empty());
    assert!(!fixture.root.join("must-not-exist").exists());
}

#[test]
fn config_dispatch_keeps_relative_config_path_and_runtime_flags() {
    let mut fixture = Fixture::new();
    fixture.spawn(&["--config".into(), "campaign file.yaml".into()]);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(29), "{stderr}");
    let values: Vec<_> = stdout.lines().collect();
    assert_eq!(values[2], hex("--config"));
    assert_eq!(values[3], hex("campaign file.yaml"));
    assert_eq!(values[4], hex("--installation"));
    assert!(stderr.is_empty());
}

#[test]
fn missing_interpreter_is_a_redacted_launch_failure() {
    let mut fixture = Fixture::new();
    fixture.spawn(&[
        "--family".into(),
        "classic".into(),
        "--python".into(),
        "missing-python-secret-marker".into(),
    ]);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(125));
    assert!(stdout.is_empty());
    assert!(stderr.contains("cannot execute the Python campaign controller"));
    assert!(!stderr.contains("secret-marker"));
}

#[test]
fn conflicting_config_is_rejected_before_interpreter_execution() {
    let mut fixture = Fixture::new();
    fixture.spawn(&[
        "--config".into(),
        "secret-config.yaml".into(),
        "--family".into(),
        "classic".into(),
    ]);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(2));
    assert!(stdout.is_empty());
    assert!(!stderr.contains("secret-config"));
}

#[test]
fn invalid_yaml_is_rejected_by_rust_before_any_python_execution() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.root.join("campaign file.yaml"),
        "workers: 1\nworkers: 2\n",
    )
    .unwrap();
    fixture.spawn(&["--config".into(), "campaign file.yaml".into()]);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(2));
    assert!(stdout.is_empty(), "Python fixture must not have started");
    assert!(stderr.contains("duplicate YAML mapping key"));
}

#[test]
fn normalized_handoff_does_not_depend_on_python_configuration_modules() {
    let mut fixture = Fixture::new();
    fixture.spawn(&["--config".into(), "campaign file.yaml".into()]);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(29), "{stderr}");
    let effective: serde_json::Value = serde_json::from_str(
        stdout
            .lines()
            .last()
            .unwrap()
            .strip_prefix("effective=")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(effective["kind"], "profile");
    assert_eq!(
        effective["campaigns"][0]["rules"]["operators"],
        serde_json::json!(["BOOLEAN_LITERAL_FLIP"])
    );
    assert_eq!(effective["campaigns"][0]["timeouts"]["command"], "120000ms");
    assert!(!fixture.root.join("installed tree/scripts").exists());
}

#[test]
fn native_handoff_works_when_standard_input_was_closed() {
    let mut fixture = Fixture::new();
    fixture.spawn_with_closed_stdin(&["--family".into(), "classic".into()], true);
    let (status, stdout, stderr) = fixture.finish();
    assert_eq!(status.code(), Some(29), "{stderr}");
    assert!(stdout.contains("effective="));
}
