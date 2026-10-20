use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn put(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    root: PathBuf,
    request: Value,
}
impl Fixture {
    fn new(flutter: bool) -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-campaign-workspace-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let project = root.join("subject");
        let dependency = root.join("test-package");
        let test_package = if flutter { "flutter_test" } else { "test" };
        put(&dependency, "lib/test.dart", "void main() {}\n");
        put(&project, "pubspec.yaml", "name: subject\n");
        put(&project, "lib/a.dart", "const value = true;\n");
        put(&project, "test/a_test.dart", "void main() {}\n");
        put(&project, ".dart_tool/package_config.json", serde_json::to_vec(&json!({
            "configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/"},
                {"name":test_package,"rootUri":format!("file://{}/",dependency.display()),"packageUri":"lib/"}]
        })).unwrap());
        put(
            &project,
            ".dart_tool/package_graph.json",
            serde_json::to_vec(&json!({
                "roots":["subject"],"packages":[{"name":"subject","dependencies":[test_package]},
                    {"name":test_package,"dependencies":[]}]
            }))
            .unwrap(),
        );
        let sdk = if flutter {
            root.join("flutter/bin/cache/dart-sdk")
        } else {
            root.join("sdk")
        };
        put(&sdk, "bin/dart", "#!/bin/sh\nexit 0\n");
        put(&sdk, "lib/core.dart", "library core;\n");
        for relative in [
            "tools/failforge",
            "tools/failforge-adapter-campaign",
            "tools/python/failforge_campaign/controller.py",
            "tools/scripts/helper",
            "adapter/bin/failforge_dart_adapter.dart",
        ] {
            put(&root, relative, "fixture\n");
        }
        put(&root, "adapter/.dart_tool/package_config.json", br#"{"configVersion":2,"packages":[{"name":"adapter","rootUri":"../","packageUri":"lib/"}]}"#);
        for path in [
            sdk.join("bin/dart"),
            root.join("tools/failforge"),
            root.join("tools/failforge-adapter-campaign"),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        if flutter {
            put(&root, "flutter/bin/flutter", "#!/bin/sh\nexit 0\n");
            fs::set_permissions(
                root.join("flutter/bin/flutter"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let request = json!({"operation":"campaign-run","mode":"profile",
            "runId":"1234567890abcdef1234567890abcdef","project":project,
            "output":null,"registry":root.join("registry"),
            "runtime":{"installation":root.join("tools"),"runner":root.join("tools/failforge"),
                "dartSdk":sdk,"flutterSdk":null,"adapterRoot":root.join("adapter")},
            "environment":{"CI":"true"},"maxMutants":4,
            "families":[{"family":"classic","rules":{"includePaths":["lib/a.dart"]},"workers":1,
                "config":{"commandTimeout":"6s","testTimeout":"1s"}}]});
        Self { root, request }
    }

    fn decode(&self) -> Request {
        decode(&serde_json::to_vec(&self.request).unwrap()).unwrap()
    }

    fn ready(&self) -> Preflight {
        preflight(&self.decode(), &Cancellation::default()).unwrap()
    }

    fn untouched(&self) {
        assert!(!self.root.join("subject/.failforge").exists());
        assert!(!self.root.join("registry").exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn bootstrap_decoder_rejects_bad_shapes_identities_and_configuration_without_io() {
    let fixture = Fixture::new(false);
    let original = fixture.request.clone();
    for (pointer, bad) in [
        ("/operation", json!("campaign")),
        ("/mode", json!("other")),
        ("/runId", json!("A".repeat(32))),
        ("/runId", json!("a".repeat(31))),
        ("/project", json!("relative")),
        ("/output", json!("relative")),
        ("/registry", json!("relative")),
        ("/maxMutants", json!(10001)),
        ("/maxMutants", json!(true)),
        ("/families", json!([])),
        ("/families/0/rules", json!([])),
        ("/families/0/config", json!(["6s", "1s"])),
        ("/families/0/config/commandTimeout", json!("1s")),
        ("/families/0/workers", json!(0)),
        ("/environment", json!([])),
        ("/environment", json!({"A=B":"x"})),
        ("/runtime", json!([])),
    ] {
        let mut bad_request = original.clone();
        *bad_request.pointer_mut(pointer).unwrap() = bad;
        assert!(
            decode(&serde_json::to_vec(&bad_request).unwrap()).is_err(),
            "{pointer}"
        );
    }
    for key in ["output", "registry"] {
        let mut bad = original.clone();
        bad.as_object_mut().unwrap().remove(key);
        assert!(decode(&serde_json::to_vec(&bad).unwrap()).is_err());
    }
    let mut bad = original;
    bad["unexpected"] = true.into();
    assert!(decode(&serde_json::to_vec(&bad).unwrap()).is_err());
    assert!(decode(br#"{"operation":"campaign-run","operation":"campaign-run"}"#).is_err());
    fixture.untouched();
}

#[test]
fn preflight_resolves_native_defaults_and_preserves_explicit_empty_selection() {
    let mut fixture = Fixture::new(false);
    let original = fixture.request["families"][0].clone();
    fixture.request["families"] = json!([
        {"family":"language_specific","rules":{},"workers":1,"config":original["config"]},
        {"family":"failure_mode","rules":{},"workers":1,"config":original["config"]},
        original]);
    let ready = fixture.ready();
    assert_eq!(
        ready
            .families
            .iter()
            .map(|family| family.family.as_str())
            .collect::<Vec<_>>(),
        FAMILIES
    );
    assert!(ready.families[0].rules.get("operators").is_none());
    assert_eq!(
        ready.families[1].rules["operators"],
        json!(["EXCEPTION_AT_ENTRY"])
    );
    assert_eq!(
        ready.families[2].rules["operators"]
            .as_array()
            .unwrap()
            .len(),
        19
    );
    assert_eq!(ready.run_id, fixture.request["runId"].as_str().unwrap());
    fixture.request["families"][2]["rules"]["operators"] = json!([]);
    assert_eq!(fixture.ready().families[0].rules["operators"], json!([]));
    fixture.untouched();

    let mut flutter = Fixture::new(true);
    flutter.request["families"][0]["family"] = "failure_mode".into();
    let ready = flutter.ready();
    assert!(ready.original.flutter);
    assert_eq!(
        ready.runtime.flutter_sdk,
        Some(flutter.root.join("flutter"))
    );
    assert_eq!(
        ready.families[0].rules["operators"],
        json!(["EXCEPTION_AT_ENTRY", "METHOD_CHANNEL_ERROR"])
    );
    flutter.untouched();
}

#[test]
fn literal_paths_runtime_failures_and_flutter_only_selection_fail_before_output() {
    let mut fixture = Fixture::new(false);
    for pattern in ["lib/typo.dart", "../a.dart", "/a.dart", "lib\\a.dart"] {
        fixture.request["families"][0]["rules"]["includePaths"] = json!([pattern]);
        assert!(preflight(&fixture.decode(), &Cancellation::default()).is_err());
        fixture.untouched();
    }
    fixture.request["families"][0]["rules"]["includePaths"] = json!(["lib/*.dart"]);
    assert!(preflight(&fixture.decode(), &Cancellation::default()).is_ok());
    fixture.request["families"][0]["rules"]["operators"] = json!(["METHOD_CHANNEL_ERROR"]);
    assert!(preflight(&fixture.decode(), &Cancellation::default()).is_err());
    fixture.request["families"][0]["rules"]["operators"] = json!([]);
    fs::set_permissions(
        fixture.root.join("sdk/bin/dart"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(preflight(&fixture.decode(), &Cancellation::default()).is_err());
    fixture.untouched();

    let fixture = Fixture::new(false);
    fs::remove_file(fixture.root.join("adapter/.dart_tool/package_config.json")).unwrap();
    assert!(preflight(&fixture.decode(), &Cancellation::default()).is_err());
    fixture.untouched();
    let fixture = Fixture::new(false);
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    assert!(matches!(
        preflight(&fixture.decode(), &cancel),
        Err(NamespaceError::Cancelled(_))
    ));
    fixture.untouched();
}

#[test]
fn preflight_rejects_output_and_registry_overlap_without_creating_directories() {
    let mut fixture = Fixture::new(false);
    for output in [
        "subject/reports",
        "sdk/reports",
        "test-package/reports",
        "registry/reports",
    ] {
        fixture.request["output"] = json!(fixture.root.join(output));
        assert!(preflight(&fixture.decode(), &Cancellation::default()).is_err());
    }
    fixture.request["output"] = Value::Null;
    for registry in [
        "subject/cache",
        "sdk/workspaces",
        "tools/scripts/workspaces",
    ] {
        fixture.request["registry"] = json!(fixture.root.join(registry));
        assert!(preflight(&fixture.decode(), &Cancellation::default()).is_err());
    }
    fixture.untouched();
}

#[test]
fn output_is_fresh_private_and_aliases_are_rejected_even_after_preflight() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let guard = create_output(&ready).unwrap();
    for path in [&ready.output, &ready.output.join("families/classic")] {
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o700);
    }
    guard.verify().unwrap();
    put(&ready.output, "sentinel", "retained");
    assert!(create_output(&ready).is_err());
    assert_eq!(
        fs::read(ready.output.join("sentinel")).unwrap(),
        b"retained"
    );

    let mut alias = Fixture::new(false);
    alias.request["output"] = json!(alias.root.join("alias/reports"));
    let ready = alias.ready();
    let outside = alias.root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, alias.root.join("alias")).unwrap();
    assert!(create_output(&ready).is_err());
    assert!(!outside.join("reports").exists());
    assert!(preflight(&alias.decode(), &Cancellation::default()).is_err());
}

#[test]
fn fresh_snapshot_rebases_package_metadata_binds_inputs_and_cleans_explicitly() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let _output = create_output(&ready).unwrap();
    let cancel = Cancellation::default();
    let mut workspace = Workspace::start(&ready, &cancel).unwrap();
    assert_eq!(workspace.root(), ready.registry);
    assert!(workspace.campaign().starts_with("campaign-"));
    assert_eq!(workspace.recovery(), json!({"recovered":[],"skipped":[]}));
    let directory = workspace.directory();
    let copied = workspace.snapshot(&ready, &cancel).unwrap();
    assert_eq!(workspace.snapshot_path(), Some(copied.root.as_path()));
    assert_eq!(copied.packages["subject"], copied.root);
    assert_eq!(copied.packages["test"], ready.original.packages["test"]);
    assert_eq!(
        fs::read(copied.root.join("lib/a.dart")).unwrap(),
        b"const value = true;\n"
    );
    assert!(workspace.snapshot(&ready, &cancel).is_err());
    let bound = workspace
        .bind_inputs(&ready, &fixture.decode().environment, &cancel)
        .unwrap();
    assert_eq!(
        bound["project"],
        json!(source_hashes(&copied.root, &cancel).unwrap())
    );
    verify_original(&ready).unwrap();
    let cleanup = workspace.finish(&ready);
    assert!(cleanup.original_unchanged && cleanup.snapshot_cleaned);
    assert!(!cleanup.cleanup_deferred);
    assert!(cleanup.errors.is_empty());
    assert!(!directory.exists());
    assert!(!copied.root.exists());
    assert!(ready.output.exists());
}

#[test]
fn partial_copy_cancellation_and_bind_failure_still_release_the_owned_workspace() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let cancel = Cancellation::default();
    let mut workspace = Workspace::start(&ready, &cancel).unwrap();
    let directory = workspace.directory();
    let result = workspace.snapshot_using(&ready, &cancel, |_, destination, cancel| {
        put(destination, "lib/partial.dart", "partial copy");
        cancel.request(libc::SIGTERM);
        Err(NamespaceError::Cancelled(libc::SIGTERM))
    });
    assert!(matches!(result, Err(NamespaceError::Cancelled(_))));
    assert!(workspace.snapshot_path().unwrap().exists());
    let cleanup = workspace.finish(&ready);
    assert!(cleanup.snapshot_cleaned && cleanup.original_unchanged);
    assert!(!cleanup.cleanup_deferred);
    assert!(cleanup.errors.is_empty());
    assert!(!directory.exists());

    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let mut workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    let directory = workspace.directory();
    assert!(
        workspace
            .bind_inputs(&ready, &BTreeMap::new(), &Cancellation::default())
            .is_err()
    );
    workspace
        .snapshot(&ready, &Cancellation::default())
        .unwrap();
    fs::remove_file(fixture.root.join("tools/failforge-adapter-campaign")).unwrap();
    assert!(
        workspace
            .bind_inputs(&ready, &BTreeMap::new(), &Cancellation::default())
            .is_err()
    );
    let cleanup = workspace.finish(&ready);
    assert!(cleanup.snapshot_cleaned && !cleanup.cleanup_deferred);
    assert!(!directory.exists());
}

#[test]
fn original_drift_and_replacement_never_skip_cleanup() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let mut workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    workspace
        .snapshot(&ready, &Cancellation::default())
        .unwrap();
    let directory = workspace.directory();
    put(&ready.original.root, "lib/a.dart", "const value = false;\n");
    let cleanup = workspace.finish(&ready);
    assert!(!cleanup.original_unchanged);
    assert!(!cleanup.errors.is_empty());
    assert!(cleanup.snapshot_cleaned && !cleanup.cleanup_deferred);
    assert!(!directory.exists());
    assert!(verify_original(&ready).is_err());

    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    fs::rename(&ready.original.root, fixture.root.join("moved")).unwrap();
    fs::create_dir(&ready.original.root).unwrap();
    assert!(verify_original(&ready).is_err());
}

#[test]
fn retained_child_lease_reports_deferred_cleanup_and_can_be_reaped_after_release() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let mut workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    workspace
        .snapshot(&ready, &Cancellation::default())
        .unwrap();
    let campaign = workspace.campaign().to_owned();
    let mut retained = workspace.caps().unwrap();
    let cleanup = workspace.finish(&ready);
    assert!(cleanup.snapshot_cleaned && cleanup.original_unchanged);
    assert!(cleanup.cleanup_deferred);
    assert!(!cleanup.skipped.is_empty());
    assert!(cleanup.errors.is_empty());
    assert!(ready.registry.join(&campaign).exists());
    retained.lease.take();
    let closed = lifecycle::close(
        &ready.registry,
        &campaign,
        retained,
        &Cancellation::default(),
    )
    .unwrap();
    assert!(!closed.cleanup_deferred);
    assert!(!ready.registry.join(campaign).exists());
}

#[test]
fn unsafe_cleanup_is_reported_and_never_claimed_as_completed() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let mut workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    workspace
        .snapshot(&ready, &Cancellation::default())
        .unwrap();
    let directory = workspace.directory();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let cleanup = workspace.finish(&ready);
    assert!(cleanup.cleanup_deferred);
    assert!(!cleanup.snapshot_cleaned);
    assert!(!cleanup.errors.is_empty());
    assert!(directory.exists());
}

#[test]
fn replaced_snapshot_allocation_must_not_be_removed_by_generic_campaign_cleanup() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let mut workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    workspace
        .snapshot(&ready, &Cancellation::default())
        .unwrap();
    let allocation = workspace.snapshot.as_ref().unwrap().path().to_owned();
    let moved = fixture.root.join("moved-snapshot");
    fs::rename(&allocation, &moved).unwrap();
    fs::create_dir(&allocation).unwrap();
    fs::set_permissions(&allocation, fs::Permissions::from_mode(0o700)).unwrap();
    put(&allocation, "foreign-sentinel", "must survive");
    let cleanup = workspace.finish(&ready);
    assert!(
        allocation.join("foreign-sentinel").exists(),
        "generic campaign cleanup must not bypass the failed allocation identity check"
    );
    assert_eq!(
        fs::read(allocation.join("foreign-sentinel")).unwrap(),
        b"must survive"
    );
    assert!(moved.join("package/lib/a.dart").exists());
    assert!(!cleanup.snapshot_cleaned);
    assert!(cleanup.cleanup_deferred);
    assert!(!cleanup.errors.is_empty());
    let restarted = lifecycle::start(&ready.registry, &Cancellation::default()).unwrap();
    assert!(
        restarted
            .skipped
            .iter()
            .any(|skip| skip.reason == "unrecognized_campaign_entry")
    );
    assert_eq!(
        fs::read(allocation.join("foreign-sentinel")).unwrap(),
        b"must survive"
    );
    assert!(moved.join("package/lib/a.dart").exists());
    let mut caps = restarted.caps;
    caps.lease.take();
    assert!(
        !lifecycle::close(
            &restarted.root_path,
            &restarted.campaign_name,
            caps,
            &Cancellation::default()
        )
        .unwrap()
        .cleanup_deferred
    );
}

#[test]
fn cleanup_quarantine_uses_the_retained_campaign_even_when_registry_path_is_retargeted() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let mut workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    workspace
        .snapshot(&ready, &Cancellation::default())
        .unwrap();
    let campaign = workspace.campaign().to_owned();
    let moved_registry = fixture.root.join("moved-registry");
    fs::rename(&ready.registry, &moved_registry).unwrap();
    let foreign = fixture.root.join("foreign-registry");
    put(
        &foreign,
        &format!("{campaign}/sentinel"),
        "foreign contents",
    );
    symlink(&foreign, &ready.registry).unwrap();

    let cleanup = workspace.finish(&ready);
    assert!(cleanup.cleanup_deferred);
    assert!(!cleanup.snapshot_cleaned);
    assert!(!cleanup.errors.is_empty());
    assert_eq!(
        fs::read(foreign.join(&campaign).join("sentinel")).unwrap(),
        b"foreign contents"
    );
    assert!(!foreign.join(&campaign).join(".cleanup-blocked").exists());
    let marker = moved_registry.join(&campaign).join(".cleanup-blocked");
    assert_eq!(fs::metadata(&marker).unwrap().mode() & 0o7777, 0o600);
    assert_eq!(fs::read(marker).unwrap(), b"FAILFORGE_CLEANUP_BLOCKED_V1\n");
    assert!(moved_registry.join(campaign).join("work").exists());
}

#[test]
fn cleanup_quarantine_never_overwrites_existing_entries_or_follows_their_links() {
    let fixture = Fixture::new(false);
    let ready = fixture.ready();
    let workspace = Workspace::start(&ready, &Cancellation::default()).unwrap();
    let directory = workspace.directory();
    let outside = fixture.root.join("outside-marker");
    fs::write(&outside, "untouched").unwrap();
    symlink(&outside, directory.join(".cleanup-blocked")).unwrap();
    block_cleanup(&workspace.caps.campaign).unwrap();
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    assert!(
        fs::symlink_metadata(directory.join(".cleanup-blocked"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let cleanup = workspace.finish(&ready);
    assert!(cleanup.cleanup_deferred);
    assert_eq!(fs::read(outside).unwrap(), b"untouched");
}
