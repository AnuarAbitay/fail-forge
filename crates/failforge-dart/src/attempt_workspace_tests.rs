use super::*;
use crate::test_fs::symlink;
use crate::{ByteEdit, MutationFamily, Symbol, ValidationState};
use failforge_workspace::lifecycle::{self, StartResult};
use serde_json::json;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn put(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn duplicate(caps: &LifecycleCaps) -> LifecycleCaps {
    LifecycleCaps {
        root: caps.root.try_clone().unwrap(),
        campaign: caps.campaign.try_clone().unwrap(),
        work: caps.work.try_clone().unwrap(),
        lease: caps.lease.as_ref().map(|file| file.try_clone().unwrap()),
    }
}

struct Fixture {
    base: PathBuf,
    project: PreparedProject,
    campaign: StartResult,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-attempt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("subject Ж");
        let dependency = base.join("test dependency");
        put(&dependency, "bin/test.dart", b"void main() {}\n");
        put(&root, "pubspec.yaml", b"name: subject\n");
        put(&root, "lib/value.dart", b"const value = true;\n");
        put(
            &root,
            "test/value_test.dart",
            b"import 'package:subject/value.dart';\n",
        );
        put(&root, ".dart_tool/package_config.json", serde_json::to_vec(&json!({"configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/"},{"name":"test","rootUri":format!("{}/",crate::test_catalog::path_uri(&dependency).unwrap()),"packageUri":"lib/"}]})).unwrap());
        put(&root, ".dart_tool/package_graph.json", br#"{"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]}"#);
        let project = inspect_project(&root, &Cancellation::default()).unwrap();
        let campaign = lifecycle::start(&base.join("registry"), &Cancellation::default()).unwrap();
        Self {
            base,
            project,
            campaign,
        }
    }
    fn allocate(&self) -> AttemptWorkspace {
        AttemptWorkspace::allocate(
            &self.campaign.root_path,
            &self.campaign.campaign_name,
            duplicate(&self.campaign.caps),
            &Cancellation::default(),
        )
        .unwrap()
    }
    fn prepare(&self) -> AttemptWorkspace {
        let mut workspace = self.allocate();
        workspace
            .copy(&self.project, &Cancellation::default())
            .unwrap();
        workspace
    }
    fn mutant(&self) -> Mutant {
        let raw = fs::read(self.project.root.join("lib/value.dart")).unwrap();
        let mut value = Mutant {
            id: String::new(),
            operator_id: "BOOLEAN_LITERAL_FLIP".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/value.dart".into(),
            source_hash: sha256_hex(&raw),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 14,
                length_bytes: 4,
                expected: "true".into(),
                replacement: "false".into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Valid,
        };
        value.id = value.stable_id();
        value
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            crate::test_fs::remove_tree(&self.base);
        }
        #[cfg(unix)]
        fn writable(path: &Path) {
            if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                return;
            }
            let _ = fs::set_permissions(path, crate::test_fs::permissions(0o700));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        }
        #[cfg(unix)]
        {
            writable(&self.base);
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

#[test]
fn fresh_copy_namespace_readonly_patch_seal_verify_and_idempotent_cleanup() {
    let fixture = Fixture::new();
    fs::set_permissions(
        fixture.project.root.join("lib/value.dart"),
        crate::test_fs::permissions(0o444),
    )
    .unwrap();
    let before = source_hashes(&fixture.project.root, &Cancellation::default()).unwrap();
    let mut workspace = fixture.prepare();
    crate::namespace::isolate_subject(
        &fixture.project.root,
        workspace.copy_path(),
        &fixture.project.root,
        "failforge_subject",
        None,
        &Cancellation::default(),
    )
    .unwrap();
    let patched = b"const value = false;\n";
    workspace
        .apply_patch(
            &fixture.mutant(),
            &sha256_hex(patched),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(
        fs::read(workspace.copy_path().join("lib/value.dart")).unwrap(),
        patched
    );
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(workspace.copy_path().join("lib/value.dart"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o444
    );
    assert!(crate::test_fs::readonly(
        workspace.copy_path().join("lib/value.dart")
    ));
    let hashes = workspace.seal(&Cancellation::default()).unwrap();
    assert_eq!(hashes[Path::new("lib/value.dart")], sha256_hex(patched));
    workspace.verify_execution_inputs().unwrap();
    workspace.verify_original().unwrap();
    assert_eq!(
        source_hashes(&fixture.project.root, &Cancellation::default()).unwrap(),
        before
    );
    assert!(
        workspace
            .apply_patch(
                &fixture.mutant(),
                &sha256_hex(patched),
                &Cancellation::default()
            )
            .is_err()
    );
    assert!(workspace.seal(&Cancellation::default()).is_err());
    let path = workspace.copy_path().to_owned();
    workspace.cleanup().unwrap();
    workspace.cleanup().unwrap();
    assert!(!path.exists());
    workspace.verify_original().unwrap();
}

#[test]
fn forged_project_and_patch_bindings_fail_before_mutating_the_copy() {
    let fixture = Fixture::new();
    let mut workspace = fixture.allocate();
    let mut forged = fixture.project.clone();
    forged.framework_collision = true;
    assert!(
        workspace
            .copy(&forged, &Cancellation::default())
            .unwrap_err()
            .to_string()
            .contains("differs from inspected")
    );
    workspace.verify_original().unwrap();
    assert!(!workspace.copy_path().exists());
    workspace.cleanup().unwrap();
    let mut workspace = fixture.prepare();
    let before = source_hashes(workspace.copy_path(), &Cancellation::default()).unwrap();
    assert!(
        workspace
            .apply_patch(&fixture.mutant(), &"0".repeat(64), &Cancellation::default())
            .unwrap_err()
            .to_string()
            .contains("patch hash")
    );
    assert_eq!(
        source_hashes(workspace.copy_path(), &Cancellation::default()).unwrap(),
        before
    );
    let mut forged = fixture.mutant();
    forged.file = ".dart_tool/hidden.dart".into();
    forged.id = forged.stable_id();
    assert!(
        workspace
            .apply_patch(&forged, &"0".repeat(64), &Cancellation::default())
            .is_err()
    );
    workspace.cleanup().unwrap();
}

#[test]
fn copied_target_aliases_and_hardlinks_are_rejected_without_original_writes() {
    for hardlink in [false, true] {
        let fixture = Fixture::new();
        let mut workspace = fixture.prepare();
        let original = fs::read(fixture.project.root.join("lib/value.dart")).unwrap();
        let target = workspace.copy_path().join("lib/value.dart");
        fs::remove_file(&target).unwrap();
        if hardlink {
            fs::hard_link(fixture.project.root.join("lib/value.dart"), &target).unwrap();
        } else {
            symlink(fixture.project.root.join("lib/value.dart"), &target).unwrap();
        }
        assert!(
            workspace
                .apply_patch(
                    &fixture.mutant(),
                    &sha256_hex(b"const value = false;\n"),
                    &Cancellation::default()
                )
                .is_err()
        );
        assert_eq!(
            fs::read(fixture.project.root.join("lib/value.dart")).unwrap(),
            original
        );
        workspace.cleanup().unwrap();
    }
}

#[test]
fn cancellation_cannot_skip_cleanup_and_invalid_call_order_fails() {
    let fixture = Fixture::new();
    let cancelled = Cancellation::default();
    cancelled.request(libc::SIGTERM);
    assert!(
        AttemptWorkspace::allocate(
            &fixture.campaign.root_path,
            &fixture.campaign.campaign_name,
            duplicate(&fixture.campaign.caps),
            &cancelled
        )
        .is_err()
    );
    let mut workspace = fixture.allocate();
    workspace.verify_original().unwrap();
    assert!(workspace.seal(&Cancellation::default()).is_err());
    assert!(workspace.verify_execution_inputs().is_err());
    assert!(matches!(
        workspace.copy(&fixture.project, &cancelled),
        Err(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    workspace.cleanup().unwrap();
    let mut workspace = fixture.prepare();
    assert!(matches!(
        workspace.apply_patch(
            &fixture.mutant(),
            &sha256_hex(b"const value = false;\n"),
            &cancelled
        ),
        Err(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    workspace.cleanup().unwrap();
    assert!(!workspace.copy_path().exists());
}

#[test]
fn original_and_execution_drift_are_detected_with_cancellation_independent_verification() {
    for original in [false, true] {
        let fixture = Fixture::new();
        let mut workspace = fixture.prepare();
        workspace.seal(&Cancellation::default()).unwrap();
        let root = if original {
            &fixture.project.root
        } else {
            workspace.copy_path()
        };
        fs::write(root.join("lib/value.dart"), b"const value = changed;\n").unwrap();
        if original {
            assert!(workspace.verify_original().is_err());
        } else {
            assert!(workspace.verify_execution_inputs().is_err());
            workspace.verify_original().unwrap();
        }
        workspace.cleanup().unwrap();
    }
}

#[test]
fn replaced_allocation_is_never_removed_and_can_be_restored_for_retry() {
    let fixture = Fixture::new();
    let mut workspace = fixture.prepare();
    let allocation = workspace.copy_path().parent().unwrap().to_owned();
    let moved = fixture.base.join("retained original allocation");
    fs::rename(&allocation, &moved).unwrap();
    fs::create_dir(&allocation).unwrap();
    fs::set_permissions(&allocation, crate::test_fs::permissions(0o700)).unwrap();
    fs::write(allocation.join("foreign"), b"keep").unwrap();
    assert!(
        workspace
            .cleanup()
            .unwrap_err()
            .to_string()
            .contains("allocation_changed")
    );
    assert_eq!(fs::read(allocation.join("foreign")).unwrap(), b"keep");
    assert!(moved.join("package").is_dir());
    fs::remove_dir_all(&allocation).unwrap();
    fs::rename(moved, allocation).unwrap();
    workspace.cleanup().unwrap();
}

#[test]
fn cleanup_failure_preserves_payload_and_is_retryable() {
    let fixture = Fixture::new();
    let mut workspace = fixture.prepare();
    let deep = workspace.copy_path().join("too_deep");
    fs::create_dir(&deep).unwrap();
    let mut current = deep.clone();
    for _ in 0..129 {
        current = current.join("d");
        fs::create_dir(&current).unwrap();
    }
    let target = workspace.copy_path().join("lib/value.dart");
    let before = fs::read(&target).unwrap();
    assert!(
        workspace
            .cleanup()
            .unwrap_err()
            .to_string()
            .contains("depth_limit")
    );
    assert_eq!(fs::read(target).unwrap(), before);
    fs::remove_dir_all(deep).unwrap();
    workspace.cleanup().unwrap();
}

#[test]
fn drop_does_not_delete_a_copy_that_may_still_have_running_children() {
    let fixture = Fixture::new();
    let workspace = fixture.prepare();
    let path = workspace.copy_path().to_owned();
    drop(workspace);
    assert!(path.join("lib/value.dart").is_file());
}

#[test]
fn protected_input_overlap_fails_but_sibling_snapshot_in_same_registry_is_allowed() {
    let fixture = Fixture::new();
    let mut workspace = fixture.allocate();
    assert!(
        workspace
            .protect_inputs(std::slice::from_ref(&fixture.campaign.root_path))
            .is_err()
    );
    workspace.cleanup().unwrap();
    let snapshot = fixture
        .campaign
        .root_path
        .join(&fixture.campaign.campaign_name)
        .join("work/snapshot-01234567890123456789012345678901");
    fs::create_dir(&snapshot).unwrap();
    fs::set_permissions(&snapshot, crate::test_fs::permissions(0o700)).unwrap();
    let project = copy_project(
        &fixture.project.root,
        &snapshot.join("package"),
        &Cancellation::default(),
    )
    .unwrap();
    let mut workspace = fixture.allocate();
    workspace.copy(&project, &Cancellation::default()).unwrap();
    workspace.cleanup().unwrap();
}

#[test]
fn failed_copy_remains_explicitly_cleanable_and_original_binding_is_retained() {
    let fixture = Fixture::new();
    let mut workspace = fixture.allocate();
    fs::write(workspace.copy_path(), b"preexisting allocation payload").unwrap();
    assert!(
        workspace
            .copy(&fixture.project, &Cancellation::default())
            .is_err()
    );
    assert_eq!(
        fs::read(workspace.copy_path()).unwrap(),
        b"preexisting allocation payload"
    );
    workspace.verify_original().unwrap();
    fs::write(
        fixture.project.root.join("lib/value.dart"),
        b"drift after failed preparation",
    )
    .unwrap();
    assert!(workspace.verify_original().is_err());
    workspace.cleanup().unwrap();
}

struct Inject;
impl Inject {
    fn new(hook: impl FnMut(Stage) -> Result<(), NamespaceError> + 'static) -> Self {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}
impl Drop for Inject {
    fn drop(&mut self) {
        HOOK.with(|slot| slot.borrow_mut().take());
    }
}

#[test]
fn late_cancellation_and_io_failure_roll_back_the_atomic_patch() {
    for cancel_after_write in [false, true] {
        let fixture = Fixture::new();
        let mut workspace = fixture.prepare();
        let before = source_hashes(workspace.copy_path(), &Cancellation::default()).unwrap();
        let cancel = Cancellation::default();
        let notify = cancel.clone();
        let _inject = Inject::new(move |stage| {
            if stage == Stage::PatchWritten {
                if cancel_after_write {
                    notify.request(libc::SIGTERM);
                } else {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
                }
            }
            Ok(())
        });
        let error = workspace
            .apply_patch(
                &fixture.mutant(),
                &sha256_hex(b"const value = false;\n"),
                &cancel,
            )
            .unwrap_err();
        if cancel_after_write {
            assert!(matches!(error, NamespaceError::Cancelled(libc::SIGTERM)));
        } else {
            assert!(
                matches!(error, NamespaceError::Io(ref e) if e.raw_os_error() == Some(libc::EIO))
            );
        }
        assert_eq!(
            source_hashes(workspace.copy_path(), &Cancellation::default()).unwrap(),
            before
        );
        workspace.verify_original().unwrap();
        workspace.cleanup().unwrap();
    }
}

#[test]
fn rollback_never_overwrites_foreign_target_and_reports_both_failures() {
    let fixture = Fixture::new();
    let mut workspace = fixture.prepare();
    let target = workspace.copy_path().join("lib/value.dart");
    let replaced = target.clone();
    let _inject = Inject::new(move |stage| {
        if stage == Stage::PatchWritten {
            fs::remove_file(&replaced)?;
            fs::write(&replaced, b"foreign target")?;
            return Err(invalid("injected patch failure"));
        }
        Ok(())
    });
    let error = workspace
        .apply_patch(
            &fixture.mutant(),
            &sha256_hex(b"const value = false;\n"),
            &Cancellation::default(),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("rollback failed after injected patch failure"),
        "{error}"
    );
    assert_eq!(fs::read(target).unwrap(), b"foreign target");
    workspace.verify_original().unwrap();
    workspace.cleanup().unwrap();
}

#[test]
fn copy_finalization_detects_original_drift_even_when_cancellation_is_latched() {
    let fixture = Fixture::new();
    let mut workspace = fixture.allocate();
    let path = fixture.project.root.join("lib/value.dart");
    let cancel = Cancellation::default();
    let notify = cancel.clone();
    let _inject = Inject::new(move |stage| {
        if stage == Stage::Copied {
            fs::write(&path, b"external original change")?;
            notify.request(libc::SIGTERM);
        }
        Ok(())
    });
    let error = workspace.copy(&fixture.project, &cancel).unwrap_err();
    assert!(
        !matches!(error, NamespaceError::Cancelled(_)),
        "original drift must take precedence"
    );
    assert!(workspace.verify_original().is_err());
    workspace.cleanup().unwrap();
}
