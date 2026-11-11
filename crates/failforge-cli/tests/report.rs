//! HTML export needs retained evidence only; it never starts a toolchain.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff html report {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("run")).unwrap();
        Self { root }
    }

    fn put(&self, relative: &str, contents: impl AsRef<[u8]>) {
        let file = self.root.join(relative);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, contents).unwrap();
    }

    fn run(&self, arguments: &[&str]) -> (ExitStatus, String, String) {
        let stdout = File::create(self.root.join("cli.stdout")).unwrap();
        let stderr = File::create(self.root.join("cli.stderr")).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_failforge"))
            .args(arguments)
            .current_dir(&self.root)
            .env("PATH", self.root.join("no-sdk-here"))
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("HTML report command exceeded external deadline");
            }
            std::thread::park_timeout(Duration::from_millis(10));
        };
        (
            status,
            fs::read_to_string(self.root.join("cli.stdout")).unwrap(),
            fs::read_to_string(self.root.join("cli.stderr")).unwrap(),
        )
    }

    fn summary(&self, value: &Value) {
        self.put("run/summary.json", serde_json::to_vec(value).unwrap());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn record(id: &str, outcome: &str) -> Value {
    json!({"recordKind":"mutation","mutantId":id,"family":"classic",
        "file":"lib/wallet.dart","operatorId":"RELATIONAL_BOUNDARY",
        "outcome":outcome,"reason":if outcome=="SURVIVED" {"user_tests_passed"}else{"user_test_failure"},
        "edit":{"offsetBytes":18,"lengthBytes":17,"expected":"balance >= amount","replacement":"balance > amount"},
        "tests":{"started":2,"passed":if outcome=="SURVIVED"{2}else{1},"failed":if outcome=="SURVIVED"{0}else{1}},
        "reporterComplete":true,"wallSeconds":0.8})
}

fn summary() -> Value {
    json!({"version":1,"runId":"report-test","family":"classic",
        "complete":true,"status":"completed","error":null,
        "expectedMutants":2,"completedMutants":2,"wallSeconds":1.5,
        "records":[record(&"a".repeat(64),"SURVIVED"),record(&"b".repeat(64),"KILLED")],
        "outcomeCounts":{"KILLED":1,"SURVIVED":1},
        "mutationScore":{"family":"classic","killed":1,"survived":1,"denominator":2,"percent":50.0,"excluded":{}},
        "failureModeScore":null,"originalProjectFilesUnchanged":true,
        "temporarySnapshotCleaned":true,"workspaceCleanupDeferred":false})
}

fn embedded(path: &Path) -> Value {
    let html = fs::read_to_string(path).unwrap();
    let (_, data) = html
        .split_once("<script type=\"application/json\" id=\"failforge-report-data\">")
        .unwrap();
    serde_json::from_str(data.split_once("</script>").unwrap().0).unwrap()
}

#[test]
fn exports_a_portable_report_with_no_sdk_and_does_not_modify_evidence() {
    let fixture = Fixture::new();
    fixture.summary(&summary());
    let original = fs::read(fixture.root.join("run/summary.json")).unwrap();
    let capture = format!("run/mutants/{}/capture/stdout.bin", "a".repeat(64));
    fixture.put(&capture, "All tests passed!\n");
    let (status, stdout, stderr) = fixture.run(&["report", "run", "--html", "wallet report.html"]);
    assert!(status.success(), "{stderr}");
    assert!(stdout.contains("HTML report:"));
    assert_eq!(
        fs::read(fixture.root.join("run/summary.json")).unwrap(),
        original
    );
    let path = fixture.root.join("wallet report.html");
    let data = embedded(&path);
    assert_eq!(data["run"]["id"], "report-test");
    assert_eq!(data["families"][0]["score"]["percent"], 50.0);
    assert!(
        data["families"][0]["records"][0]["reportLogs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|log| log["text"] == "All tests passed!\n")
    );
    let html = fs::read_to_string(&path).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("<style>"));
    assert!(html.contains("connect-src 'none'"));
    assert!(!html.contains("<script src="));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn accepts_summary_file_and_preserves_incomplete_results_without_a_score() {
    let fixture = Fixture::new();
    let mut value = summary();
    value["complete"] = false.into();
    value["status"] = "cancelled".into();
    value["mutationScore"] = Value::Null;
    fixture.summary(&value);
    let (status, _, stderr) =
        fixture.run(&["report", "run/summary.json", "--html", "cancelled.html"]);
    assert!(status.success(), "{stderr}");
    let data = embedded(&fixture.root.join("cancelled.html"));
    assert_eq!(data["run"]["complete"], false);
    assert!(data["families"][0]["score"].is_null());
    assert_eq!(data["families"][0]["records"].as_array().unwrap().len(), 2);
}

#[test]
fn existing_output_and_input_cannot_be_overwritten() {
    let fixture = Fixture::new();
    fixture.summary(&summary());
    fixture.put("report.html", "existing report");
    let original = fs::read(fixture.root.join("run/summary.json")).unwrap();
    for output in ["report.html", "run/summary.json"] {
        let (status, _, stderr) = fixture.run(&["report", "run", "--html", output]);
        assert_eq!(status.code(), Some(2));
        assert!(stderr.contains("output already exists"), "{stderr}");
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join("report.html")).unwrap(),
        "existing report"
    );
    assert_eq!(
        fs::read(fixture.root.join("run/summary.json")).unwrap(),
        original
    );
}

#[test]
fn malformed_input_does_not_create_output_and_html_path_is_required() {
    let fixture = Fixture::new();
    fixture.put("run/summary.json", "{\"complete\":true}");
    let (status, _, _) = fixture.run(&["report", "run", "--html", "invalid.html"]);
    assert_eq!(status.code(), Some(2));
    assert!(!fixture.root.join("invalid.html").exists());
    let (status, _, stderr) = fixture.run(&["report", "run"]);
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("required arguments"));
    let (status, stdout, _) = fixture.run(&["report", "--help"]);
    assert!(status.success());
    assert!(stdout.contains("--html <FILE>"));
}

#[test]
fn html_metacharacters_in_retained_data_never_become_markup() {
    let fixture = Fixture::new();
    let payload = "</script><script>globalThis.injected = true</script>";
    let mut value = summary();
    value["records"][0]["edit"]["expected"] = payload.into();
    value["records"][0]["edit"]["lengthBytes"] = payload.len().into();
    fixture.summary(&value);
    let (status, _, stderr) = fixture.run(&["report", "run", "--html", "hostile.html"]);
    assert!(status.success(), "{stderr}");
    let path = fixture.root.join("hostile.html");
    let html = fs::read_to_string(&path).unwrap();
    assert!(!html.contains(payload));
    assert_eq!(
        embedded(&path)["families"][0]["records"][0]["edit"]["expected"],
        payload
    );
}

#[test]
fn older_reports_can_add_verified_source_lines_but_never_show_changed_code() {
    let fixture = Fixture::new();
    let source = "// Unicode α\r\nbool canWithdraw(int balance, int amount) {\r\n  return balance >= amount;\r\n}\r\n";
    fixture.put("sources/lib/wallet.dart", source);
    let digest = format!("{:x}", Sha256::digest(source.as_bytes()));
    let mut value = summary();
    for row in value["records"].as_array_mut().unwrap() {
        row["edit"]["offsetBytes"] = source.find("balance >= amount").unwrap().into();
    }
    fixture.summary(&value);
    fixture.put(
        "run/manifest.json",
        json!({"runId":"report-test",
        "configuration":{"project":fixture.root.join("sources")},
        "originalInputHashes":{"lib/wallet.dart":digest.to_ascii_uppercase()}})
        .to_string(),
    );
    let args = [
        "report",
        "run",
        "--html",
        "source.html",
        "--source-root",
        "sources",
    ];
    let (status, _, stderr) = fixture.run(&args);
    assert!(status.success(), "{stderr}");
    let output = embedded(&fixture.root.join("source.html"));
    let context = &output["families"][0]["records"][0]["sourceContext"];
    assert_eq!(context["line"], 3);
    assert_eq!(context["column"], 10);
    assert_eq!(context["sourceHash"], digest);
    assert!(
        context["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line == "  return balance >= amount;")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("sources/lib/wallet.dart")).unwrap(),
        source
    );

    // A nearby changed project cannot supply source context for this run.
    fixture.put("sources/lib/wallet.dart", source.replace(">=", ">"));
    let (status, _, stderr) = fixture.run(&[
        "report",
        "run",
        "--html",
        "changed.html",
        "--source-root",
        "sources",
    ]);
    assert!(status.success(), "{stderr}");
    let output = embedded(&fixture.root.join("changed.html"));
    assert!(output["families"][0]["records"][0]["sourceContext"].is_null());
    assert!(output["families"][0]["records"][0]["sourceContextUnavailable"].is_string());
    assert_eq!(output["families"][0]["score"]["percent"], 50.0);
}

#[test]
fn retained_source_context_needs_no_original_project_at_export_time() {
    let fixture = Fixture::new();
    let hash = "a".repeat(64);
    let context = json!({"version":1,"sourceHash":hash,"line":3,"column":10,
        "endLine":3,"startLine":2,"lines":["bool allowed() {","  return balance >= amount;","}"],"truncated":false});
    let mut value = summary();
    value["records"][0]["sourceHash"] = hash.into();
    value["records"][0]["sourceContext"] = context.clone();
    fixture.summary(&value);
    let (status, _, stderr) = fixture.run(&["report", "run", "--html", "retained-source.html"]);
    assert!(status.success(), "{stderr}");
    let output = embedded(&fixture.root.join("retained-source.html"));
    assert_eq!(
        output["families"][0]["records"][0]["sourceContext"],
        context
    );
}
