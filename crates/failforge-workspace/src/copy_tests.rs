use super::*;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    dependency: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-copy-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let source = root.join("original");
        let dependency = root.join("test dependency Ж");
        fs::create_dir_all(source.join(".dart_tool")).unwrap();
        fs::create_dir_all(source.join("lib")).unwrap();
        fs::create_dir_all(source.join("test")).unwrap();
        fs::create_dir(&dependency).unwrap();
        fs::write(source.join("pubspec.yaml"), "name: subject\n").unwrap();
        fs::write(
            source.join("lib/subject.dart"),
            b"\xef\xbb\xbfbool value() => true;\r\n",
        )
        .unwrap();
        fs::write(source.join("test/subject_test.dart"), "void main() {}\n").unwrap();
        fs::write(
            source.join(".dart_tool/package_config.json"),
            serde_json::to_vec(&json!({
                "configVersion":2,"packages":[
                    {"name":"subject","rootUri":"../","packageUri":"lib/","languageVersion":"3.0"},
                    {"name":"test","rootUri":file_uri(&dependency).unwrap(),"packageUri":"lib/"}]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(source.join(".dart_tool/package_graph.json"), serde_json::to_vec(&json!({
            "roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]
        })).unwrap()).unwrap();
        Self {
            root,
            source,
            dependency,
        }
    }
    fn put(&self, relative: &str, raw: impl AsRef<[u8]>) -> PathBuf {
        let path = self.source.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, raw).unwrap();
        path
    }
    fn destination(&self) -> PathBuf {
        self.root.join("copy")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn campaign_copy_preserves_empty_directories_binary_resources_and_executable_modes() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.source.join("assets/empty/nested")).unwrap();
    let bytes: Vec<_> = (0..(64 * 1024 + 19))
        .map(|index| (index % 256) as u8)
        .collect();
    fixture.put("assets/resource.bin", &bytes);
    let executable = fixture.put("bin/tool", b"#!/bin/sh\nexit 0\n");
    fs::set_permissions(executable, fs::Permissions::from_mode(0o751)).unwrap();
    let cancel = Cancellation::default();
    let before = source_hashes(&fixture.source, &cancel).unwrap();
    let copied = copy_project(&fixture.source, &fixture.destination(), &cancel).unwrap();
    assert!(copied.root.join("assets/empty/nested").is_dir());
    assert_eq!(
        fs::read(copied.root.join("assets/resource.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        fs::metadata(copied.root.join("bin/tool")).unwrap().mode() & 0o777,
        0o751
    );
    assert_eq!(
        fs::read(copied.root.join("lib/subject.dart")).unwrap(),
        fs::read(fixture.source.join("lib/subject.dart")).unwrap()
    );
    assert_eq!(copied.packages["subject"], copied.root);
    assert_eq!(copied.packages["test"], fixture.dependency);
    assert_eq!(source_hashes(&fixture.source, &cancel).unwrap(), before);
}

#[test]
fn campaign_copy_preserves_prepared_tool_state_and_excludes_only_disposable_entries() {
    let fixture = Fixture::new();
    let kept = [
        ".dart_tool/package_config_subset",
        ".dart_tool/pub/bin/tool",
        ".dart_tool/flutter_build/dependency",
        ".dart_tool/nested/incremental_kernel.dart",
        "lib/build/source.dart",
        "lib/incremental_kernel.old",
        "incremental_kernel.dart",
    ];
    let skipped = [
        ".git/object",
        ".failforge/run/report",
        "__pycache__/module.pyc",
        "build/generated",
        "incremental_kernel.old",
        "incremental_kernel-cache/state",
        ".dart_tool/test/cache",
        ".dart_tool/nested/incremental_kernel.extra",
    ];
    for path in kept.iter().chain(&skipped) {
        fixture.put(path, path);
    }
    let copied = copy_campaign_snapshot(
        &fixture.source,
        &fixture.destination(),
        &Cancellation::default(),
    )
    .unwrap();
    for path in kept {
        assert_eq!(
            fs::read(copied.root.join(path)).unwrap(),
            path.as_bytes(),
            "{path}"
        );
    }
    for path in skipped {
        assert!(!copied.root.join(path).exists(), "{path}");
    }
}

#[test]
fn snapshot_keeps_framework_collision_and_graph_metadata() {
    let fixture = Fixture::new();
    let graph = json!({"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":["subject"]}]});
    fixture.put(
        ".dart_tool/package_graph.json",
        serde_json::to_vec(&graph).unwrap(),
    );
    let copied = copy_campaign_snapshot(
        &fixture.source,
        &fixture.destination(),
        &Cancellation::default(),
    )
    .unwrap();
    assert!(copied.framework_collision);
    assert_eq!(copied.graph, graph);
}

#[test]
fn readonly_package_configuration_can_be_rebased_without_changing_original_mode() {
    let fixture = Fixture::new();
    let original = fixture.source.join(".dart_tool/package_config.json");
    fs::set_permissions(&original, fs::Permissions::from_mode(0o444)).unwrap();
    let copied = copy_project(
        &fixture.source,
        &fixture.destination(),
        &Cancellation::default(),
    )
    .unwrap();
    for path in [original, copied.root.join(".dart_tool/package_config.json")] {
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o444);
    }
}

#[test]
fn existing_destination_file_directory_and_dangling_alias_are_untouched() {
    for kind in ["file", "directory", "alias"] {
        let fixture = Fixture::new();
        let destination = fixture.destination();
        match kind {
            "file" => fs::write(&destination, "valuable").unwrap(),
            "directory" => {
                fs::create_dir(&destination).unwrap();
                fs::write(destination.join("valuable"), "keep").unwrap();
            }
            _ => symlink(fixture.root.join("absent"), &destination).unwrap(),
        }
        assert!(copy_project(&fixture.source, &destination, &Cancellation::default()).is_err());
        match kind {
            "file" => assert_eq!(fs::read(&destination).unwrap(), b"valuable"),
            "directory" => assert_eq!(fs::read(destination.join("valuable")).unwrap(), b"keep"),
            _ => assert!(
                fs::symlink_metadata(&destination)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            ),
        }
    }
}

#[test]
fn overlapping_and_nonprivate_destinations_are_rejected_before_creation() {
    let fixture = Fixture::new();
    for destination in [
        fixture.source.clone(),
        fixture.source.join("nested"),
        fixture.root.clone(),
    ] {
        assert!(copy_project(&fixture.source, &destination, &Cancellation::default()).is_err());
    }
    assert!(!fixture.source.join("nested").exists());
    let alias = fixture.root.join("source-alias");
    symlink(&fixture.source, &alias).unwrap();
    assert!(
        copy_project(
            &fixture.source,
            &alias.join("nested"),
            &Cancellation::default()
        )
        .is_err()
    );
    fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        copy_project(
            &fixture.source,
            &fixture.destination(),
            &Cancellation::default()
        )
        .is_err()
    );
    assert!(!fixture.destination().exists());
}

#[test]
fn retained_tool_aliases_and_fifos_cannot_escape_or_block_snapshotting() {
    for kind in ["alias", "fifo"] {
        let fixture = Fixture::new();
        let special = fixture.source.join(".dart_tool/retained");
        if kind == "alias" {
            symlink(&fixture.dependency, &special).unwrap();
        } else {
            let path = c_name(special.as_os_str()).unwrap();
            // SAFETY: path is terminated and points into this private fixture.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let start = std::time::Instant::now();
        assert!(
            copy_project(
                &fixture.source,
                &fixture.destination(),
                &Cancellation::default()
            )
            .is_err()
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        assert!(!fixture.destination().exists());
    }
}

#[test]
fn transient_source_and_metadata_changes_are_rejected_even_when_restored() {
    for relative in [
        "lib/subject.dart",
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
        ".dart_tool/retained",
    ] {
        let fixture = Fixture::new();
        if relative.ends_with("retained") {
            fixture.put(relative, b"prepared");
        }
        let path = fixture.source.join(relative);
        let original = fs::read(&path).unwrap();
        let result = copy_with(
            &fixture.source,
            &fixture.destination(),
            &Cancellation::default(),
            |stage, _, _| match stage {
                Stage::Inventoried => {
                    let mut changed = original.clone();
                    changed.push(b' ');
                    fs::write(&path, changed).unwrap();
                }
                Stage::Copied => fs::write(&path, &original).unwrap(),
                Stage::Inspected | Stage::Verified => (),
            },
        );
        assert!(result.is_err(), "{relative}");
        assert!(!fixture.destination().exists());
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

#[test]
fn copied_inventory_corruption_and_new_empty_directories_are_rejected_and_cleaned() {
    for action in ["change", "remove", "add", "alias", "empty-directory"] {
        let fixture = Fixture::new();
        let result = copy_with(
            &fixture.source,
            &fixture.destination(),
            &Cancellation::default(),
            |stage, _, destination| {
                if !matches!(stage, Stage::Copied) {
                    return;
                }
                let path = destination.join("lib/subject.dart");
                match action {
                    "change" => fs::write(path, "changed").unwrap(),
                    "remove" => fs::remove_file(path).unwrap(),
                    "add" => fs::write(destination.join("unexpected"), "new").unwrap(),
                    "alias" => {
                        fs::remove_file(&path).unwrap();
                        symlink(fixture.source.join("lib/subject.dart"), path).unwrap();
                    }
                    _ => fs::create_dir(destination.join("unexpected-empty")).unwrap(),
                }
            },
        );
        assert!(result.is_err(), "{action}");
        assert!(!fixture.destination().exists(), "{action}");
    }
}

#[test]
fn changes_after_initial_inventory_verification_are_not_hidden_by_rebase() {
    for copied in [false, true] {
        let fixture = Fixture::new();
        let result = copy_with(
            &fixture.source,
            &fixture.destination(),
            &Cancellation::default(),
            |stage, source, destination| {
                if matches!(stage, Stage::Verified) {
                    fs::write(
                        if copied { destination } else { source }.join("lib/subject.dart"),
                        "changed late",
                    )
                    .unwrap();
                }
            },
        );
        assert!(result.is_err());
        assert!(!fixture.destination().exists());
    }
}

#[test]
fn cancelled_copy_removes_its_partial_destination_without_unlinking_source() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    let result = copy_with(
        &fixture.source,
        &fixture.destination(),
        &cancel,
        |stage, _, _| {
            if matches!(stage, Stage::Copied) {
                cancel.request(libc::SIGTERM);
            }
        },
    );
    assert!(matches!(result, Err(InputError::Cancelled(libc::SIGTERM))));
    assert!(!fixture.destination().exists());
    assert!(fixture.source.join("lib/subject.dart").exists());
    assert!(copy_project(&fixture.source, &fixture.destination(), &cancel).is_err());
    assert!(!fixture.destination().exists());
}

#[test]
fn replaced_destination_is_never_deleted_using_the_old_copy_authority() {
    let fixture = Fixture::new();
    let moved = fixture.root.join("original-copy-moved");
    let result = copy_with(
        &fixture.source,
        &fixture.destination(),
        &Cancellation::default(),
        |stage, _, destination| {
            if matches!(stage, Stage::Copied) {
                fs::rename(destination, &moved).unwrap();
                fs::create_dir(destination).unwrap();
                fs::write(destination.join("valuable"), "keep").unwrap();
            }
        },
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read(fixture.destination().join("valuable")).unwrap(),
        b"keep"
    );
    assert!(moved.join("lib/subject.dart").exists());
}

#[test]
fn cancellation_before_copy_creates_no_destination() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    cancel.request(libc::SIGINT);
    assert!(matches!(
        copy_campaign_snapshot(&fixture.source, &fixture.destination(), &cancel),
        Err(InputError::Cancelled(libc::SIGINT))
    ));
    assert!(!fixture.destination().exists());
}

#[test]
fn copied_hard_link_cannot_rebase_or_chmod_original_metadata() {
    for phase in [Stage::Copied, Stage::Verified] {
        let fixture = Fixture::new();
        let original = fixture.source.join(".dart_tool/package_config.json");
        let original_bytes = fs::read(&original).unwrap();
        fs::set_permissions(&original, fs::Permissions::from_mode(0o444)).unwrap();
        let result = copy_with(
            &fixture.source,
            &fixture.destination(),
            &Cancellation::default(),
            |stage, _, destination| {
                if std::mem::discriminant(&stage) == std::mem::discriminant(&phase) {
                    let target = destination.join(".dart_tool/package_config.json");
                    fs::remove_file(&target).unwrap();
                    fs::hard_link(&original, target).unwrap();
                }
            },
        );
        assert!(result.is_err());
        assert!(!fixture.destination().exists());
        assert_eq!(fs::read(&original).unwrap(), original_bytes);
        assert_eq!(fs::metadata(original).unwrap().mode() & 0o777, 0o444);
    }
}

#[test]
fn source_hard_links_are_copied_as_independent_files() {
    let fixture = Fixture::new();
    fs::hard_link(
        fixture.source.join("lib/subject.dart"),
        fixture.source.join("lib/other.dart"),
    )
    .unwrap();
    let result = copy_project(
        &fixture.source,
        &fixture.destination(),
        &Cancellation::default(),
    )
    .unwrap();
    let first = fs::metadata(result.root.join("lib/subject.dart")).unwrap();
    let second = fs::metadata(result.root.join("lib/other.dart")).unwrap();
    assert_ne!(first.ino(), second.ino());
    assert_eq!(first.nlink(), 1);
    assert_eq!(second.nlink(), 1);
}

#[test]
fn metadata_changes_between_inspection_and_inventory_cannot_rebase_stale_dependencies() {
    for relative in [
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ] {
        let fixture = Fixture::new();
        let replacement = fixture.root.join("replacement-test-package");
        fs::create_dir(&replacement).unwrap();
        let path = fixture.source.join(relative);
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if relative.ends_with("package_config.json") {
            metadata["packages"][1]["rootUri"] = json!(file_uri(&replacement).unwrap());
        } else {
            metadata["packages"][1]["dependencies"] = json!(["subject"]);
        }
        let changed = serde_json::to_vec(&metadata).unwrap();
        let result = copy_with(
            &fixture.source,
            &fixture.destination(),
            &Cancellation::default(),
            |stage, _, _| {
                if matches!(stage, Stage::Inspected) {
                    fs::write(&path, &changed).unwrap();
                }
            },
        );
        assert!(result.is_err(), "{relative}");
        assert!(!fixture.destination().exists());
        assert_eq!(fs::read(path).unwrap(), changed);
    }
}

#[test]
fn file_and_directory_modification_times_survive_copy_and_metadata_rebase() {
    let fixture = Fixture::new();
    fixture.put("assets/resource.bin", [0, 255, 17]);
    fs::create_dir(fixture.source.join("assets/empty")).unwrap();
    let paths = [
        "",
        "lib",
        "lib/subject.dart",
        "assets",
        "assets/empty",
        "assets/resource.bin",
        ".dart_tool",
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ];
    let mut expected = BTreeMap::new();
    for (index, relative) in paths.iter().enumerate() {
        let path = fixture.source.join(relative);
        let modified = std::time::UNIX_EPOCH
            + std::time::Duration::new(1_000_000_000 + index as u64, 123_456_789);
        File::open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        expected.insert(*relative, fs::metadata(path).unwrap().modified().unwrap());
    }
    let copied = copy_project(
        &fixture.source,
        &fixture.destination(),
        &Cancellation::default(),
    )
    .unwrap();
    for (relative, modified) in expected {
        assert_eq!(
            fs::metadata(fixture.source.join(relative))
                .unwrap()
                .modified()
                .unwrap(),
            modified,
            "source {relative}"
        );
        let actual = fs::metadata(copied.root.join(relative))
            .unwrap()
            .modified()
            .unwrap();
        if relative == ".dart_tool/package_config.json" {
            assert_ne!(
                actual, modified,
                "config rebase intentionally writes fresh bytes"
            );
        } else {
            assert_eq!(actual, modified, "copied {relative}");
        }
    }
}

#[test]
fn readonly_directory_modes_are_preserved_and_owned_cleanup_removes_them() {
    for cancelled in [false, true] {
        let fixture = Fixture::new();
        fixture.put("readonly/nested/resource", b"valuable resource");
        let paths = ["", "readonly", "readonly/nested"];
        for relative in paths {
            fs::set_permissions(
                fixture.source.join(relative),
                fs::Permissions::from_mode(0o500),
            )
            .unwrap();
        }
        let cancel = Cancellation::default();
        let result = copy_with(
            &fixture.source,
            &fixture.destination(),
            &cancel,
            |stage, _, _| {
                if cancelled && matches!(stage, Stage::Copied) {
                    cancel.request(libc::SIGINT);
                }
            },
        );
        if cancelled {
            assert!(matches!(result, Err(InputError::Cancelled(libc::SIGINT))));
            assert!(!fixture.destination().exists());
        } else {
            let copied = result.unwrap();
            for relative in paths {
                assert_eq!(
                    fs::metadata(copied.root.join(relative)).unwrap().mode() & 0o777,
                    0o500,
                    "copy {relative}"
                );
            }
            let directory = open_directory_at(libc::AT_FDCWD, copied.root.as_os_str()).unwrap();
            remove_contents(&directory, 0).unwrap();
            fs::remove_dir(copied.root).unwrap();
        }
        for relative in paths {
            let path = fixture.source.join(relative);
            assert_eq!(
                fs::metadata(&path).unwrap().mode() & 0o777,
                0o500,
                "source {relative}"
            );
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert_eq!(
            fs::read(fixture.source.join("readonly/nested/resource")).unwrap(),
            b"valuable resource"
        );
    }
}
