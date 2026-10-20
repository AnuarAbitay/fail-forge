use super::*;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use failforge_workspace::{inspect_project, source_hashes};
use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn put(root: &Path, name: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    base: PathBuf,
    project: PreparedProject,
    sdk: PathBuf,
    adapter: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-discovery-owner-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("subject Ж");
        let dependency = base.join("test dependency");
        put(&dependency, "bin/test.dart", b"void main() {}\n");
        put(&root, "pubspec.yaml", b"name: subject\n");
        put(&root, "lib/value.dart", b"const value = true;\n");
        put(&root, "test/value_test.dart", b"void main() {}\n");
        put(&root, ".dart_tool/package_config.json", serde_json::to_vec(&json!({
            "configVersion":2,"packages":[
                {"name":"subject","rootUri":"../","packageUri":"lib/"},
                {"name":"test","rootUri":format!("file://{}/",dependency.display()),"packageUri":"lib/"}
            ]
        })).unwrap());
        put(&root, ".dart_tool/package_graph.json", br#"{"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]}"#);
        let sdk = base.join("sdk");
        let adapter = base.join("adapter");
        put(&sdk, "bin/dart", b"not executed by binding tests\n");
        put(&sdk, "lib/core/core.dart", b"library dart.core;\n");
        put(
            &adapter,
            "bin/failforge_dart_adapter.dart",
            b"void main() {}\n",
        );
        put(
            &adapter,
            ".dart_tool/package_config.json",
            br#"{"configVersion":2,"packages":[]}"#,
        );
        let project = inspect_project(&root, &Cancellation::default()).unwrap();
        Self {
            base,
            project,
            sdk,
            adapter,
        }
    }

    fn value(&self) -> Value {
        json!({
            "operation":"discover", "project":self.project,
            "directory":self.base.join("reports/discovery"),
            "runtime":{"dartSdk":self.sdk,"adapterRoot":self.adapter,"helper":self.base.join("helper")},
            "family":"classic",
            "payload":{"workspaceRoot":self.project.root,"packageRoot":".","dartSdkPath":self.sdk,
                "rules":{"preset":"classic"},"limits":{"maxMutants":0}}
        })
    }

    fn request(&self) -> Request {
        decode(&serde_json::to_vec(&self.value()).unwrap()).unwrap()
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
fn discovery_request_schema_rejects_unknown_ambiguous_and_mistyped_fields() {
    let fixture = Fixture::new();
    let valid = fixture.value();
    for family in ["classic", "failure_mode", "language_specific"] {
        let mut value = valid.clone();
        value["family"] = family.into();
        assert!(decoded(&value).is_ok(), "{family}");
    }
    for parent in ["", "/project", "/runtime"] {
        let mut value = valid.clone();
        value
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
        assert!(decoded(&value).is_err(), "{parent}");
    }
    for (pointer, bad) in [
        ("/operation", json!("run")),
        ("/family", json!("Classical")),
        ("/family", json!(null)),
        ("/project/flutter", json!(1)),
        ("/project/frameworkCollision", json!("false")),
        ("/directory", json!("relative")),
        ("/runtime/dartSdk", json!("relative")),
        ("/runtime/adapterRoot", json!("/bad\0root")),
        ("/runtime/helper", json!("relative")),
        ("/project/root", json!("relative")),
        ("/project/packages/test", json!("relative")),
        ("/payload", json!([])),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).unwrap() = bad;
        assert!(decoded(&value).is_err(), "{pointer}");
    }
    let raw = serde_json::to_string(&valid).unwrap();
    for (old, new) in [
        (
            "\"operation\":\"discover\"",
            "\"operation\":\"discover\",\"operation\":\"discover\"",
        ),
        ("\"maxMutants\":0", "\"maxMutants\":0,\"maxMutants\":0"),
        ("\"maxMutants\":0", "\"maxMutants\":18446744073709551617"),
        ("\"maxMutants\":0", "\"maxMutants\":NaN"),
    ] {
        assert!(raw.contains(old));
        assert!(
            decode(raw.replacen(old, new, 1).as_bytes()).is_err(),
            "{new}"
        );
    }
}

#[test]
fn discovery_binding_rejects_forged_prepared_metadata_before_report_creation() {
    let fixture = Fixture::new();
    let valid = fixture.value();
    InputBinding::new(&fixture.request(), &Cancellation::default())
        .unwrap()
        .verify()
        .unwrap();
    for (pointer, bad) in [
        ("/project/name", json!("different")),
        ("/project/frameworkCollision", json!(true)),
        ("/project/packages/test", json!(fixture.adapter)),
        ("/project/graph/roots", json!(["different"])),
    ] {
        let mut value = valid.clone();
        *value.pointer_mut(pointer).unwrap() = bad;
        let request = decoded(&value).unwrap();
        assert!(
            InputBinding::new(&request, &Cancellation::default()).is_err(),
            "{pointer}"
        );
        assert!(!fixture.base.join("reports").exists());
    }
}

#[test]
fn discovery_binding_detects_source_and_package_metadata_changes() {
    let fixture = Fixture::new();
    let binding = InputBinding::new(&fixture.request(), &Cancellation::default()).unwrap();
    for relative in [
        "lib/value.dart",
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ] {
        let path = fixture.project.root.join(relative);
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed.extend_from_slice(b" \n");
        fs::write(&path, changed).unwrap();
        assert!(binding.verify().is_err(), "{relative}");
        fs::write(&path, original).unwrap();
        binding.verify().unwrap();
    }
}

fn copy_bytes(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_bytes(&entry.path(), &destination.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
        }
    }
}

#[test]
fn discovery_binding_rejects_root_replacement_even_with_identical_bytes() {
    let fixture = Fixture::new();
    let token = Cancellation::default();
    let binding = InputBinding::new(&fixture.request(), &token).unwrap();
    let before = source_hashes(&fixture.project.root, &token).unwrap();
    let displaced = fixture.base.join("displaced");
    fs::rename(&fixture.project.root, &displaced).unwrap();
    copy_bytes(&displaced, &fixture.project.root);
    assert_eq!(
        source_hashes(&fixture.project.root, &token).unwrap(),
        before
    );
    assert!(binding.verify().is_err());
    fs::remove_dir_all(&fixture.project.root).unwrap();
    fs::rename(displaced, &fixture.project.root).unwrap();
    binding.verify().unwrap();
}

#[test]
fn discovery_payload_cannot_redirect_package_or_sdk_and_cancel_starts_no_work() {
    let fixture = Fixture::new();
    let mut value = fixture.value();
    value["payload"]["workspaceRoot"] = json!(fixture.base);
    value["payload"]["packageRoot"] =
        json!(fixture.project.root.file_name().unwrap().to_str().unwrap());
    InputBinding::new(&decoded(&value).unwrap(), &Cancellation::default())
        .unwrap()
        .verify()
        .unwrap();
    for (pointer, bad) in [
        ("/payload/workspaceRoot", json!(fixture.adapter)),
        ("/payload/packageRoot", json!("../sdk")),
        ("/payload/dartSdkPath", json!(fixture.adapter)),
        ("/payload/dartSdkPath", json!("relative")),
    ] {
        let mut bad_value = fixture.value();
        *bad_value.pointer_mut(pointer).unwrap() = bad;
        assert!(
            InputBinding::new(&decoded(&bad_value).unwrap(), &Cancellation::default()).is_err(),
            "{pointer}"
        );
    }
    let cancelled = Cancellation::default();
    cancelled.request(libc::SIGTERM);
    assert!(matches!(
        InputBinding::new(&fixture.request(), &cancelled),
        Err(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    assert!(!fixture.base.join("reports").exists());
}

#[test]
fn discovery_reports_cannot_create_descendants_through_protected_aliases() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let protected = vec![
        fixture.project.root.clone(),
        fixture.sdk.clone(),
        fixture.adapter.clone(),
        fixture.project.packages["test"].clone(),
    ];
    for (index, root) in protected.iter().enumerate() {
        let alias = fixture.base.join(format!("alias-{index}"));
        symlink(root, &alias).unwrap();
        for path in [
            root.join("never-created/discovery"),
            alias.join("never-created/discovery"),
        ] {
            assert!(crate::attempt_artifacts::create(&path, &protected).is_err());
            assert!(!root.join("never-created").exists());
        }
    }
}

#[test]
fn discovery_artifact_verification_rejects_replaced_transcript_inode() {
    let fixture = Fixture::new();
    let directory = fixture.base.join("reports/discovery");
    let report = crate::attempt_artifacts::create(&directory, &[]).unwrap();
    let original = report
        .write_private("adapter-session.jsonl", b"original\n")
        .unwrap();
    report
        .verify_file("adapter-session.jsonl", &original)
        .unwrap();
    fs::rename(
        directory.join("adapter-session.jsonl"),
        directory.join("displaced.jsonl"),
    )
    .unwrap();
    fs::write(directory.join("adapter-session.jsonl"), b"foreign\n").unwrap();
    assert!(
        report
            .verify_file("adapter-session.jsonl", &original)
            .is_err()
    );
    assert_eq!(
        fs::read(directory.join("adapter-session.jsonl")).unwrap(),
        b"foreign\n"
    );
    assert_eq!(
        fs::read(directory.join("displaced.jsonl")).unwrap(),
        b"original\n"
    );
}

fn cancelled_run() -> DiscoveryRun {
    DiscoveryRun {
        outcome: Err(DiscoveryError::Cancelled(libc::SIGTERM)),
        cleanup: json!({"complete":true,"runnerShutdown":true,"activeRuns":0,
            "campaignSucceeded":false,"cancelled":true,"cancellationReason":"SIGTERM",
            "validatedCandidates":0,"staticallyValidCandidates":0,"writesPerformed":false}),
        command_result: None,
    }
}

#[test]
fn discovery_artifact_hashes_reject_same_inode_tampering_after_owned_writes() {
    for name in ["payload.json", "adapter-session.jsonl"] {
        let fixture = Fixture::new();
        let mut prepared = prepare(fixture.request(), &Cancellation::default()).unwrap();
        prepared
            .transcript
            .write_all(b"{\"step\":\"cleanup\",\"payload\":{}}\n")
            .unwrap();
        verify_artifacts(&prepared, &cancelled_run()).unwrap();
        let path = prepared.directory.canonical_path().unwrap().join(name);
        let original = fs::read(&path).unwrap();
        let mut changed = original.clone();
        changed.extend_from_slice(b" \n");
        fs::write(&path, changed).unwrap();
        assert!(
            verify_artifacts(&prepared, &cancelled_run()).is_err(),
            "{name}"
        );
        fs::write(path, original).unwrap();
        verify_artifacts(&prepared, &cancelled_run()).unwrap();
    }
}

#[test]
fn discovery_source_integrity_failure_takes_priority_over_cancelled_outcome() {
    let fixture = Fixture::new();
    let prepared = prepare(fixture.request(), &Cancellation::default()).unwrap();
    let directory = prepared.directory.canonical_path().unwrap();
    fs::write(
        fixture.project.root.join("lib/value.dart"),
        b"changed by adapter\n",
    )
    .unwrap();
    let failure = finish(prepared, cancelled_run(), Instant::now()).unwrap_err();
    assert!(!matches!(failure, NamespaceError::Cancelled(_)));
    assert_eq!(exit_code(&failure), 125);
    let receipt: Value =
        serde_json::from_slice(&fs::read(directory.join("result.json")).unwrap()).unwrap();
    assert_eq!(receipt["exitCode"], 125);
    assert_eq!(receipt["freshStaticValidation"], false);
    assert_eq!(receipt["cleanup"]["cancellationReason"], "SIGTERM");
    assert!(receipt["error"].is_string());
    assert_eq!(
        fs::read(fixture.project.root.join("lib/value.dart")).unwrap(),
        b"changed by adapter\n"
    );
}

#[test]
fn discovery_payload_hash_matches_the_known_serialized_request_bytes() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let mut expected = serde_json::to_vec_pretty(&request.payload).unwrap();
    expected.push(b'\n');
    let prepared = prepare(request, &Cancellation::default()).unwrap();
    assert_eq!(
        prepared.payload_hash,
        format!("{:x}", Sha256::digest(&expected))
    );
    assert_eq!(
        fs::read(
            prepared
                .directory
                .canonical_path()
                .unwrap()
                .join("payload.json")
        )
        .unwrap(),
        expected
    );
}

#[test]
fn successful_discovery_requires_command_capture_evidence_but_prelaunch_cancel_does_not() {
    use failforge_dart::discovery::{DiscoveryResult, DiscoverySteps};
    let fixture = Fixture::new();
    let prepared = prepare(fixture.request(), &Cancellation::default()).unwrap();
    let mut run = cancelled_run();
    verify_artifacts(&prepared, &run).unwrap();
    run.outcome = Ok(DiscoveryResult {
        steps: DiscoverySteps {
            handshake: json!({}),
            discover: json!({}),
            cleanup: run.cleanup.clone(),
        },
        candidates: Vec::new(),
    });
    assert!(verify_artifacts(&prepared, &run).is_err());
}
