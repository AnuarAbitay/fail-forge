//! Public campaigns exercise real Rust ownership, isolation and publication.
//! The finite SDK fixture speaks adapter and package:test protocols in Rust.
#![cfg(unix)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const FAMILIES: [&str; 3] = ["classic", "failure_mode", "language_specific"];
const OPERATORS: [&str; 3] = [
    "NUMERIC_LITERAL_REPLACEMENT",
    "EXCEPTION_AT_ENTRY",
    "NSM-001",
];
const ORIGINAL: &str = "const value = 1;\n";

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}
fn put(path: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff native mutate {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let fixture = Self {
            root: root.canonicalize().unwrap(),
            child: None,
        };
        fs::create_dir(fixture.root.join("temporary")).unwrap();
        let project = fixture.project();
        let dependency = fixture.root.join("test dependency");
        put(project.join("pubspec.yaml"), "name: subject\n");
        put(project.join("pubspec.lock"), "packages: {}\n");
        put(project.join("lib/subject.dart"), ORIGINAL);
        put(
            project.join("test/subject_test.dart"),
            "import 'package:subject/subject.dart';\nvoid main() {}\n",
        );
        put(dependency.join("bin/test.dart"), "void main() {}\n");
        put(dependency.join("lib/test.dart"), "// fixture dependency\n");
        put(
            project.join(".dart_tool/package_config.json"),
            json!({"configVersion":2,"packages":[
            {"name":"subject","rootUri":"../","packageUri":"lib/"},
            {"name":"test","rootUri":"../../test%20dependency/","packageUri":"lib/"}]})
            .to_string(),
        );
        put(
            project.join(".dart_tool/package_graph.json"),
            json!({"roots":["subject"],"packages":[
            {"name":"subject","dependencies":[]},{"name":"test","dependencies":[]}]})
            .to_string(),
        );
        put(
            fixture.adapter().join("bin/failforge_dart_adapter.dart"),
            "void main() {}\n",
        );
        put(
            fixture.adapter().join(".dart_tool/package_config.json"),
            r#"{"configVersion":2,"packages":[]}"#,
        );
        put(fixture.adapter().join("lib/adapter.dart"), "// adapter\n");
        put(fixture.sdk().join("lib/core/core.dart"), "// core\n");
        fs::create_dir_all(fixture.sdk().join("bin")).unwrap();
        fs::copy(
            env!("CARGO_BIN_EXE_failforge-campaign-sdk-fixture"),
            fixture.sdk().join("bin/dart"),
        )
        .unwrap();
        fixture.configure(
            "ordinary",
            json!({"classic":["2"],"failure_mode":["2"],"language_specific":["2"]}),
        );
        fixture
    }
    fn project(&self) -> PathBuf {
        self.root.join("subject")
    }
    fn install(&self) -> PathBuf {
        self.root.join("installation")
    }
    fn sdk(&self) -> PathBuf {
        self.install().join("dart-sdk")
    }
    fn adapter(&self) -> PathBuf {
        self.install().join("adapter")
    }
    fn output(&self) -> PathBuf {
        self.root.join("report")
    }
    fn configure(&self, mode: &str, cases: Value) {
        put(self.root.join("fixture.json"), json!({"mode":mode,"cases":cases,"trace":self.root.join("sdk.jsonl"),
            "original":self.project().join("lib/subject.dart"),"sdk_input":self.sdk().join("lib/core/core.dart"),"output":self.output()}).to_string());
    }
    fn direct(&self) -> Vec<OsString> {
        vec![
            "--project".into(),
            self.project().into_os_string(),
            "--family".into(),
            "classic".into(),
            "--operator".into(),
            OPERATORS[0].into(),
            "--output".into(),
            self.output().into_os_string(),
            "--dart-sdk".into(),
            self.sdk().into_os_string(),
            "--adapter-root".into(),
            self.adapter().into_os_string(),
            "--command-timeout".into(),
            "20s".into(),
            "--test-timeout".into(),
            "5s".into(),
        ]
    }
    fn profile(&self, families: &[&str], maximum: usize) -> Vec<OsString> {
        let mut text = "mutations:\n".to_owned();
        for family in families {
            let index = FAMILIES.iter().position(|value| value == family).unwrap();
            text.push_str(&format!(
                "  {family}:\n    enabled: true\n    operators: {{{}: true}}\n",
                OPERATORS[index]
            ));
            if *family == "language_specific" {
                text.push_str("    language: dart\n");
            }
        }
        for (name, path) in [
            ("output", self.output()),
            ("dart_sdk", self.sdk()),
            ("adapter_root", self.adapter()),
        ] {
            text.push_str(&format!("{name}: {}\n", json!(path)));
        }
        text.push_str(&format!(
            "workers: 1\ntimeouts: {{test: 5s, command: 20s}}\nlimits: {{max_mutants: {maximum}}}\n"
        ));
        let path = self.project().join("profile.yml");
        put(&path, text);
        vec!["--config".into(), path.into_os_string()]
    }
    fn spawn(&mut self, arguments: &[OsString], close_stdin: bool) -> u32 {
        self.spawn_kind("mutate", arguments, close_stdin)
    }
    fn spawn_kind(&mut self, kind: &str, arguments: &[OsString], close_stdin: bool) -> u32 {
        assert!(self.child.is_none());
        let mut command = Command::new(env!("CARGO_BIN_EXE_failforge"));
        command
            .arg(kind)
            .args(arguments)
            .arg("--installation")
            .arg(self.install())
            .current_dir(self.project())
            .env("PATH", "/nonexistent-path")
            .env("TMPDIR", self.root.join("temporary"))
            .env("FF_CAMPAIGN_FIXTURE", self.root.join("fixture.json"))
            .stdin(Stdio::null())
            .stdout(File::create(self.root.join("stdout")).unwrap())
            .stderr(File::create(self.root.join("stderr")).unwrap());
        if close_stdin {
            // SAFETY: only the async-signal-safe close is called after fork.
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
    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "campaign exceeded test deadline");
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        Output {
            status,
            stdout: fs::read(self.root.join("stdout")).unwrap(),
            stderr: fs::read(self.root.join("stderr")).unwrap(),
        }
    }
    fn run(&mut self, arguments: &[OsString]) -> Output {
        self.spawn(arguments, false);
        self.finish()
    }
    fn retained_replay(&mut self) -> Vec<OsString> {
        let direct = self.direct();
        success(&self.run(&direct));
        fs::rename(self.output(), self.root.join("seed")).unwrap();
        let archive = self.root.join("retained");
        fs::create_dir(&archive).unwrap();
        let baseline = Command::new(env!("CARGO_BIN_EXE_failforge"))
            .args(["run", "--cwd"])
            .arg(self.project())
            .arg("--capture-dir")
            .arg(archive.join("capture"))
            .arg("--")
            .arg(self.sdk().join("bin/dart"))
            .env("FF_CAMPAIGN_FIXTURE", self.root.join("fixture.json"))
            .output()
            .unwrap();
        success(&baseline);
        let hashes = failforge_workspace::source_hashes(
            &self.project(),
            &failforge_workspace::Cancellation::default(),
        )
        .unwrap();
        let hash = |path: PathBuf| format!("{:x}", Sha256::digest(fs::read(path).unwrap()));
        put(
            archive.join("baseline.json"),
            json!({
                "runnerExitCode":0,"runnerSummary":String::from_utf8(baseline.stderr).unwrap(),
                "outputSource":"capture/stdout.bin",
                "completedTestIdentities":[["test/subject_test.dart","assertion"]]
            })
            .to_string(),
        );
        put(
            archive.join("provenance.json"),
            json!({"sourceFileHashes":hashes}).to_string(),
        );
        put(archive.join("dependency-result.json"), json!({
            "generatedLockSha256":hash(self.project().join("pubspec.lock")),
            "generatedPackageConfigSha256":hash(self.project().join(".dart_tool/package_config.json"))
        }).to_string());
        let framework = self.root.join("upstream");
        put(
            framework.join("pubspec.yaml"),
            fs::read(self.project().join("pubspec.yaml")).unwrap(),
        );
        put(framework.join("lib/subject.dart"), ORIGINAL);
        fs::write(self.root.join("sdk.jsonl"), "").unwrap();
        vec![
            "--project".into(),
            self.project().into_os_string(),
            "--framework".into(),
            framework.into_os_string(),
            "--transcript".into(),
            self.root
                .join("seed/discovery/adapter-session.jsonl")
                .into_os_string(),
            "--baseline".into(),
            archive.join("baseline.json").into_os_string(),
            "--output".into(),
            self.output().into_os_string(),
            "--dart-sdk".into(),
            self.sdk().into_os_string(),
            "--adapter-root".into(),
            self.adapter().into_os_string(),
            "--command-timeout".into(),
            "20s".into(),
            "--test-timeout".into(),
            "5s".into(),
        ]
    }
    fn rows(&self, kind: &str) -> Vec<Value> {
        fs::read_to_string(self.root.join("sdk.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|row| kind.is_empty() || row["kind"] == kind)
            .collect()
    }
    fn summary(&self) -> Value {
        self.read_json("summary.json")
    }
    fn read_json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.output().join(name)).unwrap()).unwrap()
    }
    fn assert_clean(&self, unchanged: bool) {
        let summary = self.summary();
        assert_eq!(summary["originalProjectFilesUnchanged"], unchanged);
        assert_eq!(summary["temporarySnapshotCleaned"], true);
        assert_eq!(summary["workspaceCleanupDeferred"], false);
        if let Some(path) = summary["workspaceDirectory"].as_str() {
            assert!(!Path::new(path).exists(), "snapshot still exists: {path}");
        }
        for row in summary["records"].as_array().unwrap() {
            if let Some(path) = row["isolatedWorkingDirectory"].as_str() {
                assert_eq!(row["temporaryCopyCleaned"], true, "{row}");
                assert!(!Path::new(path).exists(), "{path}");
            } else {
                assert_eq!(row["outcome"], "INVALID", "{row}");
                assert_eq!(row["reason"], "static_analysis", "{row}");
            }
        }
        if unchanged {
            assert_eq!(
                fs::read_to_string(self.project().join("lib/subject.dart")).unwrap(),
                ORIGINAL
            );
        }
        for row in self
            .rows("test_start")
            .iter()
            .chain(self.rows("adapter_start").iter())
        {
            let pid = row["pid"].as_i64().unwrap() as i32;
            // SAFETY: signal zero checks existence without changing the process.
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "fixture {pid} still alive"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        for registry in fs::read_dir(self.root.join("temporary")).unwrap() {
            let registry = registry.unwrap().path();
            if registry.is_dir() {
                for entry in fs::read_dir(registry).unwrap() {
                    let name = entry.unwrap().file_name();
                    assert!(
                        !name.to_string_lossy().starts_with("campaign-")
                            && !name.to_string_lossy().starts_with("pending-"),
                        "workspace registry still contains {name:?}"
                    );
                }
            }
        }
    }
    fn assert_progress(&self) {
        let summary = self.summary();
        let progress = self.read_json("progress.json");
        assert_eq!(progress["status"], "finalizing");
        assert_eq!(progress["complete"], false);
        assert_eq!(progress["intendedStatus"], summary["status"]);
        for field in [
            "runId",
            "expectedMutants",
            "completedMutants",
            "outcomeCounts",
            "reasonCounts",
        ] {
            assert_eq!(progress[field], summary[field], "{field}");
        }
        assert!(progress.get("records").is_none());
        assert!(progress.get("mutationScore").is_none());
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
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn retained_replay_reuses_exact_candidates_and_runs_fresh_isolated_controls() {
    let mut fixture = Fixture::new();
    let arguments = fixture.retained_replay();
    let seed: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("seed/summary.json")).unwrap()).unwrap();
    fixture.spawn_kind("replay", &arguments, false);
    success(&fixture.finish());
    let summary = fixture.summary();
    assert_eq!(
        summary["records"][0]["mutantId"],
        seed["records"][0]["mutantId"]
    );
    assert_eq!(summary["outcomeCounts"], json!({"KILLED":1}));
    assert_eq!(summary["staticValidationReused"], true);
    assert!(summary["retainedReplay"].is_object());
    assert_eq!(summary["originalControls"].as_array().unwrap().len(), 2);
    assert!(summary["baseline"]["namespaceIsolation"].is_object());
    assert!(fixture.rows("adapter_start").is_empty());
    assert!(fixture.rows("discovery").is_empty());
    assert_eq!(
        fixture
            .rows("test_start")
            .iter()
            .map(|row| row["patched"].clone())
            .collect::<Vec<_>>(),
        [json!(false), json!(false), json!(true), json!(false)]
    );
    let copies: BTreeSet<_> = fixture
        .rows("test_start")
        .into_iter()
        .map(|row| row["root"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(copies.len(), 4);
    fixture.assert_clean(true);
    fixture.assert_progress();
}

#[test]
fn retained_replay_rejects_changed_source_baseline_and_incomplete_transcript_before_tests() {
    for failure in ["source", "baseline", "transcript"] {
        let mut fixture = Fixture::new();
        let arguments = fixture.retained_replay();
        match failure {
            "source" => put(
                fixture.project().join("lib/subject.dart"),
                "const value = 99;\n",
            ),
            "baseline" => {
                let path = fixture.root.join("retained/baseline.json");
                let mut baseline: Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                baseline["completedTestIdentities"] =
                    json!([["test/subject_test.dart", "foreign"]]);
                put(path, baseline.to_string());
            }
            _ => {
                let path = fixture.root.join("seed/discovery/adapter-session.jsonl");
                let raw = fs::read_to_string(&path).unwrap();
                let mut lines = raw.lines().collect::<Vec<_>>();
                lines.pop();
                put(path, format!("{}\n", lines.join("\n")));
            }
        }
        fixture.spawn_kind("replay", &arguments, false);
        let output = fixture.finish();
        assert_eq!(
            output.status.code(),
            Some(125),
            "{failure}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fixture.rows("test_start").is_empty(), "{failure}");
        assert!(fixture.rows("adapter_start").is_empty(), "{failure}");
        assert!(!fixture.output().exists(), "{failure}");
    }
}

#[test]
fn retained_replay_failed_second_original_control_prevents_mutants_and_score() {
    let mut fixture = Fixture::new();
    let arguments = fixture.retained_replay();
    fixture.configure("final_original_failure", json!({"classic":["2"]}));
    fixture.spawn_kind("replay", &arguments, false);
    let output = fixture.finish();
    assert_eq!(
        output.status.code(),
        Some(125),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = fixture.summary();
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["records"], json!([]));
    assert!(summary["mutationScore"].is_null());
    assert_eq!(summary["originalControls"].as_array().unwrap().len(), 1);
    assert_eq!(
        fixture
            .rows("test_start")
            .iter()
            .map(|row| row["patched"].clone())
            .collect::<Vec<_>>(),
        [json!(false), json!(false)]
    );
    fixture.assert_clean(true);
}

#[test]
fn direct_native_campaign_runs_without_path_programs_and_preserves_original() {
    let mut f = Fixture::new();
    let args = f.direct();
    f.spawn(&args, false);
    success(&f.finish());
    let summary = f.summary();
    assert_eq!(summary["status"], "completed");
    assert_eq!(summary["complete"], true);
    assert_eq!(summary["outcomeCounts"], json!({"KILLED":1}));
    assert_eq!(summary["mutationScore"]["percent"], 100.0);
    assert_eq!(
        f.rows("test_start")
            .iter()
            .map(|r| r["patched"].clone())
            .collect::<Vec<_>>(),
        [json!(false), json!(true), json!(false)]
    );
    let parents: BTreeSet<_> = f
        .rows("test_start")
        .iter()
        .chain(f.rows("adapter_start").iter())
        .map(|row| row["parent"].as_u64().unwrap())
        .collect();
    assert_eq!(parents.len(), 1, "attempts acquired multiple native owners");
    assert!(!f.install().join("python").exists());
    assert!(!f.install().join("scripts").exists());
    f.assert_clean(true);
    f.assert_progress();
}
#[test]
fn yaml_runs_all_families_in_canonical_order_and_publishes_consistent_children() {
    let mut f = Fixture::new();
    let args = f.profile(&["language_specific", "failure_mode", "classic"], 10);
    success(&f.run(&args));
    let summary = f.summary();
    assert_eq!(summary["kind"], "profile");
    assert_eq!(summary["outcomeCounts"], json!({"KILLED":3}));
    assert_eq!(
        f.rows("discovery")
            .iter()
            .map(|r| r["family"].as_str().unwrap())
            .collect::<Vec<_>>(),
        FAMILIES
    );
    assert_eq!(f.rows("test_start").len(), 9);
    let trace = f.rows("");
    let first_test = trace
        .iter()
        .position(|row| row["kind"] == "test_start")
        .unwrap();
    assert_eq!(
        trace[..first_test]
            .iter()
            .filter(|row| row["kind"] == "adapter_end")
            .map(|row| row["family"].as_str().unwrap())
            .collect::<Vec<_>>(),
        FAMILIES,
        "all discovery owners must drain before the first baseline"
    );
    let roots: BTreeSet<_> = f
        .rows("test_start")
        .iter()
        .map(|r| r["root"].to_string())
        .collect();
    assert_eq!(roots.len(), 9, "every attempt requires a fresh copy");
    for family in FAMILIES {
        let child = f.read_json(&format!("families/{family}/summary.json"));
        assert_eq!(summary["families"][family]["summary"], child);
        assert_eq!(child["complete"], true);
        assert_eq!(child["profileComplete"], true);
    }
    f.assert_clean(true);
    f.assert_progress();
}
#[test]
fn empty_direct_runs_original_controls_but_empty_profile_runs_no_tests() {
    for profile in [false, true] {
        let mut f = Fixture::new();
        f.configure(
            "ordinary",
            json!({"classic":[],"failure_mode":[],"language_specific":[]}),
        );
        let args = if profile {
            f.profile(&FAMILIES, 0)
        } else {
            f.direct()
        };
        success(&f.run(&args));
        assert_eq!(f.summary()["expectedMutants"], 0);
        assert_eq!(f.summary()["complete"], true);
        assert_eq!(f.rows("test_start").len(), if profile { 0 } else { 2 });
        f.assert_clean(true);
        f.assert_progress();
    }
}
#[test]
fn global_limit_counts_invalid_candidates_before_any_author_tests() {
    let mut f = Fixture::new();
    f.configure(
        "ordinary",
        json!({"classic":["missing"],"failure_mode":["2"],"language_specific":["2"]}),
    );
    let args = f.profile(&FAMILIES, 2);
    assert_eq!(f.run(&args).status.code(), Some(125));
    assert_eq!(f.rows("discovery").len(), 3);
    assert!(f.rows("test_start").is_empty());
    assert_eq!(f.summary()["status"], "failed");
    assert_eq!(f.summary()["complete"], false);
    f.assert_clean(true);
}
#[test]
fn truncated_discovery_and_forged_validation_never_run_author_tests() {
    for mode in ["truncated", "invalid_validation"] {
        let mut f = Fixture::new();
        f.configure(mode, json!({"classic":["2"]}));
        let args = f.direct();
        assert_eq!(f.run(&args).status.code(), Some(125), "{mode}");
        assert!(f.rows("test_start").is_empty(), "{mode}");
        assert_eq!(f.summary()["complete"], false);
        f.assert_clean(true);
    }
}
#[test]
fn failed_baseline_stops_family_execution_after_discovering_every_family() {
    let mut f = Fixture::new();
    f.configure(
        "baseline_failure",
        json!({"classic":["2"],"failure_mode":["2"],"language_specific":["2"]}),
    );
    let args = f.profile(&FAMILIES, 10);
    assert_eq!(f.run(&args).status.code(), Some(125));
    assert_eq!(f.rows("discovery").len(), 3);
    assert_eq!(f.rows("test_start").len(), 1);
    let summary = f.summary();
    assert_eq!(summary["families"]["classic"]["status"], "failed");
    assert_eq!(summary["families"]["failure_mode"]["status"], "not_started");
    assert_eq!(summary["mutationScore"], Value::Null);
    f.assert_clean(true);
}
#[test]
fn survived_mutant_remains_in_score_and_static_invalid_is_excluded() {
    let mut f = Fixture::new();
    f.configure("survived", json!({"classic":["2","missing"]}));
    let args = f.direct();
    success(&f.run(&args));
    let summary = f.summary();
    assert_eq!(summary["outcomeCounts"], json!({"SURVIVED":1,"INVALID":1}));
    assert_eq!(summary["mutationScore"]["denominator"], 1);
    assert_eq!(summary["mutationScore"]["percent"], 0.0);
    assert_eq!(f.rows("test_start").len(), 3);
    f.assert_clean(true);
}
#[test]
fn malformed_mutant_reporter_output_cannot_count_as_killed() {
    let mut f = Fixture::new();
    f.configure("garbage", json!({"classic":["2"]}));
    let args = f.direct();
    let _output = f.run(&args);
    let summary = f.summary();
    assert_ne!(summary["records"][0]["outcome"], "KILLED");
    assert_eq!(summary["mutationScore"]["killed"].as_u64().unwrap_or(0), 0);
    f.assert_clean(true);
}
#[test]
fn source_and_sdk_drift_override_execution_success() {
    for mode in ["source_drift", "sdk_drift"] {
        let mut f = Fixture::new();
        f.configure(mode, json!({"classic":["2"]}));
        let args = f.direct();
        assert_eq!(f.run(&args).status.code(), Some(125), "{mode}");
        let summary = f.summary();
        assert_eq!(summary["status"], "failed");
        assert_eq!(summary["complete"], false);
        assert_eq!(summary["mutationScore"], Value::Null);
        f.assert_clean(mode != "source_drift");
    }
}
#[test]
fn sigterm_cancels_active_workers_preserves_late_records_and_cleans_copies() {
    let mut f = Fixture::new();
    f.configure("hold", json!({"classic":["2","3","4"]}));
    let mut args = f.direct();
    args.extend(["--workers".into(), "2".into()]);
    let pid = f.spawn(&args, false);
    let deadline = Instant::now() + Duration::from_secs(20);
    while f
        .rows("test_start")
        .iter()
        .filter(|r| r["patched"] == true)
        .count()
        < 2
    {
        assert!(
            Instant::now() < deadline,
            "workers never reached cancellation barrier"
        );
        assert!(f.child.as_mut().unwrap().try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    // SAFETY: pid belongs to the retained child that has not exited above.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    let output = f.finish();
    assert_eq!(
        output.status.code(),
        Some(143),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = f.summary();
    assert_eq!(summary["status"], "cancelled");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["completedMutants"], 2);
    assert_eq!(summary["mutationScore"], Value::Null);
    f.assert_clean(true);
    f.assert_progress();
}
#[test]
fn progress_write_failure_retains_both_workers_and_preserves_foreign_directory() {
    let mut f = Fixture::new();
    f.configure("progress_failure", json!({"classic":["2","3","4"]}));
    let mut args = f.direct();
    args.extend(["--workers".into(), "2".into()]);
    let output = f.run(&args);
    assert_eq!(
        output.status.code(),
        Some(125),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = f.summary();
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["completedMutants"], 2);
    assert_eq!(summary["mutationScore"], Value::Null);
    assert_eq!(
        fs::read_to_string(f.output().join("progress.json/sentinel")).unwrap(),
        "foreign"
    );
    f.assert_clean(true);
}
#[test]
fn malformed_project_fails_before_output_or_sdk_execution() {
    let mut f = Fixture::new();
    fs::remove_file(f.project().join(".dart_tool/package_config.json")).unwrap();
    let args = f.direct();
    assert_eq!(f.run(&args).status.code(), Some(125));
    assert!(!f.output().exists());
    assert!(f.rows("adapter_start").is_empty());
}
#[test]
fn existing_output_and_source_output_are_preserved() {
    for source in [false, true] {
        let mut f = Fixture::new();
        let target = if source {
            f.project().join("lib")
        } else {
            f.output()
        };
        put(target.join("sentinel"), "foreign");
        let mut args = f.direct();
        let index = args.iter().position(|a| a == "--output").unwrap();
        args[index + 1] = target.clone().into_os_string();
        assert_eq!(f.run(&args).status.code(), Some(125));
        assert_eq!(
            fs::read_to_string(target.join("sentinel")).unwrap(),
            "foreign"
        );
        assert!(f.rows("adapter_start").is_empty());
    }
}
#[test]
fn invalid_yaml_and_conflicting_flags_are_rejected_before_sdk_execution() {
    for conflict in [false, true] {
        let mut f = Fixture::new();
        let mut args = f.profile(&["classic"], 10);
        if conflict {
            args.extend(["--family".into(), "classic".into()]);
        } else {
            put(f.project().join("profile.yml"), "mutations: [invalid]\n");
        }
        assert_eq!(f.run(&args).status.code(), Some(2));
        assert!(f.rows("adapter_start").is_empty());
        assert!(!f.output().exists());
    }
}

#[test]
fn missing_literal_profile_include_is_a_configuration_error_before_runtime_preflight() {
    let mut f = Fixture::new();
    let args = f.profile(&["classic"], 10);
    let path = f.project().join("profile.yml");
    let mut config = fs::read_to_string(&path).unwrap();
    config.push_str("rules:\n  include: [lib/missing.dart]\n");
    put(path, config);
    // Configuration errors take precedence even when the SDK is unavailable.
    fs::remove_file(f.sdk().join("bin/dart")).unwrap();
    let result = f.run(&args);
    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&result.stderr)
            .contains("literal include must name an existing project file")
    );
    assert!(result.stdout.is_empty());
    assert!(!f.output().exists());
    assert!(f.rows("adapter_start").is_empty());
    assert!(
        !f.root
            .join("temporary")
            .join(format!("failforge-workspaces-{}", unsafe {
                libc::geteuid()
            }))
            .exists()
    );
}
#[test]
fn removed_interpreter_flag_is_rejected_without_executing_anything() {
    let mut f = Fixture::new();
    let mut args = f.direct();
    args.extend(["--python".into(), "untrusted-interpreter".into()]);
    assert_eq!(f.run(&args).status.code(), Some(2));
    assert!(!f.output().exists());
    assert!(f.rows("adapter_start").is_empty());
}
#[test]
fn closed_standard_input_does_not_break_private_native_descriptors() {
    let mut f = Fixture::new();
    let args = f.direct();
    f.spawn(&args, true);
    success(&f.finish());
    f.assert_clean(true);
}

#[test]
fn native_owner_drains_and_publishes_partial_report_after_controller_sigkill() {
    let mut f = Fixture::new();
    f.configure("hold", json!({"classic":["2","3","4"]}));
    let mut args = f.direct();
    args.extend(["--workers".into(), "2".into()]);
    let pid = f.spawn(&args, false);
    let deadline = Instant::now() + Duration::from_secs(20);
    while f
        .rows("test_start")
        .iter()
        .filter(|row| row["patched"] == true)
        .count()
        < 2
    {
        assert!(Instant::now() < deadline, "workers did not start");
        assert!(f.child.as_mut().unwrap().try_wait().unwrap().is_none());
        std::thread::sleep(Duration::from_millis(5));
    }
    let owner = f.rows("test_start")[0]["parent"].as_i64().unwrap() as i32;
    assert_ne!(
        owner, pid as i32,
        "controller must have an independent owner"
    );
    // SAFETY: pid belongs to the retained, live outer controller child.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    assert_eq!(f.finish().status.signal(), Some(libc::SIGKILL));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let summary = fs::read(f.output().join("summary.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        // SAFETY: signal zero only observes the original native owner PID.
        let gone = unsafe { libc::kill(owner, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone
            && summary.as_ref().is_some_and(|summary| {
                summary["status"] == "cancelled" && summary["temporarySnapshotCleaned"] == true
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "native owner did not drain after controller death: {summary:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(f.summary()["complete"], false);
    assert_eq!(f.summary()["completedMutants"], 2);
    f.assert_clean(true);
    f.assert_progress();
}

#[test]
fn four_workers_use_fresh_copies_and_retain_every_result() {
    let mut f = Fixture::new();
    f.configure("ordinary", json!({"classic":["2","3","4","5","6"]}));
    let mut args = f.direct();
    args.extend(["--workers".into(), "4".into()]);
    success(&f.run(&args));
    assert_eq!(f.summary()["outcomeCounts"], json!({"KILLED":5}));
    assert_eq!(f.summary()["completedMutants"], 5);
    let roots: BTreeSet<_> = f
        .rows("test_start")
        .iter()
        .map(|row| row["root"].to_string())
        .collect();
    assert_eq!(roots.len(), 7);
    f.assert_clean(true);
    f.assert_progress();
}

#[test]
fn omitted_operator_selection_and_literal_filters_remain_native() {
    let mut f = Fixture::new();
    let mut args = f.direct();
    let operator = args.iter().position(|arg| arg == "--operator").unwrap();
    args.drain(operator..operator + 2);
    let sentinel = f.root.join("must not exist");
    let literal = format!("$(touch '{}')", sentinel.display());
    args.extend(["--exclude".into(), literal.into()]);
    success(&f.run(&args));
    assert_eq!(f.summary()["outcomeCounts"], json!({"KILLED": 1}));
    assert!(!sentinel.exists());
    f.assert_clean(true);
}

#[test]
fn failed_final_original_control_retains_mutants_without_publishing_a_score() {
    let mut f = Fixture::new();
    f.configure("final_original_failure", json!({"classic":["2"]}));
    let args = f.direct();
    let output = f.run(&args);
    assert_eq!(
        output.status.code(),
        Some(125),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = f.summary();
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["completedMutants"], 1);
    assert_eq!(summary["outcomeCounts"], json!({"KILLED":1}));
    assert_eq!(summary["mutationScore"], Value::Null);
    assert_eq!(summary["baseline"]["outcome"], "SURVIVED");
    assert!(
        summary["originalControls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|control| control["outcome"] == "KILLED"),
        "failed final control must remain in the report"
    );
    assert_eq!(f.rows("test_start").len(), 3);
    f.assert_clean(true);
    f.assert_progress();
}

#[test]
fn later_family_baseline_failure_retains_earlier_records_and_withholds_all_scores() {
    let mut f = Fixture::new();
    f.configure(
        "later_baseline_failure",
        json!({"classic":["2"],"failure_mode":["2"],"language_specific":["2"]}),
    );
    let args = f.profile(&FAMILIES, 10);
    let output = f.run(&args);
    assert_eq!(
        output.status.code(),
        Some(125),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = f.summary();
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["completedMutants"], 1);
    assert_eq!(summary["outcomeCounts"], json!({"KILLED":1}));
    assert_eq!(summary["families"]["classic"]["status"], "completed");
    assert_eq!(summary["families"]["failure_mode"]["status"], "failed");
    assert_eq!(
        summary["families"]["language_specific"]["status"],
        "not_started"
    );
    assert_eq!(
        summary["families"]["failure_mode"]["summary"]["baseline"]["outcome"],
        "KILLED"
    );
    for family in FAMILIES {
        let child = f.read_json(&format!("families/{family}/summary.json"));
        assert_eq!(summary["families"][family]["summary"], child);
        assert_eq!(child["profileComplete"], false);
        for score in ["mutationScore", "failureModeScore", "languageSpecificScore"] {
            assert!(summary[score].is_null(), "{score}");
            assert!(child[score].is_null(), "{family}: {score}");
        }
    }
    assert_eq!(f.rows("discovery").len(), 3);
    assert_eq!(f.rows("test_start").len(), 4);
    f.assert_clean(true);
    f.assert_progress();
}
