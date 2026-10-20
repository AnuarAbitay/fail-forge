use super::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

fn value() -> Value {
    json!({"operation":"family","project":{"root":"/tmp/subject","name":"subject",
        "packages":{"subject":"/tmp/subject","test":"/tmp/test"},"graph":{},
        "flutter":false,"frameworkCollision":false},"output":"/tmp/reports",
        "runtime":{"installation":"/tmp/tools","runner":"/tmp/tools/failforge",
            "dartSdk":"/tmp/sdk","flutterSdk":null,"adapterRoot":"/tmp/adapter"},
        "config":{"commandTimeout":"6s","testTimeout":"1s"},
        "workspace":{"root":"/tmp/registry","campaign":"campaign-00000000000000000000000000000000"},
        "candidates":[],"family":"classic","workers":2,"emptyPolicy":"controls",
        "frozen":{},"environment":{"CI":"true"}})
}
fn decode_value(value: &Value) -> Result<Request, NamespaceError> {
    decode(&serde_json::to_vec(value).unwrap())
}

#[test]
fn family_decode_preserves_empty_policies_and_bounded_worker_configuration() {
    for policy in ["controls", "skip"] {
        for workers in [1, 4] {
            let mut value = value();
            value["emptyPolicy"] = policy.into();
            value["workers"] = workers.into();
            let request = decode_value(&value).unwrap();
            assert_eq!(request.workers, workers as usize);
            assert_eq!(request.empty_policy == EmptyPolicy::Skip, policy == "skip");
        }
    }
}

#[test]
fn family_decode_rejects_unknown_fields_shapes_limits_and_unsafe_paths() {
    let original = value();
    for pointer in [
        "",
        "/project",
        "/project/packages",
        "/project/graph",
        "/runtime",
        "/config",
        "/workspace",
        "/frozen",
        "/environment",
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = json!([]);
        assert!(decode_value(&bad).is_err(), "{pointer}");
    }
    for pointer in ["", "/project", "/runtime", "/config", "/workspace"] {
        let mut bad = original.clone();
        bad.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), true.into());
        assert!(decode_value(&bad).is_err(), "{pointer}");
    }
    for (pointer, bad_value) in [
        ("/operation", json!("mutant")),
        ("/workers", json!(0)),
        ("/workers", json!(5)),
        ("/workers", json!(true)),
        ("/workers", json!(1.0)),
        ("/family", json!("other")),
        ("/emptyPolicy", json!("other")),
        ("/output", json!("relative")),
        ("/runtime/installation", json!("relative")),
        ("/runtime/runner", json!("/tmp/\0runner")),
        ("/project/root", json!("relative")),
        ("/workspace/campaign", json!("invalid")),
        ("/config/commandTimeout", json!("1s")),
        ("/candidates", json!(vec![Value::Null; 10001])),
        ("/environment", json!({"":"bad"})),
        ("/environment", json!({"A=B":"bad"})),
        ("/environment", json!({"A":"bad\0value"})),
    ] {
        let mut bad = original.clone();
        *bad.pointer_mut(pointer).unwrap() = bad_value;
        assert!(decode_value(&bad).is_err(), "{pointer}");
    }
    let raw = serde_json::to_string(&original)
        .unwrap()
        .replace("\"workers\":2", "\"workers\":2,\"workers\":2");
    assert!(decode(raw.as_bytes()).is_err());
}

#[test]
fn family_decode_rejects_duplicate_and_mixed_family_candidates() {
    let mut value = value();
    value["candidates"] = json!([{"mutant":{"id":"a","family":"classic"}}]);
    assert!(decode_value(&value).is_ok()); // Complete proof is checked against disk in prepare.
    value["candidates"] =
        json!([{"mutant":{"id":"a","family":"classic"}},{"mutant":{"id":"a","family":"classic"}}]);
    assert!(decode_value(&value).is_err());
    value["candidates"] = json!([{"mutant":{"id":"a","family":"language_specific"}}]);
    assert!(decode_value(&value).is_err());
}

#[test]
fn ledger_retains_completion_order_independence_and_rejects_unbound_records() {
    let mut value = value();
    value["candidates"] = json!([
        {"mutant":{"id":"a","family":"classic"}},{"mutant":{"id":"b","family":"classic"}}]);
    let request = decode_value(&value).unwrap();
    let a = json!({"recordKind":"mutation","mutantId":"a","family":"classic","outcome":"KILLED","reason":"user_test_failure"});
    let b = json!({"recordKind":"mutation","mutantId":"b","family":"classic","outcome":"INFRA_ERROR","reason":"source_input_changed"});
    let mut ledger = Ledger::default();
    let send_failed = ledger.publish(&request, 0, &a, || Err(invalid("frame exceeds limit")));
    assert!(send_failed.is_err());
    assert!(ledger.ids.is_empty());
    assert!(!ledger.inconclusive && !ledger.infrastructure);
    assert!(ledger.publish(&request, 0, &b, || Ok(())).is_err());
    assert!(ledger.publish(&request, 2, &a, || Ok(())).is_err());
    for (field, bad_value) in [
        ("family", json!("failure_mode")),
        ("outcome", json!("LOAD_FAILURE")),
        ("reason", Value::Null),
    ] {
        let mut bad = a.clone();
        bad[field] = bad_value;
        assert!(ledger.publish(&request, 0, &bad, || Ok(())).is_err());
        assert!(ledger.ids.is_empty());
    }
    ledger.publish(&request, 1, &b, || Ok(())).unwrap();
    ledger.publish(&request, 0, &a, || Ok(())).unwrap();
    assert!(ledger.publish(&request, 0, &a, || Ok(())).is_err());
    assert_eq!(ledger.ids.len(), 2);
    assert!(ledger.inconclusive && ledger.infrastructure);
}

#[test]
fn terminal_success_requires_complete_and_conclusive_published_records() {
    let cancel = Cancellation::default();
    let mut ledger = Ledger::default();
    ledger.ids.insert("a".into());
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 1).0, "completed");
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 2).0, "failed");
    ledger.inconclusive = true;
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 1).0, "failed");
}

#[test]
fn passing_controls_and_family_terminals_never_hide_integrity_as_cancellation() {
    let cancel = Cancellation::default();
    let ledger = Ledger::default();
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 0).0, "no_mutants");
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 1).0, "failed");
    assert!(require_passing(&json!({"outcome":"SURVIVED"}), &cancel, "bad baseline").is_ok());
    assert!(require_passing(&json!({"outcome":"KILLED"}), &cancel, "bad baseline").is_err());
    cancel.request(libc::SIGTERM);
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 1).0, "cancelled");
    let failure =
        require_passing(&json!({"outcome":"INFRA_ERROR"}), &cancel, "integrity").unwrap_err();
    assert!(!matches!(failure, NamespaceError::Cancelled(_)));
    assert_eq!(
        terminal_state(Err(failure), &cancel, &ledger, 1).0,
        "failed"
    );
    let ledger = Ledger {
        infrastructure: true,
        ..Default::default()
    };
    assert_eq!(
        terminal_state(
            Err(NamespaceError::Cancelled(libc::SIGTERM)),
            &cancel,
            &ledger,
            1
        )
        .0,
        "failed"
    );
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 1).0, "failed");
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-family-guard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn directory_guard_detects_replacements_and_ancestor_alias_retargets() {
    let temp = Temp::new();
    let original = temp.0.join("original");
    let replacement = temp.0.join("replacement");
    fs::create_dir_all(original.join("reports")).unwrap();
    fs::create_dir_all(replacement.join("reports")).unwrap();
    let alias = temp.0.join("alias");
    symlink(&original, &alias).unwrap();
    let guard = DirectoryGuard::open(&alias.join("reports")).unwrap();
    guard.verify().unwrap();
    fs::remove_file(&alias).unwrap();
    symlink(&replacement, &alias).unwrap();
    assert!(guard.verify().is_err());
    let guard = DirectoryGuard::open(&original.join("reports")).unwrap();
    fs::rename(original.join("reports"), original.join("moved")).unwrap();
    fs::create_dir(original.join("reports")).unwrap();
    assert!(guard.verify().is_err());
    let symlink_path = temp.0.join("direct-alias");
    symlink(&original, &symlink_path).unwrap();
    assert!(DirectoryGuard::open(&symlink_path).is_err());
}
