//! Thin-dispatch tests; no Dart SDK, adapter, network, or mutation campaign.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
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
        fs::write(entry, "import os, sys\nprint(os.getpid(), flush=True)\nprint(sys.flags.ignore_environment, flush=True)\nfor value in sys.argv[1:]:\n print(os.fsencode(value).hex(), flush=True)\nsys.exit(29)\n").unwrap();
        Self { root, child: None }
    }

    fn spawn(&mut self, arguments: &[OsString]) -> u32 {
        let stdout = File::create(self.root.join("stdout")).unwrap();
        let stderr = File::create(self.root.join("stderr")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_failforge"))
            .arg("mutate")
            .args(arguments)
            .arg("--installation")
            .arg(self.root.join("installed tree"))
            .env("PYTHONPATH", self.root.join("untrusted python path"))
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap();
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
    assert_eq!(values[2..], expected);
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
