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
fn typed_and_wire_decoders_share_validation() {
    let mut value = value();
    assert_eq!(
        super::decode_value(value.clone()).unwrap().workers,
        decode_value(&value).unwrap().workers
    );
    value["workers"] = 5.into();
    assert!(super::decode_value(value.clone()).is_err());
    assert!(decode_value(&value).is_err());
    value["workers"] = 2.into();
    let mut typed = super::decode_value(value.clone()).unwrap();
    typed.frozen = json!([]);
    assert!(validate_request(&typed).is_err());
    value["frozen"] = json!([]);
    assert!(decode_value(&value).is_err());
    typed.frozen = json!({});
    typed.project.graph = Value::Null;
    assert!(validate_request(&typed).is_err());
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
    assert_eq!(guard.canonical_path().unwrap(), original.join("reports"));
    fs::remove_file(&alias).unwrap();
    symlink(&replacement, &alias).unwrap();
    assert!(guard.verify().is_err());
    assert!(guard.canonical_path().is_err());
    let guard = DirectoryGuard::open(&original.join("reports")).unwrap();
    fs::rename(original.join("reports"), original.join("moved")).unwrap();
    fs::create_dir(original.join("reports")).unwrap();
    assert!(guard.verify().is_err());
    let symlink_path = temp.0.join("direct-alias");
    symlink(&original, &symlink_path).unwrap();
    assert!(DirectoryGuard::open(&symlink_path).is_err());
}

fn put(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn owned_fixture() -> (Temp, Request, LifecycleCaps) {
    let temp = Temp::new();
    let root = temp.0.join("subject");
    let dependency = temp.0.join("test");
    put(&dependency, "bin/test.dart", "void main() {}\n");
    put(&root, "pubspec.yaml", "name: subject\n");
    put(&root, "lib/a.dart", "const value = true;\n");
    put(&root, "test/a_test.dart", "void main() {}\n");
    put(&root, ".dart_tool/package_config.json", serde_json::to_vec(&json!({
        "configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/"},
            {"name":"test","rootUri":format!("file://{}/",dependency.display()),"packageUri":"lib/"}]
    })).unwrap());
    put(&root, ".dart_tool/package_graph.json", br#"{"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]}"#);
    for relative in [
        "sdk/bin/dart",
        "sdk/lib/core.dart",
        "tools/failforge",
        "tools/failforge-adapter-campaign",
        "tools/python/failforge_campaign/controller.py",
        "tools/scripts/helper",
        "adapter/lib/main.dart",
    ] {
        put(&temp.0, relative, "fixture\n");
    }
    put(&temp.0, "adapter/.dart_tool/package_config.json", br#"{"configVersion":2,"packages":[{"name":"adapter","rootUri":"../","packageUri":"lib/"}]}"#);
    let project = inspect_project(&root, &Cancellation::default()).unwrap();
    let started =
        failforge_workspace::lifecycle::start(&temp.0.join("registry"), &Cancellation::default())
            .unwrap();
    fs::create_dir(temp.0.join("reports")).unwrap();
    let mut value = value();
    value["project"] = serde_json::to_value(project).unwrap();
    value["output"] = json!(temp.0.join("reports"));
    value["runtime"] = json!({"installation":temp.0.join("tools"),
        "runner":temp.0.join("tools/failforge"),"dartSdk":temp.0.join("sdk"),
        "flutterSdk":null,"adapterRoot":temp.0.join("adapter")});
    value["workspace"] = json!({"root":started.root_path,"campaign":started.campaign_name});
    value["emptyPolicy"] = "skip".into();
    let mut request = decode_value(&value).unwrap();
    request.frozen = crate::workspace_inputs::bind_inputs(
        &request.runtime,
        &request.project,
        &request.environment,
        &Cancellation::default(),
    )
    .unwrap();
    (temp, request, started.caps)
}

struct OwnedSink {
    registry: PathBuf,
    campaign: String,
    close_caps: LifecycleCaps,
    events: Vec<Value>,
    terminal: Option<Value>,
    fail_emit: bool,
    fail_finish: bool,
}
impl OwnedSink {
    fn new(request: &Request, caps: &LifecycleCaps) -> Self {
        let mut close_caps = caps.try_clone().unwrap();
        close_caps.lease.take();
        Self {
            registry: request.workspace.root.clone(),
            campaign: request.workspace.campaign.clone(),
            close_caps,
            events: vec![],
            terminal: None,
            fail_emit: false,
            fail_finish: false,
        }
    }

    fn assert_released(&self) {
        assert!(
            !failforge_workspace::lifecycle::close(
                &self.registry,
                &self.campaign,
                self.close_caps.try_clone().unwrap(),
                &Cancellation::default(),
            )
            .unwrap()
            .cleanup_deferred
        );
    }
}
impl FamilyEvents for OwnedSink {
    fn emit(&mut self, event: Value) -> io::Result<()> {
        self.events.push(event);
        if self.fail_emit {
            Err(io::Error::other("injected family sink failure"))
        } else {
            Ok(())
        }
    }

    fn finish(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed: usize,
        total: usize,
    ) -> io::Result<()> {
        assert!(self.terminal.is_none());
        assert!(
            failforge_workspace::lifecycle::close(
                &self.registry,
                &self.campaign,
                self.close_caps.try_clone().unwrap(),
                &Cancellation::default(),
            )
            .unwrap()
            .cleanup_deferred,
            "the family lease must stay held during the outer completion callback"
        );
        self.terminal = Some(json!({"status":status,"error":error,
            "completedMutants":completed,"expectedMutants":total}));
        if self.fail_finish {
            Err(io::Error::other("injected family finish failure"))
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn shared_sink_receives_empty_completion_while_lease_is_held() {
    let (_temp, request, caps) = owned_fixture();
    let mut sink = OwnedSink::new(&request, &caps);
    run_owned(request, caps, &Cancellation::default(), &mut sink)
        .await
        .unwrap();
    assert!(sink.events.is_empty());
    assert_eq!(
        sink.terminal,
        Some(json!({"status":"no_mutants","error":null,"completedMutants":0,"expectedMutants":0}))
    );
    sink.assert_released();
}

#[tokio::test]
async fn shared_sink_failures_stop_before_child_execution_and_release_only_after_finish() {
    let (_temp, mut request, caps) = owned_fixture();
    request.empty_policy = EmptyPolicy::Controls;
    let output = request.output.clone();
    let mut sink = OwnedSink::new(&request, &caps);
    sink.fail_emit = true;
    run_owned(request, caps, &Cancellation::default(), &mut sink)
        .await
        .unwrap();
    assert_eq!(
        sink.events,
        [json!({"kind":"stage_started","stage":"baseline"})]
    );
    assert_eq!(sink.terminal.as_ref().unwrap()["status"], "failed");
    assert!(
        sink.terminal.as_ref().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("injected family sink failure")
    );
    assert!(!output.join("baseline").exists());
    sink.assert_released();

    let (_temp, request, caps) = owned_fixture();
    let mut sink = OwnedSink::new(&request, &caps);
    sink.fail_finish = true;
    let error = run_owned(request, caps, &Cancellation::default(), &mut sink)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("injected family finish failure"));
    sink.assert_released();
}

#[tokio::test]
async fn typed_family_request_is_revalidated_before_effects() {
    let (_temp, mut request, caps) = owned_fixture();
    request.workers = 0;
    let mut sink = OwnedSink::new(&request, &caps);
    assert!(
        run_owned(request, caps, &Cancellation::default(), &mut sink)
            .await
            .is_err()
    );
    assert!(sink.events.is_empty());
    assert!(sink.terminal.is_none());
    sink.assert_released();
}
