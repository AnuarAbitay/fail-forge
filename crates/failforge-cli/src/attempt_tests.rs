use super::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-attempt-owner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn receipt(&self) -> Receipt {
        Receipt {
            directory: Arc::new(
                crate::attempt_artifacts::create(&self.0.join("case"), &[]).unwrap(),
            ),
            attempt: json!({"outcome":"SURVIVED", "reason":"user_tests_passed", "runner":{"cause":"ProcessCompleted"}, "exitCode":0}),
            catalogue: json!({"bound":"catalogue"}),
            initialization: json!({"observation":{"observation":{"suites":["test/a_test.dart"]}}}),
            namespace_plan: json!({"plan":{"kind":"dart_namespace_plan"},"receipt":{"complete":true}}),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request() -> Value {
    json!({
        "operation":"attempt",
        "project":{"root":"/subject", "name":"subject", "packages":{"test":"/deps/test"}, "graph":{}, "flutter":false, "frameworkCollision":false},
        "case":"/reports/baseline", "runtime":{"runner":"/tools/failforge", "dartSdk":"/sdk", "flutterSdk":null, "adapterRoot":"/adapter"},
        "config":{"commandTimeout":"6s", "testTimeout":"1s"},
        "workspace":{"root":"/workspaces", "campaign":"campaign-00000000000000000000000000000000"},
        "context":{"expected":null,"baseline":null,"mutant":null,"alias":"subject","initialization":null},
        "patchSha256":null, "namespacePlan":null,
    })
}

fn decoded(value: &Value) -> Result<Request, NamespaceError> {
    decode(&serde_json::to_vec(value).unwrap())
}

#[test]
fn decoded_value_and_wire_requests_share_binding_validation() {
    assert!(decode_value(request()).is_ok());
    let mut value = request();
    value["context"]["alias"] = "other".into();
    assert!(decode_value(value.clone()).is_err());
    assert!(decoded(&value).is_err());
    let mut typed = decoded(&request()).unwrap();
    typed.workspace.campaign = "other-campaign".into();
    assert!(validate_request(&typed).is_err());
}

#[test]
fn explicit_replay_framework_requires_isolated_alias_even_without_name_collision() {
    let mut value = request();
    value["frameworkRoot"] = "/archive/pristine".into();
    assert!(decoded(&value).is_err());
    value["context"]["alias"] = "failforge_subject".into();
    let replay = decoded(&value).unwrap();
    assert_eq!(
        replay.framework_root,
        Some(PathBuf::from("/archive/pristine"))
    );
    assert!(!replay.project.framework_collision);
    value["frameworkRoot"] = "relative".into();
    assert!(decoded(&value).is_err());
    value["frameworkRoot"] = Value::Null;
    assert!(decoded(&value).is_err());
}

#[test]
fn deferred_attempt_leaves_the_final_mutation_report_unoccupied() {
    let fixture = Fixture::new();
    let (receipt, bytes) = seal_receipt(
        fixture.receipt(),
        &Cancellation::default(),
        Publication::Deferred,
        false,
    )
    .unwrap();
    assert!(bytes.is_none());
    assert!(!fixture.0.join("case/result.json").exists());
    let initial = fs::read(fixture.0.join("case/attempt.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&initial).unwrap(),
        receipt.attempt
    );
    assert_eq!(receipt.attempt["outcome"], "SURVIVED");
    assert!(!receipt.initialization.is_null());
    let final_record = json!({"recordKind":"mutation", "outcome":"KILLED"});
    receipt
        .directory
        .write_json("result.json", &final_record)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fixture.0.join("case/result.json")).unwrap())
            .unwrap(),
        final_record
    );
    assert_eq!(
        fs::read(fixture.0.join("case/attempt.json")).unwrap(),
        initial
    );
}

#[test]
fn standalone_sealing_preserves_public_and_private_receipts() {
    let fixture = Fixture::new();
    let (receipt, bytes) = seal_receipt(
        fixture.receipt(),
        &Cancellation::default(),
        Publication::Attempt,
        true,
    )
    .unwrap();
    let envelope: Value = serde_json::from_slice(&bytes.unwrap()).unwrap();
    assert_eq!(envelope["version"], 1);
    assert_eq!(envelope["result"]["attempt"], receipt.attempt);
    assert_eq!(envelope["result"]["catalogue"], receipt.catalogue);
    assert_eq!(envelope["result"]["initialization"], receipt.initialization);
    assert_eq!(envelope["result"]["namespacePlan"], receipt.namespace_plan);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fixture.0.join("case/result.json")).unwrap())
            .unwrap(),
        receipt.attempt
    );
}

#[test]
fn deferred_sealing_revokes_evidence_on_cancellation_and_respects_commit_point() {
    let fixture = Fixture::new();
    let cancelled = Cancellation::default();
    cancelled.request(libc::SIGINT);
    let (receipt, _) =
        seal_receipt(fixture.receipt(), &cancelled, Publication::Deferred, false).unwrap();
    assert_eq!(receipt.attempt["outcome"], "CANCELLED");
    assert!(receipt.catalogue.is_null());
    assert!(receipt.initialization.is_null());
    assert!(receipt.namespace_plan.is_null());

    let later = Fixture::new();
    let signal = Cancellation::default();
    let (receipt, _) =
        seal_receipt(later.receipt(), &signal, Publication::Deferred, false).unwrap();
    signal.request(libc::SIGTERM);
    assert_eq!(receipt.attempt["outcome"], "SURVIVED");
}

#[test]
fn native_controls_persist_the_receipt_without_retaining_redundant_transport_bytes() {
    let fixture = Fixture::new();
    let (receipt, bytes) = seal_receipt(
        fixture.receipt(),
        &Cancellation::default(),
        Publication::Attempt,
        false,
    )
    .unwrap();
    assert!(bytes.is_none());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(fixture.0.join("case/result.json")).unwrap())
            .unwrap(),
        receipt.attempt
    );
    assert!(!receipt.catalogue.is_null());
    assert!(!receipt.initialization.is_null());
}

#[test]
fn request_schema_rejects_ambiguous_unknown_and_unbound_inputs() {
    let valid = request();
    assert!(decoded(&valid).is_ok());
    for pointer in [
        "/extra",
        "/project/extra",
        "/runtime/extra",
        "/config/extra",
        "/workspace/extra",
        "/context/extra",
    ] {
        let mut value = valid.clone();
        let (parent, name) = pointer.rsplit_once('/').unwrap();
        value
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(name.into(), true.into());
        assert!(decoded(&value).is_err(), "{pointer}");
    }
    for (pointer, replacement) in [
        ("/operation", json!("execute")),
        ("/case", json!("relative")),
        ("/runtime/runner", json!("/bad\0runner")),
        ("/project/packages/test", json!("relative")),
        ("/project/flutter", json!(1)),
        ("/project/flutter", json!(true)),
        ("/context/alias", json!("different")),
        ("/workspace/campaign", json!("campaign-ABC")),
        ("/config/commandTimeout", json!("5999ms")),
        ("/config/testTimeout", json!("0s")),
        ("/patchSha256", json!("a".repeat(64))),
        (
            "/namespacePlan",
            json!({"plan":{},"receipt":{"complete":true}}),
        ),
        (
            "/context/baseline",
            json!({"rawSha256":"bad","cwd":"/", "identities":[],"view":null}),
        ),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert!(decoded(&value).is_err(), "{pointer}");
    }
    let encoded = serde_json::to_string(&valid).unwrap();
    let duplicated = encoded.replacen(
        "\"operation\":\"attempt\"",
        "\"operation\":\"attempt\",\"operation\":\"attempt\"",
        1,
    );
    assert!(decode(duplicated.as_bytes()).is_err());
}

#[test]
fn late_cancellation_preserves_infrastructure_and_checks_owned_exit_codes() {
    let cancelled = Cancellation::default();
    cancelled.request(libc::SIGTERM);
    for (index, (outcome, reason, cause, exit, expected_outcome, expected_reason)) in [
        (
            "SURVIVED",
            "user_tests_passed",
            "ProcessCompleted",
            0,
            "CANCELLED",
            "campaign_cancelled",
        ),
        (
            "TIMEOUT",
            "runner_timeout",
            "TimedOut",
            124,
            "CANCELLED",
            "campaign_cancelled",
        ),
        (
            "INFRA_ERROR",
            "execution_input_changed",
            "Cancelled(SigTerm)",
            143,
            "INFRA_ERROR",
            "execution_input_changed",
        ),
        (
            "INFRA_ERROR",
            "runner_not_completed",
            "Cancelled(SigTerm)",
            143,
            "CANCELLED",
            "campaign_cancelled",
        ),
        (
            "INFRA_ERROR",
            "runner_not_completed",
            "Cancelled(CtrlC)",
            130,
            "CANCELLED",
            "campaign_cancelled",
        ),
        (
            "INFRA_ERROR",
            "runner_not_completed",
            "Cancelled(SigTerm)",
            130,
            "INFRA_ERROR",
            "cancelled_exit_code_mismatch",
        ),
        (
            "INFRA_ERROR",
            "runner_not_completed",
            "Cancelled(User)",
            125,
            "INFRA_ERROR",
            "cancelled_exit_code_mismatch",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new();
        let mut receipt = fixture.receipt();
        receipt.attempt =
            json!({"outcome":outcome,"reason":reason,"runner":{"cause":cause},"exitCode":exit});
        finalize_cancel(&mut receipt, &cancelled);
        assert_eq!(receipt.attempt["outcome"], expected_outcome, "case {index}");
        assert_eq!(receipt.attempt["reason"], expected_reason, "case {index}");
        assert!(receipt.catalogue.is_null());
        assert!(receipt.initialization.is_null());
        assert!(receipt.namespace_plan.is_null());
    }
    let fixture = Fixture::new();
    let mut receipt = fixture.receipt();
    let before = receipt.attempt.clone();
    finalize_cancel(&mut receipt, &Cancellation::default());
    assert_eq!(receipt.attempt, before);
    assert!(!receipt.catalogue.is_null() && !receipt.initialization.is_null());
}

#[test]
fn pristine_baseline_requires_native_catalogue_and_valid_suite_observation() {
    for (catalogue, initialization, expected) in [
        (Value::Null, json!({}), "missing_native_catalogue"),
        (
            json!({}),
            Value::Null,
            "missing_native_initialization_evidence",
        ),
        (
            json!({}),
            json!({"observation":{"observation":{"suites":[7]}}}),
            "missing_native_initialization_evidence",
        ),
    ] {
        let fixture = Fixture::new();
        let mut receipt = fixture.receipt();
        receipt.catalogue = catalogue;
        receipt.initialization = initialization;
        validate_baseline(&mut receipt, true);
        assert_eq!(receipt.attempt["outcome"], "INFRA_ERROR");
        assert_eq!(receipt.attempt["reason"], expected);
        assert!(receipt.catalogue.is_null() && receipt.initialization.is_null());
    }
    let fixture = Fixture::new();
    let mut valid = fixture.receipt();
    validate_baseline(&mut valid, true);
    assert_eq!(valid.attempt["outcome"], "SURVIVED");
    valid.catalogue = Value::Null;
    validate_baseline(&mut valid, false);
    assert_eq!(valid.attempt["outcome"], "SURVIVED");
    valid.attempt["outcome"] = "KILLED".into();
    validate_baseline(&mut valid, true);
    assert_eq!(valid.attempt["outcome"], "KILLED");
}

#[test]
fn source_integrity_failure_revokes_namespace_cache_too() {
    let fixture = Fixture::new();
    let mut receipt = fixture.receipt();
    override_infra(&mut receipt, "source_input_changed");
    assert_eq!(receipt.attempt["reason"], "source_input_changed");
    assert!(receipt.catalogue.is_null() && receipt.initialization.is_null());
    assert!(
        receipt.namespace_plan.is_null(),
        "a plan bound to changed original input must not be cached"
    );
}

#[test]
fn initial_infrastructure_verdict_never_retains_private_capabilities() {
    for reason in [
        "native_owner_failed",
        "runner_capture_issue",
        "author_test_selection_changed",
    ] {
        let fixture = Fixture::new();
        let mut receipt = fixture.receipt();
        receipt.attempt = infra(reason);
        finalize_cancel(&mut receipt, &Cancellation::default());
        assert_eq!(receipt.attempt["outcome"], "INFRA_ERROR");
        assert_eq!(receipt.attempt["reason"], reason);
        assert!(receipt.catalogue.is_null());
        assert!(receipt.initialization.is_null());
        assert!(receipt.namespace_plan.is_null());
    }
}

#[test]
fn report_binding_detects_retargeted_existing_parent_alias_before_capture_creation() {
    let fixture = Fixture::new();
    let reports = fixture.0.join("reports");
    let source = fixture.0.join("source");
    fs::create_dir(&reports).unwrap();
    fs::create_dir(&source).unwrap();
    fs::write(source.join("sentinel"), b"original").unwrap();
    let alias = fixture.0.join("alias");
    symlink(&reports, &alias).unwrap();
    let case = alias.join("nested/case");
    let report = crate::attempt_artifacts::create(&case, std::slice::from_ref(&source)).unwrap();
    report.verify().unwrap();
    assert_eq!(
        report.canonical_path().unwrap(),
        reports.join("nested/case")
    );
    fs::remove_file(&alias).unwrap();
    symlink(&source, &alias).unwrap();
    assert!(
        report.verify().is_err(),
        "Runner's lexical capture parent was redirected but the artifact guard accepted it"
    );
    assert!(report.canonical_path().is_err());
    assert!(report.write_private("result.json", b"receipt").is_err());
    assert!(!source.join("nested").exists());
    assert_eq!(fs::read(source.join("sentinel")).unwrap(), b"original");
    assert!(Path::new(&reports.join("nested/case")).is_dir());
}
