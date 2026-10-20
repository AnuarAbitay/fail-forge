use super::*;
use failforge_dart::{ByteEdit, Mutant, MutationFamily, Symbol, ValidationState, sha256_hex};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
fn put(root: &Path, relative: &str, raw: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, raw).unwrap();
}
struct Fixture {
    base: PathBuf,
    project: PreparedProject,
    mutant: Mutant,
}
impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-mutant-owner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("subject Ж");
        let dependency = base.join("test-package");
        put(&dependency, "bin/test.dart", b"void main() {}\n");
        put(&root, "pubspec.yaml", b"name: subject\n");
        let source = b"const value = true;\n";
        put(&root, "lib/a.dart", source);
        put(&root, "test/a_test.dart", b"void main() {}\n");
        put(&root, ".dart_tool/package_config.json", serde_json::to_vec(&json!({
            "configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/"},
                {"name":"test","rootUri":format!("file://{}/",dependency.display()),"packageUri":"lib/"}]
        })).unwrap());
        put(&root, ".dart_tool/package_graph.json", br#"{"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]}"#);
        let project = inspect_project(&root, &Cancellation::default()).unwrap();
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: "BOOLEAN_LITERAL_FLIP".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/a.dart".into(),
            source_hash: sha256_hex(source),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 14,
                length_bytes: 4,
                expected: "true".into(),
                replacement: "false".into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        Self {
            base,
            project,
            mutant,
        }
    }
    fn candidate(&self, valid: bool) -> Value {
        let patched = "const value = false;\n";
        json!({"index":0,"mutant":self.mutant,"validation":{"mutantId":self.mutant.id,
            "valid":valid,"validation":if valid {"valid"} else {"invalid"},"diagnostics":[]},
            "safePatch":if valid {json!({"applied":true,"writesPerformed":false,"mutantId":self.mutant.id,
                "file":self.mutant.file,"patchedHash":sha256_hex(patched.as_bytes()),"patchedBytes":patched.len(),"patchedUtf8":patched})}
            else {json!({"applied":false,"writesPerformed":false,"mutantId":self.mutant.id,"file":self.mutant.file,"reason":"static_invalid"})}})
    }
    fn value(&self, valid: bool) -> Value {
        json!({"operation":"mutant","project":self.project,
            "case":self.base.join("reports").join(&self.mutant.id),
            "runtime":{"installation":self.base.join("tools"),"runner":self.base.join("tools/failforge"),
                "dartSdk":self.base.join("sdk"),"flutterSdk":null,"adapterRoot":self.base.join("adapter")},
            "config":{"commandTimeout":"6s","testTimeout":"1s"},
            "workspace":{"root":self.base.join("registry"),"campaign":"campaign-00000000000000000000000000000000"},
            "candidate":self.candidate(valid),
            "context":{"expected":[["test/a_test.dart","author"]],
                "baseline":{"rawSha256":"0".repeat(64),"cwd":"/baseline","identities":[["test/a_test.dart","author"]],"view":null},
                "mutant":self.mutant,"alias":"subject","initialization":{"role":"initial","candidateValid":valid,
                    "baseline":{"outcome":"SURVIVED","reason":"user_tests_passed","testIdentities":[["test/a_test.dart","author"]],
                        "observation":{"suites":["test/a_test.dart"],"failedSuite":null,"signature":null}}}},
            "namespacePlan":null,"frozen":{},"environment":{"CI":"true"}})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}
fn decoded(value: &Value) -> Result<Request, NamespaceError> {
    decode(&serde_json::to_vec(value).unwrap())
}

#[test]
fn in_process_and_wire_request_decoders_share_all_binding_validation() {
    let fixture = Fixture::new();
    for valid in [true, false] {
        let value = fixture.value(valid);
        let request = decode_value(value.clone()).unwrap();
        assert_eq!(request.candidate, decoded(&value).unwrap().candidate);
        let mut invalid = value;
        invalid["context"]["alias"] = "unbound".into();
        assert!(decode_value(invalid.clone()).is_err());
        assert!(decoded(&invalid).is_err());
    }
}

#[tokio::test]
async fn owned_static_invalid_seals_without_wire_and_pins_the_exact_lease_until_drop() {
    use failforge_workspace::lifecycle::{close, start};

    let fixture = Fixture::new();
    fs::create_dir(fixture.base.join("sdk")).unwrap();
    fs::create_dir(fixture.base.join("adapter")).unwrap();
    let registry = fixture.base.join("registry");
    let started = start(&registry, &Cancellation::default()).unwrap();
    let campaign = started.campaign_name;
    let mut close_caps = started.caps.try_clone().unwrap();
    close_caps.lease.take();
    let mut value = fixture.value(false);
    value["workspace"]["campaign"] = campaign.clone().into();
    let request = decode_value(value).unwrap();
    let case = request.case.clone();
    let owned = run_owned(request, started.caps, &Cancellation::default())
        .await
        .unwrap();
    assert_eq!(owned.record["outcome"], "INVALID");
    assert_eq!(owned.record["compileChecked"], false);
    assert!(owned.private_response.is_none());
    assert!(owned.namespace_plan.is_none());
    assert!(!owned.namespace_checked);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(case.join("result.json")).unwrap()).unwrap(),
        owned.record
    );
    assert!(
        close(
            &registry,
            &campaign,
            close_caps.try_clone().unwrap(),
            &Cancellation::default()
        )
        .unwrap()
        .cleanup_deferred
    );
    drop(owned);
    assert!(
        !close(&registry, &campaign, close_caps, &Cancellation::default())
            .unwrap()
            .cleanup_deferred
    );
}

#[tokio::test]
async fn standalone_mutant_retains_the_original_private_envelope_and_sealed_report() {
    use failforge_workspace::lifecycle::start;

    let fixture = Fixture::new();
    fs::create_dir(fixture.base.join("sdk")).unwrap();
    fs::create_dir(fixture.base.join("adapter")).unwrap();
    let started = start(&fixture.base.join("registry"), &Cancellation::default()).unwrap();
    let mut value = fixture.value(false);
    value["workspace"]["campaign"] = started.campaign_name.into();
    let request = decode_value(value).unwrap();
    let case = request.case.clone();
    let owned = run_owned_inner(request, started.caps, &Cancellation::default(), true)
        .await
        .unwrap();
    let bytes = owned.private_response.as_ref().unwrap();
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).unwrap(),
        json!({"version":1,"result":{"record":owned.record,
            "namespacePlan":null,"namespaceChecked":false}})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(case.join("result.json")).unwrap()).unwrap(),
        owned.record
    );
}

#[test]
fn final_source_drift_preserves_diagnostics_revokes_confirmation_and_beats_cancellation() {
    for outcome in ["KILLED", "SURVIVED", "CANCELLED", "INFRA_ERROR"] {
        let mut record = json!({"compileChecked":true,"outcome":outcome,
            "reason":"original_reason", "runner":{"receipt":"preserved"},
            "initializationConfirmation":{"confirmed":true,"attempts":[]}});
        finalize_source(&mut record, Err(invalid("changed"))).unwrap();
        assert_eq!(record["outcome"], "INFRA_ERROR");
        assert_eq!(
            record["reason"],
            if outcome == "INFRA_ERROR" {
                "original_reason"
            } else {
                "source_input_changed"
            }
        );
        assert_eq!(record["runner"], json!({"receipt":"preserved"}));
        assert_eq!(record["initializationConfirmation"]["confirmed"], false);
        let cancel = Cancellation::default();
        cancel.request(libc::SIGTERM);
        finalize_cancel(&mut record, &cancel);
        assert_eq!(record["outcome"], "INFRA_ERROR");
    }
    let mut invalid_record = json!({"compileChecked":false,"outcome":"INVALID"});
    assert!(finalize_source(&mut invalid_record, Err(invalid("changed"))).is_err());
    let original = invalid_record.clone();
    finalize_source(&mut invalid_record, Ok(())).unwrap();
    assert_eq!(invalid_record, original);
}

#[test]
fn failed_attempt_cannot_hide_failed_final_source_check_as_cancellation() {
    let failure = finalize_result(
        Err(NamespaceError::Cancelled(libc::SIGTERM)),
        Err(invalid("original changed")),
    )
    .err()
    .unwrap();
    assert!(!matches!(failure, NamespaceError::Cancelled(_)));
    assert!(failure.to_string().contains("original changed"));
    let cancellation = finalize_result(Err(NamespaceError::Cancelled(libc::SIGTERM)), Ok(()))
        .err()
        .unwrap();
    assert!(matches!(
        cancellation,
        NamespaceError::Cancelled(libc::SIGTERM)
    ));
}

#[test]
fn native_mutant_request_accepts_bound_valid_and_static_invalid_candidates() {
    let fixture = Fixture::new();
    for valid in [true, false] {
        let value = fixture.value(valid);
        let request = decoded(&value).unwrap();
        assert_eq!(
            request.context.mutant,
            Some(value["candidate"]["mutant"].clone())
        );
        let proof = verify_candidate(&request.project.root, &request.candidate).unwrap();
        assert_eq!(proof.patch_sha256.is_some(), valid);
    }
    assert!(!fixture.base.join("reports").exists());
}

#[test]
fn native_mutant_request_rejects_ambiguous_unknown_or_redirected_fields() {
    let fixture = Fixture::new();
    let value = fixture.value(true);
    assert!(decoded(&json!([])).is_err());
    for parent in [
        "",
        "/project",
        "/runtime",
        "/config",
        "/workspace",
        "/context",
    ] {
        let mut bad = value.clone();
        bad.pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), true.into());
        assert!(decoded(&bad).is_err(), "unknown {parent}");
    }
    for (pointer, bad_value) in [
        ("/operation", json!("attempt")),
        ("/case", json!("relative")),
        ("/case", json!(fixture.base.join("wrong-mutant-id"))),
        ("/runtime/installation", json!("relative")),
        ("/runtime/runner", json!("/bad\0runner")),
        ("/runtime/dartSdk", json!("relative")),
        ("/runtime/adapterRoot", json!("relative")),
        ("/workspace/root", json!("relative")),
        ("/workspace/campaign", json!("campaign-invalid")),
        ("/project/root", json!("relative")),
        ("/project/packages/test", json!("relative")),
        ("/frozen", json!([])),
        ("/context/expected", Value::Null),
        ("/context/baseline", Value::Null),
        ("/context/alias", json!("other")),
        ("/context/mutant/file", json!("lib/other.dart")),
        ("/context/initialization", Value::Null),
        ("/context/initialization/candidateValid", json!(false)),
        ("/candidate/validation/valid", json!(1)),
        ("/candidate/safePatch/patchedHash", Value::Null),
        ("/config/commandTimeout", json!("5999ms")),
    ] {
        let mut bad = value.clone();
        *bad.pointer_mut(pointer).unwrap() = bad_value;
        assert!(decoded(&bad).is_err(), "{pointer}");
    }
    for environment in [
        json!({"":"value"}),
        json!({"A=B":"value"}),
        json!({"A":"bad\0value"}),
        json!({"A":true}),
    ] {
        let mut bad = value.clone();
        bad["environment"] = environment;
        assert!(decoded(&bad).is_err());
    }
    let raw = serde_json::to_string(&value).unwrap();
    let ambiguous = raw.replacen(
        "\"operation\":\"mutant\"",
        "\"operation\":\"mutant\",\"operation\":\"mutant\"",
        1,
    );
    assert!(decode(ambiguous.as_bytes()).is_err());
    assert!(!fixture.base.join("reports").exists());
}

#[test]
fn native_mutant_request_nested_dtos_require_named_objects() {
    let fixture = Fixture::new();
    let value = fixture.value(true);
    for (field, array) in [
        ("config", json!(["6s", "1s"])),
        (
            "workspace",
            json!([
                fixture.base.join("registry"),
                "campaign-00000000000000000000000000000000"
            ]),
        ),
        (
            "runtime",
            json!([
                fixture.base.join("tools"),
                fixture.base.join("tools/failforge"),
                fixture.base.join("sdk"),
                null,
                fixture.base.join("adapter")
            ]),
        ),
    ] {
        let mut bad = value.clone();
        bad[field] = array;
        assert!(decoded(&bad).is_err(), "positional {field} was accepted");
    }
}

#[test]
fn mutation_record_preserves_source_metadata_and_only_dart_specific_category() {
    let fixture = Fixture::new();
    for (family, operator, category) in [
        (MutationFamily::Classic, "BOOLEAN_LITERAL_FLIP", None),
        (MutationFamily::FailureMode, "EXCEPTION_AT_ENTRY", None),
        (
            MutationFamily::LanguageSpecific,
            "NSM-001",
            Some("null_safety"),
        ),
        (MutationFamily::LanguageSpecific, "FUTURE_OPERATOR", None),
    ] {
        let mut mutant = fixture.mutant.clone();
        mutant.family = family;
        mutant.operator_id = operator.into();
        mutant.id = mutant.stable_id();
        let raw = json!(mutant);
        let validation = json!({"mutantId":mutant.id,"valid":true,"validation":"valid","diagnostics":[{"message":"original diagnostic"}]});
        let verified = VerifiedCandidate {
            mutant,
            raw_mutant: raw.clone(),
            validation: validation.clone(),
            patch_sha256: Some("0".repeat(64)),
        };
        let value = record(&verified);
        assert_eq!(value["recordKind"], "mutation");
        assert_eq!(value["mutantId"], raw["id"]);
        assert_eq!(value["family"], raw["family"]);
        assert_eq!(value["operatorId"], raw["operatorId"]);
        assert_eq!(value["file"], raw["file"]);
        assert_eq!(value["edit"], raw["edit"]);
        assert_eq!(value["validation"], validation);
        assert_eq!(
            value.get("language").and_then(Value::as_str),
            if family == MutationFamily::LanguageSpecific {
                Some("dart")
            } else {
                None
            }
        );
        assert_eq!(value.get("category").and_then(Value::as_str), category);
        for private in [
            "patchSha256",
            "safePatch",
            "context",
            "namespacePlan",
            "initialization",
        ] {
            assert!(value.get(private).is_none());
        }
    }
}

#[test]
fn original_guard_and_candidate_check_detect_root_metadata_and_target_drift() {
    let fixture = Fixture::new();
    let guard = InputGuard::new(&fixture.project).unwrap();
    let target = fixture.project.root.join(&fixture.mutant.file);
    let original = fs::read(&target).unwrap();
    fs::write(&target, b"changed source").unwrap();
    assert!(verify_candidate(&fixture.project.root, &fixture.candidate(false)).is_err());
    fs::write(&target, original).unwrap();
    guard.verify().unwrap();
    let graph = fixture.project.root.join(".dart_tool/package_graph.json");
    let original = fs::read(&graph).unwrap();
    let mut changed: Value = serde_json::from_slice(&original).unwrap();
    changed["packages"][0]["dependencies"] = json!([]);
    fs::write(&graph, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(guard.verify().is_err());
    fs::write(&graph, original).unwrap();
    guard.verify().unwrap();
    let displaced = fixture.base.join("displaced");
    fs::rename(&fixture.project.root, &displaced).unwrap();
    fs::create_dir(&fixture.project.root).unwrap();
    assert!(guard.verify().is_err());
    fs::remove_dir(&fixture.project.root).unwrap();
    fs::rename(displaced, &fixture.project.root).unwrap();
    guard.verify().unwrap();
    let mut forged = fixture.project.clone();
    forged.name = "other".into();
    assert!(InputGuard::new(&forged).is_err());
}

#[test]
fn final_cancellation_revokes_a_confirmed_kill_while_integrity_failure_wins() {
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    for outcome in ["SURVIVED", "KILLED", "TIMEOUT", "UNCLASSIFIED"] {
        let mut value = json!({"outcome":outcome,"reason":"reason","initializationConfirmation":{"confirmed":true,"attempts":[{"role":"original-after"}]}});
        let attempts = value["initializationConfirmation"]["attempts"].clone();
        finalize_cancel(&mut value, &cancel);
        assert_eq!(value["outcome"], "CANCELLED");
        assert_eq!(value["reason"], "campaign_cancelled");
        assert_eq!(value["initializationConfirmation"]["confirmed"], false);
        assert_eq!(value["initializationConfirmation"]["attempts"], attempts);
    }
    let original = json!({"outcome":"INFRA_ERROR","reason":"source_input_changed","initializationConfirmation":{"confirmed":false,"attempts":[]}});
    let mut value = original.clone();
    finalize_cancel(&mut value, &cancel);
    assert_eq!(value, original);
    let original = json!({"outcome":"KILLED","reason":"mutation_caused_initialization_failure","initializationConfirmation":{"confirmed":true,"attempts":[]}});
    let mut value = original.clone();
    finalize_cancel(&mut value, &Cancellation::default());
    assert_eq!(value, original);
}
