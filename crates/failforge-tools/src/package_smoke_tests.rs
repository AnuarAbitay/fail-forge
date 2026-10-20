use super::*;

fn evidence(profile: bool) -> (Value, Value) {
    let healthy = |path: &str| json!({"outcome":"SURVIVED","exitCode":0,"reporterComplete":true,"temporaryCopyCleaned":true,"runner":{"cause":"ProcessCompleted","cleanup":"complete","status":"exit:0"},"tests":{"started":1,"failed":0,"passed":1,"skipped":0,"hidden":0,"loadingPassed":1,"loadingFailed":0},"loadFailures":[],"testIdentities":[["test/value_test.dart","enabled"]],"isolatedWorkingDirectory":path});
    let mut record = healthy("/temporary/mutant");
    record["outcome"] = "KILLED".into();
    record["exitCode"] = 1.into();
    record["runner"]["status"] = "exit:1".into();
    record["tests"]["failed"] = 1.into();
    record["tests"]["passed"] = 0.into();
    record["mutantId"] = "a".repeat(64).into();
    record["operatorId"] = "BOOLEAN_LITERAL_FLIP".into();
    record["file"] = "lib/value.dart".into();
    record["compileChecked"] = true.into();
    record["validation"] = json!({"valid":true});
    let family = json!({"version":1,"family":"classic","complete":true,"status":"completed","completedMutants":1,"expectedMutants":1,"outcomeCounts":{"KILLED":1},"originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,"workspaceCleanupDeferred":false,"allDiscoveredMutantsProcessed":true,"error":null,"mutationScore":{"denominator":1,"killed":1,"survived":0,"percent":100.0},"records":[record],"baseline":healthy("/temporary/baseline"),"originalControls":[healthy("/temporary/original-after")]});
    let mut summary = family.clone();
    if profile {
        summary["version"] = 2.into();
        summary["kind"] = "profile".into();
        summary["mutationScore"] = Value::Null;
        summary["families"] = json!({"classic":{"enabled":true,"complete":true,"summary":family}});
    }
    (
        summary,
        json!({"configuration":{"adapter_root":"/installation/adapters/dart","dart_sdk":"/sdk","project":"/subject"}}),
    )
}

fn check(summary: &Value, manifest: &Value, profile: bool) -> Result<(String, Vec<PathBuf>)> {
    campaign(
        summary,
        manifest,
        Path::new("/installation"),
        Path::new("/sdk"),
        Path::new("/subject"),
        profile,
    )
}

#[test]
fn healthy_direct_and_profile_require_matching_mutant_and_fresh_controls() {
    for profile in [false, true] {
        let (summary, manifest) = evidence(profile);
        let (id, paths) = check(&summary, &manifest, profile).unwrap();
        assert_eq!(id, "a".repeat(64));
        assert_eq!(paths.len(), 3);
    }
}

#[test]
fn final_success_cannot_hide_incomplete_cleanup_counts_or_baseline_failure() {
    for (pointer, replacement) in [
        ("/complete", json!(false)),
        ("/status", json!("cancelled")),
        ("/temporarySnapshotCleaned", json!(false)),
        ("/workspaceCleanupDeferred", json!(true)),
        ("/originalProjectFilesUnchanged", json!(false)),
        ("/expectedMutants", json!(2)),
        ("/outcomeCounts", json!({"KILLED":1,"INFRA_ERROR":1})),
        ("/baseline/runner/cleanup", json!("incomplete")),
        ("/baseline/outcome", json!("KILLED")),
        ("/baseline/reporterComplete", json!(false)),
        ("/baseline/tests/loadingFailed", json!(1)),
        ("/originalControls", json!([])),
        ("/records/0/temporaryCopyCleaned", json!(false)),
        (
            "/records/0/isolatedWorkingDirectory",
            json!("/temporary/baseline"),
        ),
        ("/records/0/isolatedWorkingDirectory", json!("/subject")),
        ("/records/0/validation/valid", json!(false)),
        (
            "/records/0/testIdentities",
            json!([["test/value_test.dart", "different"]]),
        ),
        ("/mutationScore/percent", json!(0.0)),
    ] {
        let (mut summary, manifest) = evidence(false);
        *summary.pointer_mut(pointer).unwrap() = replacement;
        assert!(check(&summary, &manifest, false).is_err(), "{pointer}");
    }
}

#[test]
fn profile_root_must_agree_with_its_completed_family() {
    for pointer in [
        "/families/classic/complete",
        "/families/classic/summary/complete",
        "/families/classic/summary/originalProjectFilesUnchanged",
    ] {
        let (mut summary, manifest) = evidence(true);
        *summary.pointer_mut(pointer).unwrap() = false.into();
        assert!(check(&summary, &manifest, true).is_err(), "{pointer}");
    }
    let (mut summary, manifest) = evidence(true);
    summary["records"][0]["mutantId"] = "b".repeat(64).into();
    assert!(check(&summary, &manifest, true).is_err());
}

#[test]
fn fallback_to_build_checkout_or_wrong_sdk_is_rejected() {
    for key in ["adapter_root", "dart_sdk", "project"] {
        let (summary, mut manifest) = evidence(false);
        manifest["configuration"][key] = "/build/checkout".into();
        assert!(check(&summary, &manifest, false).is_err(), "{key}");
    }
}

#[test]
fn cli_version_must_match_the_archive_manifest() {
    let manifest = json!({"version":"0.1.0-beta.1"});
    assert!(version("failforge 0.1.0-beta.1\n", &manifest).is_ok());
    for text in [
        "failforge 0.1.0",
        "other 0.1.0-beta.1",
        "failforge 0.1.0-beta.1\nextra",
    ] {
        assert!(version(text, &manifest).is_err(), "{text}");
    }
    assert!(version("failforge 0.1.0-beta.1", &json!({})).is_err());
}

fn package_fixture() -> (common::Temporary, Value) {
    let temporary = common::Temporary::new("ff-smoke-payload").unwrap();
    let mut files = Vec::new();
    for path in [
        "bin/failforge",
        "bin/failforge-adapter-campaign",
        "adapters/dart/pubspec.yaml",
        "adapters/dart/pubspec.lock",
    ] {
        let full = temporary.0.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, "fixture").unwrap();
        let executable = path.starts_with("bin/");
        fs::set_permissions(
            &full,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .unwrap();
        files.push(json!({"path":path,"bytes":7,"sha256":common::hash(&full).unwrap(),"executable":executable}));
    }
    (
        temporary,
        json!({"schemaVersion":1,"version":"0.1.0-beta.1","files":files}),
    )
}

#[test]
fn manifest_rejects_missing_changed_or_aliased_payload_files() {
    let (temporary, manifest) = package_fixture();
    assert_eq!(payload(&temporary.0, &manifest).unwrap(), 4);
    for (pointer, value) in [
        ("/files/0/path", json!("bin/missing")),
        ("/files/0/path", json!("../escaped")),
        ("/files/0/bytes", json!(8)),
        ("/files/0/sha256", json!("0".repeat(64))),
        ("/files/0/executable", json!(false)),
    ] {
        let mut changed = manifest.clone();
        *changed.pointer_mut(pointer).unwrap() = value;
        assert!(payload(&temporary.0, &changed).is_err(), "{pointer}");
    }
    let mut missing = manifest.clone();
    missing["files"].as_array_mut().unwrap().pop();
    assert!(payload(&temporary.0, &missing).is_err());
    let path = temporary.0.join("adapters/dart/pubspec.lock");
    fs::write(&path, "changed").unwrap();
    assert!(payload(&temporary.0, &manifest).is_err());
    fs::remove_file(&path).unwrap();
    symlink("pubspec.yaml", &path).unwrap();
    assert!(payload(&temporary.0, &manifest).is_err());
}

#[test]
fn invalid_output_inside_installation_or_sdk_creates_nothing() {
    let (temporary, _) = package_fixture();
    let sdk = common::Temporary::new("ff-smoke-sdk").unwrap();
    fs::create_dir(sdk.0.join("bin")).unwrap();
    fs::write(sdk.0.join("bin/dart"), "fixture").unwrap();
    fs::set_permissions(sdk.0.join("bin/dart"), fs::Permissions::from_mode(0o755)).unwrap();
    let outside = common::Temporary::new("ff-smoke-output").unwrap();
    symlink(&temporary.0, outside.0.join("alias")).unwrap();
    for output in [
        temporary.0.join("new/deep"),
        sdk.0.join("new/deep"),
        outside.0.join("alias/new/deep"),
    ] {
        assert!(
            run(Options {
                installation: temporary.0.clone(),
                dart_sdk: sdk.0.clone(),
                output,
                offline: false
            })
            .is_err()
        );
        assert!(!temporary.0.join("new").exists());
        assert!(!sdk.0.join("new").exists());
    }
    let candidate = outside.0.join("new/deep");
    assert_eq!(
        fresh_output(&candidate, &temporary.0, &sdk.0).unwrap(),
        candidate
    );
    assert!(!outside.0.join("new").exists());
    assert!(fresh_output(&outside.0, &temporary.0, &sdk.0).is_err());
}
