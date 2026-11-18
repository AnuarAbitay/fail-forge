use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
// Runner intentionally limits capture writers process-wide. These separate
// planner scenarios must not compete for that capacity when Rust runs unit
// tests in parallel; concurrency is tested separately by the Runner suite.
static PLANNER_RUN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-namespace-owner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Self(root);
        fixture.file("subject/pubspec.yaml", b"name: subject\n");
        fixture.file("subject/lib/subject.dart", b"const value = true;\n");
        fixture.file(
            "subject/test/subject_test.dart",
            b"import 'package:subject/subject.dart';\n",
        );
        fixture.file(
            "subject/.dart_tool/package_config.json",
            br#"{"configVersion":2,"packages":[{"name":"subject","rootUri":"../"}]}"#,
        );
        fixture.file(
            "adapter/bin/failforge_dart_namespace.dart",
            b"planner fixture",
        );
        fixture.file("adapter/.dart_tool/package_config.json", b"{}");
        fixture.file("sdk/bin/dart", b"#!/bin/sh\nexit 0\n");
        fixture
    }
    fn file(&self, name: &str, raw: &[u8]) {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, raw).unwrap();
    }
    fn project(&self) -> PlanProject {
        PlanProject {
            root: self.0.join("subject"),
            name: "subject".into(),
            flutter: false,
        }
    }
    fn runtime(&self) -> PlanRuntime {
        PlanRuntime {
            dart_sdk: self.0.join("sdk"),
            flutter_sdk: None,
            adapter_root: self.0.join("adapter"),
        }
    }
    fn plan(&self) -> Value {
        let root = self.0.join("subject");
        json!({"version":1,"kind":"dart_namespace_plan","packageName":"subject","alias":"failforge_subject",
            "packageConfigHash":hash_file(&root.join(".dart_tool/package_config.json"), &Cancellation::default()).unwrap(),
            "files":[{"file":"test/subject_test.dart","sourceHash":hash_file(&root.join("test/subject_test.dart"), &Cancellation::default()).unwrap(),"edits":[]}],"unsupported":[]})
    }
    fn executable(&self, script: &str) {
        self.file("sdk/bin/dart", format!("#!/bin/sh\n{script}\n").as_bytes());
        fs::set_permissions(
            self.0.join("sdk/bin/dart"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fn output(&self, bytes: &[u8]) {
        let text = std::str::from_utf8(bytes).unwrap();
        self.executable(&format!("printf '%s' {}", quote(text)));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

#[test]
fn planner_command_removes_private_report_descriptors_from_child_environment() {
    let fixture = Fixture::new();
    let project = fixture.project();
    let runtime = fixture.runtime();
    let report = report_directory(&project, &fixture.0.join("report"), &runtime).unwrap();
    let (spec, _) = command(&project, &report, &runtime, &[]).unwrap();
    let key = std::ffi::OsStr::new("FAILFORGE_REPORT_FDS");
    assert!(spec.env.remove.contains(key));
    assert!(!spec.env.set.contains_key(key));
}

#[tokio::test]
async fn native_planner_publishes_private_source_bound_artifacts() {
    let _permit = PLANNER_RUN.lock().await;
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let raw = serde_json::to_vec(&plan).unwrap();
    fixture.output(&raw);
    let directory = fixture.0.join("report");
    let before = source_hashes(&fixture.project().root, &Cancellation::default()).unwrap();
    let value = prepare(
        &fixture.project(),
        &directory,
        &fixture.runtime(),
        None,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(value["plan"], plan);
    assert_eq!(value["receipt"]["complete"], true);
    assert_eq!(value["receipt"]["sourceInputsUnchanged"], true);
    assert_eq!(value["receipt"]["runner"]["cleanup"], "complete");
    assert_eq!(
        value["receipt"]["command"][0],
        json!(fixture.0.join("sdk/bin/dart"))
    );
    assert_eq!(fs::read(directory.join("plan.json")).unwrap(), raw);
    assert_eq!(
        value["receipt"]["planSha256"],
        format!("{:x}", Sha256::digest(&raw))
    );
    assert_eq!(
        parse_json(&fs::read(directory.join("result.json")).unwrap()).unwrap(),
        value["receipt"]
    );
    for name in [
        "payload.json",
        "plan.json",
        "result.json",
        "runner.stdout",
        "runner.stderr",
    ] {
        assert_eq!(
            fs::metadata(directory.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    verify_source(&fixture.project().root, &before, &Cancellation::default()).unwrap();
}

#[tokio::test]
async fn malformed_duplicate_nonfinite_and_wrong_binding_outputs_never_publish() {
    let _permit = PLANNER_RUN.lock().await;
    let fixture = Fixture::new();
    let mut cases = vec![
        b"{".to_vec(),
        b"null".to_vec(),
        br#"{"version":1,"version":1}"#.to_vec(),
        b"NaN".to_vec(),
    ];
    for (key, value) in [
        ("version", json!(true)),
        ("version", json!(1.0)),
        ("alias", json!("other")),
        ("packageName", json!("other")),
        ("packageConfigHash", json!("wrong")),
        ("kind", json!("wrong")),
    ] {
        let mut plan = fixture.plan();
        plan[key] = value;
        cases.push(serde_json::to_vec(&plan).unwrap());
    }
    for (index, raw) in cases.into_iter().enumerate() {
        fixture.output(&raw);
        let directory = fixture.0.join(format!("bad-{index}"));
        assert!(
            prepare(
                &fixture.project(),
                &directory,
                &fixture.runtime(),
                None,
                &Cancellation::default()
            )
            .await
            .is_err()
        );
        assert!(!directory.join("plan.json").exists());
        let receipt = parse_json(&fs::read(directory.join("result.json")).unwrap()).unwrap();
        assert_eq!(receipt["complete"], false);
        assert_eq!(receipt["sourceInputsUnchanged"], true);
    }
}

#[tokio::test]
async fn failing_analyzer_retains_typed_receipt_and_no_plan() {
    let _permit = PLANNER_RUN.lock().await;
    let fixture = Fixture::new();
    fixture.executable("printf 'not a plan'; exit 9");
    let directory = fixture.0.join("failed");
    assert!(
        prepare(
            &fixture.project(),
            &directory,
            &fixture.runtime(),
            None,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    let receipt = parse_json(&fs::read(directory.join("result.json")).unwrap()).unwrap();
    assert_eq!(receipt["runner"]["status"], "exit:9");
    assert_eq!(receipt["exitCode"], 9);
    assert_eq!(receipt["complete"], false);
    assert!(!directory.join("plan.json").exists());
}

#[tokio::test]
async fn capture_digest_rejects_same_size_tampering_before_plan_parsing() {
    let _permit = PLANNER_RUN.lock().await;
    let fixture = Fixture::new();
    fixture.output(&serde_json::to_vec(&fixture.plan()).unwrap());
    let directory = fixture.0.join("capture-test");
    let report = report_directory(&fixture.project(), &directory, &fixture.runtime()).unwrap();
    let (spec, _) = command(
        &fixture.project(),
        &report,
        &fixture.runtime(),
        &["test/subject_test.dart".into()],
    )
    .unwrap();
    let result = run(spec, &Cancellation::default()).await.unwrap();
    let path = result.stdout.file_path.as_ref().unwrap();
    let raw = fs::read(path).unwrap();
    assert!(completed_plan(&result, &fixture.project(), &Cancellation::default()).is_ok());
    let mut altered = raw.clone();
    altered[0] = b'[';
    fs::write(path, altered).unwrap();
    assert!(
        completed_plan(&result, &fixture.project(), &Cancellation::default())
            .unwrap_err()
            .to_string()
            .contains("raw capture")
    );
    fs::write(path, raw).unwrap();
    let mut over_limit = result.clone();
    over_limit.stdout.bytes_read = MAX_PLAN + 1;
    assert!(
        completed_plan(&over_limit, &fixture.project(), &Cancellation::default())
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
    let mut incomplete = result.clone();
    incomplete.cleanup.child_reaped = false;
    assert!(completed_plan(&incomplete, &fixture.project(), &Cancellation::default()).is_err());
}

#[tokio::test]
async fn analyzer_cannot_redirect_failure_receipts_into_original_sources() {
    let _permit = PLANNER_RUN.lock().await;
    for replace_parent in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("reports")).unwrap();
        let directory = fixture.0.join("reports/plan");
        let replaced = if replace_parent {
            fixture.0.join("reports")
        } else {
            directory.clone()
        };
        let detached = fixture.0.join("detached-report");
        let subject = fixture.project().root;
        let before = source_hashes(&subject, &Cancellation::default()).unwrap();
        fixture.executable(&format!(
            "mv {} {}\nln -s {} {}\nprintf '{{}}'",
            quote(replaced.to_str().unwrap()),
            quote(detached.to_str().unwrap()),
            quote(subject.to_str().unwrap()),
            quote(replaced.to_str().unwrap())
        ));
        assert!(
            prepare(
                &fixture.project(),
                &directory,
                &fixture.runtime(),
                None,
                &Cancellation::default()
            )
            .await
            .is_err()
        );
        verify_source(&subject, &before, &Cancellation::default()).unwrap();
        for name in [
            "payload.json",
            "runner.stdout",
            "runner.stderr",
            "plan.json",
            "result.json",
        ] {
            assert!(!subject.join(name).exists(), "redirected write: {name}");
        }
        assert!(replaced.is_symlink());
        assert!(detached.is_dir());
    }
}

#[test]
fn report_writes_reject_replaced_directory_and_parent_and_preserve_foreign_artifacts() {
    for replace_parent in [false, true] {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("reports")).unwrap();
        let directory = fixture.0.join("reports/plan");
        let report = report_directory(&fixture.project(), &directory, &fixture.runtime()).unwrap();
        let file = report
            .write_private("plan.json", b"original-owned")
            .unwrap();
        fs::remove_file(directory.join("plan.json")).unwrap();
        fs::write(directory.join("plan.json"), b"foreign replacement").unwrap();
        assert!(report.remove_owned(OsStr::new("plan.json"), &file).is_err());
        assert_eq!(
            fs::read(directory.join("plan.json")).unwrap(),
            b"foreign replacement"
        );
        let replaced = if replace_parent {
            fixture.0.join("reports")
        } else {
            directory.clone()
        };
        fs::rename(&replaced, fixture.0.join("detached")).unwrap();
        symlink(fixture.project().root, &replaced).unwrap();
        assert!(
            report
                .write_private("result.json", b"must not escape")
                .is_err()
        );
        assert!(!fixture.project().root.join("result.json").exists());
    }
}

#[test]
fn report_locations_cannot_alias_source_sdk_adapter_or_dependencies() {
    let fixture = Fixture::new();
    for path in [
        fixture.0.join("subject/report"),
        fixture.0.join("sdk/report"),
        fixture.0.join("adapter/report"),
    ] {
        assert!(report_directory(&fixture.project(), &path, &fixture.runtime()).is_err());
        assert!(!path.exists());
    }
    symlink(fixture.0.join("subject"), fixture.0.join("alias")).unwrap();
    assert!(
        report_directory(
            &fixture.project(),
            &fixture.0.join("alias/report"),
            &fixture.runtime()
        )
        .is_err()
    );
}

#[tokio::test]
async fn missing_inventory_and_preexisting_report_are_rejected_without_overwrite() {
    let _permit = PLANNER_RUN.lock().await;
    let fixture = Fixture::new();
    fixture.output(&serde_json::to_vec(&fixture.plan()).unwrap());
    let directory = fixture.0.join("report");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("keep"), b"unchanged").unwrap();
    assert!(
        prepare(
            &fixture.project(),
            &directory,
            &fixture.runtime(),
            None,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read(directory.join("keep")).unwrap(), b"unchanged");
    fs::remove_file(fixture.0.join("subject/test/subject_test.dart")).unwrap();
    let empty = fixture.0.join("empty");
    assert!(
        prepare(
            &fixture.project(),
            &empty,
            &fixture.runtime(),
            None,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert!(!empty.exists());
    fixture.file("subject/test/subject_test.dart", b"void main() {}\n");
    fs::remove_file(fixture.0.join("adapter/bin/failforge_dart_namespace.dart")).unwrap();
    let missing = fixture.0.join("missing-tool");
    assert!(
        prepare(
            &fixture.project(),
            &missing,
            &fixture.runtime(),
            None,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert!(!missing.exists());
}

#[tokio::test]
async fn planner_cancellation_drains_child_and_drift_overrides_cancellation() {
    let _permit = PLANNER_RUN.lock().await;
    for drift in [false, true] {
        let fixture = Fixture::new();
        let ready = fixture.0.join("ready");
        let mutate = if drift {
            format!(
                "printf changed > {}\n",
                quote(fixture.0.join("subject/lib/subject.dart").to_str().unwrap())
            )
        } else {
            String::new()
        };
        fixture.executable(&format!(
            "{mutate}printf ready > {}\nprintf '{{'\nwhile :; do sleep 1; done",
            quote(ready.to_str().unwrap())
        ));
        let cancel = Cancellation::default();
        let requester = cancel.clone();
        let signaller = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !ready.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            requester.request(libc::SIGTERM);
        });
        let directory = fixture.0.join("cancelled");
        let error = tokio::time::timeout(
            Duration::from_secs(12),
            prepare(
                &fixture.project(),
                &directory,
                &fixture.runtime(),
                None,
                &cancel,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        signaller.await.unwrap();
        if drift {
            assert!(!matches!(error, NamespaceError::Cancelled(_)));
        } else {
            assert!(matches!(error, NamespaceError::Cancelled(libc::SIGTERM)));
        }
        assert!(!directory.join("plan.json").exists());
        let receipt = parse_json(&fs::read(directory.join("result.json")).unwrap()).unwrap();
        assert_eq!(receipt["sourceInputsUnchanged"], !drift);
        assert_eq!(receipt["complete"], false);
    }
}
