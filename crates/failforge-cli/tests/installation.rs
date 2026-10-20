//! Relocated binaries resolve their runtime independently of the build checkout.
#![cfg(unix)]

use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const ORIGINAL: &str = "const value = 1;\n";

fn put(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn sdk(installation: &Path) -> PathBuf {
    installation.join(".toolchain/flutter/bin/cache/dart-sdk")
}

fn prepare_installation(installation: &Path) {
    fs::create_dir_all(installation.join("bin")).unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_failforge"),
        installation.join("bin/failforge"),
    )
    .unwrap();
    put(
        installation.join("adapters/dart/bin/failforge_dart_adapter.dart"),
        "void main() {}\n",
    );
    put(
        installation.join("adapters/dart/.dart_tool/package_config.json"),
        r#"{"configVersion":2,"packages":[]}"#,
    );
    put(
        installation.join("adapters/dart/lib/adapter.dart"),
        "// fixture adapter\n",
    );
    put(sdk(installation).join("lib/core/core.dart"), "// core\n");
    fs::create_dir_all(sdk(installation).join("bin")).unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_failforge-campaign-sdk-fixture"),
        sdk(installation).join("bin/dart"),
    )
    .unwrap();
}

struct Fixture {
    root: PathBuf,
    installation: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff relocated installation {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let fixture = Self {
            installation: root.join("installation"),
            root,
            child: None,
        };
        fs::create_dir(fixture.cwd()).unwrap();
        fs::create_dir(fixture.root.join("temporary")).unwrap();
        put(fixture.project().join("pubspec.yaml"), "name: subject\n");
        put(fixture.project().join("pubspec.lock"), "packages: {}\n");
        put(fixture.project().join("lib/subject.dart"), ORIGINAL);
        put(
            fixture.project().join("test/subject_test.dart"),
            "import 'package:subject/subject.dart';\nvoid main() {}\n",
        );
        put(
            fixture.root.join("test-dependency/bin/test.dart"),
            "void main() {}\n",
        );
        put(
            fixture.root.join("test-dependency/lib/test.dart"),
            "// fixture dependency\n",
        );
        put(
            fixture.project().join(".dart_tool/package_config.json"),
            json!({"configVersion":2,"packages":[
                {"name":"subject","rootUri":"../","packageUri":"lib/"},
                {"name":"test","rootUri":"../../test-dependency/","packageUri":"lib/"}
            ]})
            .to_string(),
        );
        put(
            fixture.project().join(".dart_tool/package_graph.json"),
            json!({"roots":["subject"],"packages":[
                {"name":"subject","dependencies":[]},{"name":"test","dependencies":[]}
            ]})
            .to_string(),
        );
        prepare_installation(&fixture.installation);
        fixture.configure(&fixture.installation);
        fixture
    }

    fn project(&self) -> PathBuf {
        self.root.join("subject")
    }

    fn cwd(&self) -> PathBuf {
        self.root.join("unrelated-working-directory")
    }

    fn output(&self) -> PathBuf {
        self.root.join("report")
    }

    fn binary(&self) -> PathBuf {
        self.installation.join("bin/failforge")
    }

    fn configure(&self, installation: &Path) {
        put(
            self.root.join("fixture.json"),
            json!({
                "mode":"ordinary","cases":{"classic":["2"]},
                "trace":self.root.join("sdk.jsonl"),
                "original":self.project().join("lib/subject.dart"),
                "sdk_input":sdk(installation).join("lib/core/core.dart"),
                "output":self.output()
            })
            .to_string(),
        );
    }

    fn direct(&self) -> Vec<OsString> {
        vec![
            "mutate".into(),
            "--project".into(),
            self.project().into_os_string(),
            "--family".into(),
            "classic".into(),
            "--operator".into(),
            "NUMERIC_LITERAL_REPLACEMENT".into(),
            "--output".into(),
            self.output().into_os_string(),
            "--test-timeout".into(),
            "5s".into(),
            "--command-timeout".into(),
            "20s".into(),
        ]
    }

    fn yaml(&self) -> Vec<OsString> {
        let path = self.project().join("profile.yml");
        put(
            &path,
            format!(
                "mutations:\n  classic:\n    enabled: true\n    operators:\n      \
                 NUMERIC_LITERAL_REPLACEMENT: true\n\
                 timeouts: {{test: 5s, command: 20s}}\noutput: {}\n",
                json!(self.output())
            ),
        );
        vec!["mutate".into(), "--config".into(), path.into_os_string()]
    }

    fn template(&self) -> Vec<OsString> {
        vec![
            "profile".into(),
            "generate".into(),
            "--template".into(),
            "--file".into(),
            self.project().join("lib/subject.dart").into_os_string(),
            "--family".into(),
            "classic".into(),
            "--output".into(),
            self.project().join("profiles/subject.yml").into_os_string(),
        ]
    }

    fn run(&mut self, executable: &Path, arguments: &[OsString]) -> Output {
        assert!(self.child.is_none());
        self.child = Some(
            Command::new(executable)
                .args(arguments)
                .current_dir(self.cwd())
                .env("PATH", "/nonexistent-path")
                .env("TMPDIR", self.root.join("temporary"))
                .env("FF_CAMPAIGN_FIXTURE", self.root.join("fixture.json"))
                .stdin(Stdio::null())
                .stdout(File::create(self.root.join("stdout")).unwrap())
                .stderr(File::create(self.root.join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "relocated command exceeded test deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        Output {
            status,
            stdout: fs::read(self.root.join("stdout")).unwrap(),
            stderr: fs::read(self.root.join("stderr")).unwrap(),
        }
    }

    fn assert_campaign(&self, installation: &Path, profile: bool) {
        let read = |relative: &str| -> Value {
            serde_json::from_slice(&fs::read(self.output().join(relative)).unwrap()).unwrap()
        };
        let summary = read("summary.json");
        assert_eq!(summary["complete"], true);
        assert_eq!(summary["outcomeCounts"], json!({"KILLED":1}));
        assert_eq!(summary["originalProjectFilesUnchanged"], true);
        assert_eq!(summary["temporarySnapshotCleaned"], true);
        let manifest = read(if profile {
            "families/classic/manifest.json"
        } else {
            "manifest.json"
        });
        assert_eq!(
            manifest["configuration"]["dart_sdk"],
            json!(sdk(installation))
        );
        assert_eq!(
            manifest["configuration"]["adapter_root"],
            json!(installation.join("adapters/dart"))
        );
        let trace: Vec<Value> = fs::read_to_string(self.root.join("sdk.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            trace
                .iter()
                .filter(|row| row["kind"] == "test_start")
                .count(),
            3,
            "the selected SDK must run baseline, mutant and final control"
        );
        assert_eq!(
            fs::read_to_string(self.project().join("lib/subject.dart")).unwrap(),
            ORIGINAL
        );
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

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}, stdout: {}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn copied_binary_runs_direct_and_yaml_campaigns_using_its_installation() {
    for profile in [false, true] {
        let mut fixture = Fixture::new();
        let args = if profile {
            fixture.yaml()
        } else {
            fixture.direct()
        };
        let output = fixture.run(&fixture.binary(), &args);
        success(&output);
        fixture.assert_campaign(&fixture.installation, profile);
    }
}

#[test]
fn moving_the_installation_and_invoking_a_symlink_keeps_the_actual_binary_root() {
    let mut fixture = Fixture::new();
    let moved = fixture.root.join("moved installation");
    fs::rename(&fixture.installation, &moved).unwrap();
    fixture.installation = moved;
    fixture.configure(&fixture.installation);
    let executable = fixture.cwd().join("failforge-link");
    symlink(fixture.binary(), &executable).unwrap();
    let output = fixture.run(&executable, &fixture.template());
    success(&output);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains(fixture.installation.to_str().unwrap())
    );
    assert!(fixture.project().join("profiles/subject.yml").is_file());
    assert!(!fixture.root.join("sdk.jsonl").exists());

    let output = fixture.run(&executable, &fixture.direct());
    success(&output);
    fixture.assert_campaign(&fixture.installation, false);
}

#[test]
fn relative_explicit_installation_overrides_the_binary_default() {
    let mut fixture = Fixture::new();
    let alternate = fixture.root.join("alternate installation");
    prepare_installation(&alternate);
    fixture.configure(&alternate);
    let mut args = fixture.direct();
    args.extend(["--installation".into(), "../alternate installation".into()]);
    let output = fixture.run(&fixture.binary(), &args);
    success(&output);
    fixture.assert_campaign(&alternate, false);
}

#[test]
fn unknown_binary_layout_has_no_checkout_or_current_directory_fallback() {
    let mut fixture = Fixture::new();
    // The invocation directory intentionally looks like a prepared installation.
    prepare_installation(&fixture.cwd());
    let executable = fixture.root.join("loose-failforge");
    fs::copy(fixture.binary(), &executable).unwrap();
    for arguments in [fixture.template(), fixture.direct()] {
        let output = fixture.run(&executable, &arguments);
        assert_eq!(output.status.code(), Some(125), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--installation"),
            "{output:?}"
        );
        assert!(output.stdout.is_empty());
        assert!(!fixture.project().join("profiles/subject.yml").exists());
        assert!(!fixture.root.join("sdk.jsonl").exists());
        assert!(!fixture.output().exists());
    }
}

#[test]
fn explicit_installation_allows_a_binary_outside_recognized_layouts() {
    let mut fixture = Fixture::new();
    let executable = fixture.root.join("loose-failforge");
    fs::copy(fixture.binary(), &executable).unwrap();
    let mut args = fixture.template();
    args.extend(["--installation".into(), "../installation".into()]);
    let output = fixture.run(&executable, &args);
    success(&output);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains(fixture.installation.to_str().unwrap())
    );
    assert!(fixture.project().join("profiles/subject.yml").is_file());
}
