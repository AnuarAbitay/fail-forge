use super::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::sync::{Arc, Barrier};

const DOCUMENT: &str = "rules:\n  include: [lib/данные.dart]\n";
const PREVIOUS: &str = "previous complete profile\n";

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    output: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-publication-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let project = root.join("bucket/package");
        fs::create_dir_all(project.join("lib")).unwrap();
        fs::write(project.join("pubspec.yaml"), "name: sample\n").unwrap();
        let output = project.join("profiles/generated/profile.yml");
        Self {
            root,
            project,
            output,
        }
    }

    fn parents(&self) {
        fs::create_dir_all(self.output.parent().unwrap()).unwrap();
    }

    fn previous(&self) {
        self.parents();
        fs::write(&self.output, PREVIOUS).unwrap();
    }

    fn binding(&self) -> OutputBinding {
        capture_output_binding(&self.project, &self.output).unwrap()
    }

    fn publish(&self, force: bool, binding: Option<&OutputBinding>) -> Result<()> {
        write_profile(&self.project, &self.output, DOCUMENT, force, binding)
    }

    fn hooked(
        &self,
        force: bool,
        binding: Option<&OutputBinding>,
        hook: &mut Hook<'_>,
    ) -> Result<()> {
        write_profile_with_hook(&self.project, &self.output, DOCUMENT, force, binding, hook)
    }

    fn staging(&self) -> Vec<PathBuf> {
        fn collect(path: &Path, found: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                if name.as_bytes().starts_with(b".failforge-profile-") {
                    found.push(entry.path());
                }
                if entry.file_type().unwrap().is_dir() {
                    collect(&entry.path(), found);
                }
            }
        }
        let mut found = Vec::new();
        collect(&self.root, &mut found);
        found
    }

    fn clean(&self) {
        assert_eq!(self.staging(), Vec::<PathBuf>::new());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn injected() -> ProfileError {
    std::io::Error::other("injected publication failure").into()
}

#[test]
fn publication_is_whole_synced_utf8_and_leaves_no_staging() {
    let fixture = Fixture::new();
    let mut synced = false;
    let mut committed = false;
    fixture
        .hooked(false, None, &mut |event| {
            if event.stage == Stage::AfterSync {
                synced = true;
            }
            if event.stage == Stage::BeforeCommit {
                assert!(synced);
                assert!(!fixture.output.exists());
                assert_eq!(
                    fs::read(
                        fixture
                            .output
                            .parent()
                            .unwrap()
                            .join(event.temporary.unwrap())
                    )
                    .unwrap(),
                    DOCUMENT.as_bytes()
                );
            }
            if event.stage == Stage::AfterCommit {
                assert_eq!(fs::read_to_string(&fixture.output).unwrap(), DOCUMENT);
                committed = true;
            }
            Ok(())
        })
        .unwrap();
    assert!(committed);
    assert!(fixture.output.symlink_metadata().unwrap().is_file());
    fixture.clean();
}

#[test]
fn force_preserves_old_bytes_until_atomic_commit_and_private_mode() {
    let fixture = Fixture::new();
    fixture.previous();
    fs::set_permissions(&fixture.output, fs::Permissions::from_mode(0o600)).unwrap();
    let previous_inode = fixture.output.metadata().unwrap().ino();
    fixture
        .hooked(true, None, &mut |event| {
            if event.stage == Stage::BeforeCommit {
                assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
                assert_eq!(
                    fs::read_to_string(
                        fixture.output.parent().unwrap().join(event.backup.unwrap())
                    )
                    .unwrap(),
                    PREVIOUS
                );
            }
            Ok(())
        })
        .unwrap();
    let published = fixture.output.metadata().unwrap();
    assert_ne!(previous_inode, published.ino());
    assert_eq!(published.mode() & 0o7777, 0o600);
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), DOCUMENT);
    fixture.clean();
}

#[test]
fn destination_check_never_creates_parents_and_requires_force() {
    let fixture = Fixture::new();
    check_destination(&fixture.output, false).unwrap();
    check_destination(&fixture.output, true).unwrap();
    assert!(!fixture.project.join("profiles").exists());
    fixture.previous();
    assert!(check_destination(&fixture.output, false).is_err());
    assert!(fixture.publish(false, None).is_err());
    check_destination(&fixture.output, true).unwrap();
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
    fixture.clean();
}

#[test]
fn symlink_dangling_symlink_directory_and_fifo_destinations_are_rejected() {
    for kind in ["symlink", "dangling", "directory", "fifo"] {
        let fixture = Fixture::new();
        fixture.parents();
        let outside = fixture.root.join("outside.yml");
        fs::write(&outside, "outside sentinel").unwrap();
        match kind {
            "symlink" => symlink(&outside, &fixture.output).unwrap(),
            "dangling" => symlink(fixture.root.join("missing"), &fixture.output).unwrap(),
            "directory" => fs::create_dir(&fixture.output).unwrap(),
            "fifo" => {
                let path = name(fixture.output.as_os_str()).unwrap();
                // SAFETY: the fixture owns this terminated new path.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        for force in [false, true] {
            assert!(check_destination(&fixture.output, force).is_err(), "{kind}");
            assert!(fixture.publish(force, None).is_err(), "{kind}");
        }
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside sentinel");
        fixture.clean();
    }
}

#[test]
fn concurrent_exclusive_creators_publish_exactly_one_complete_document() {
    let fixture = Fixture::new();
    fixture.parents();
    let binding = fixture.binding();
    let barrier = Arc::new(Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = ["first complete document\n", "second complete document\n"]
            .into_iter()
            .map(|text| {
                let barrier = Arc::clone(&barrier);
                let fixture = &fixture;
                let binding = &binding;
                scope.spawn(move || {
                    write_profile_with_hook(
                        &fixture.project,
                        &fixture.output,
                        text,
                        false,
                        Some(binding),
                        &mut |event| {
                            if event.stage == Stage::BeforeCommit {
                                barrier.wait();
                            }
                            Ok(())
                        },
                    )
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        ["first complete document\n", "second complete document\n"]
            .contains(&fs::read_to_string(&fixture.output).unwrap().as_str())
    );
    fixture.clean();
}

#[test]
fn late_exclusive_destination_and_symlink_win_without_being_overwritten() {
    for is_symlink in [false, true] {
        let fixture = Fixture::new();
        let outside = fixture.root.join("outside.yml");
        fs::write(&outside, "outside sentinel").unwrap();
        let result = fixture.hooked(false, None, &mut |event| {
            if event.stage == Stage::BeforeCommit {
                if is_symlink {
                    symlink(&outside, &fixture.output).unwrap();
                } else {
                    fs::write(&fixture.output, "concurrent winner").unwrap();
                }
            }
            Ok(())
        });
        assert!(result.is_err());
        if is_symlink {
            assert!(fixture.output.is_symlink());
        } else {
            assert_eq!(
                fs::read_to_string(&fixture.output).unwrap(),
                "concurrent winner"
            );
        }
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside sentinel");
        fixture.clean();
    }
}

#[test]
fn static_parent_and_root_aliases_are_not_followed() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, fixture.project.join("profiles")).unwrap();
    assert!(fixture.publish(false, None).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_file(fixture.project.join("profiles")).unwrap();
    let alias = fixture.root.join("alias");
    symlink(&fixture.project, &alias).unwrap();
    assert!(write_profile(&alias, &alias.join("file.yml"), DOCUMENT, false, None).is_err());
    fixture.clean();
}

#[test]
fn replaced_bound_ancestor_or_package_root_is_rejected_before_writes() {
    for replace_ancestor in [false, true] {
        let fixture = Fixture::new();
        let binding = fixture.binding();
        let moved = fixture.root.join("retained");
        let selected = if replace_ancestor {
            fixture.project.parent().unwrap()
        } else {
            &fixture.project
        };
        fs::rename(selected, &moved).unwrap();
        fs::create_dir_all(&fixture.project).unwrap();
        assert!(fixture.publish(false, Some(&binding)).is_err());
        assert_eq!(fs::read_dir(&fixture.project).unwrap().count(), 0);
        fixture.clean();
    }
}

#[test]
fn replaced_bound_parent_is_rejected() {
    let fixture = Fixture::new();
    fixture.parents();
    let binding = fixture.binding();
    let parent = fixture.output.parent().unwrap();
    fs::rename(parent, fixture.root.join("retained")).unwrap();
    fs::create_dir(parent).unwrap();
    assert!(fixture.publish(false, Some(&binding)).is_err());
    assert_eq!(fs::read_dir(parent).unwrap().count(), 0);
    fixture.clean();
}

#[test]
fn directory_replaced_between_mkdir_and_open_is_not_followed_or_deleted() {
    let fixture = Fixture::new();
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    let mut attacked = false;
    let result = fixture.hooked(false, None, &mut |event| {
        if event.stage == Stage::AfterMkdir && !attacked {
            let parent = fixture.project.join("profiles");
            fs::remove_dir(&parent).unwrap();
            symlink(&outside, &parent).unwrap();
            attacked = true;
        }
        Ok(())
    });
    assert!(result.is_err());
    assert!(attacked);
    assert!(fixture.project.join("profiles").is_symlink());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fixture.clean();
}

#[test]
fn relocated_open_parent_is_rejected_and_staging_is_cleaned_through_handle() {
    let fixture = Fixture::new();
    fixture.parents();
    let moved = fixture.root.join("relocated");
    let replacement = fixture.root.join("replacement");
    fs::create_dir(&replacement).unwrap();
    let result = fixture.hooked(false, None, &mut |event| {
        if event.stage == Stage::BeforeStageOpen {
            fs::rename(fixture.output.parent().unwrap(), &moved).unwrap();
            symlink(&replacement, fixture.output.parent().unwrap()).unwrap();
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&replacement).unwrap().count(), 0);
    fixture.clean();
}

#[test]
fn changed_root_pubspec_and_new_nested_pubspec_invalidate_binding() {
    for nested in [false, true] {
        let fixture = Fixture::new();
        let binding = fixture.binding();
        if nested {
            fixture.parents();
            fs::write(
                fixture.output.parent().unwrap().join("pubspec.yaml"),
                "name: nested\n",
            )
            .unwrap();
        } else {
            fs::write(fixture.project.join("pubspec.yaml"), "name: changed\n").unwrap();
        }
        assert!(fixture.publish(false, Some(&binding)).is_err());
        assert!(!fixture.output.exists());
        if !nested {
            assert!(!fixture.project.join("profiles").exists());
        }
        fixture.clean();
    }
}

#[test]
fn pubspec_symlinks_and_nested_packages_are_rejected_during_capture() {
    let fixture = Fixture::new();
    fixture.parents();
    let nested = fixture.output.parent().unwrap().join("pubspec.yaml");
    symlink(fixture.project.join("pubspec.yaml"), &nested).unwrap();
    assert!(capture_output_binding(&fixture.project, &fixture.output).is_err());
    fs::remove_file(nested).unwrap();
    fs::remove_file(fixture.project.join("pubspec.yaml")).unwrap();
    symlink(
        fixture.root.join("missing"),
        fixture.project.join("pubspec.yaml"),
    )
    .unwrap();
    assert!(capture_output_binding(&fixture.project, &fixture.output).is_err());
}

#[test]
fn staged_inode_or_content_replacement_is_never_published() {
    for replacement in ["symlink", "regular", "in-place"] {
        for force in [false, true] {
            let fixture = Fixture::new();
            if force {
                fixture.previous();
            }
            let outside = fixture.root.join("outside");
            fs::write(&outside, "outside sentinel").unwrap();
            let mut replaced_path = None;
            let result = fixture.hooked(force, None, &mut |event| {
                if event.stage == Stage::AfterSync {
                    let temporary = fixture
                        .output
                        .parent()
                        .unwrap()
                        .join(event.temporary.unwrap());
                    if replacement == "in-place" {
                        fs::write(&temporary, vec![b'X'; DOCUMENT.len()]).unwrap();
                    } else {
                        fs::remove_file(&temporary).unwrap();
                        if replacement == "symlink" {
                            symlink(&outside, &temporary).unwrap();
                        } else {
                            fs::write(&temporary, "unvalidated replacement").unwrap();
                        }
                        replaced_path = Some(temporary);
                    }
                }
                Ok(())
            });
            assert!(result.is_err(), "{replacement} force={force}");
            if force {
                assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
            } else {
                assert!(!fixture.output.exists());
            }
            assert_eq!(fs::read_to_string(outside).unwrap(), "outside sentinel");
            if let Some(path) = replaced_path {
                assert!(
                    path.symlink_metadata().is_ok(),
                    "foreign staging must not be removed"
                );
                fs::remove_file(path).unwrap();
            }
            fixture.clean();
        }
    }
}

#[test]
fn late_force_destination_replacements_are_preserved() {
    for is_symlink in [false, true] {
        let fixture = Fixture::new();
        fixture.previous();
        let outside = fixture.root.join("outside");
        fs::write(&outside, "outside sentinel").unwrap();
        let result = fixture.hooked(true, None, &mut |event| {
            if event.stage == Stage::AfterSync {
                fs::remove_file(&fixture.output).unwrap();
                if is_symlink {
                    symlink(&outside, &fixture.output).unwrap();
                } else {
                    fs::write(&fixture.output, "concurrent winner").unwrap();
                }
            }
            Ok(())
        });
        assert!(result.is_err());
        if is_symlink {
            assert!(fixture.output.is_symlink());
        } else {
            assert_eq!(
                fs::read_to_string(&fixture.output).unwrap(),
                "concurrent winner"
            );
        }
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside sentinel");
        fixture.clean();
    }
}

#[test]
fn move_after_commit_rolls_back_new_file_or_restores_previous_document() {
    for force in [false, true] {
        let fixture = Fixture::new();
        fixture.parents();
        if force {
            fixture.previous();
        }
        let moved = fixture.root.join("relocated");
        let result = fixture.hooked(force, None, &mut |event| {
            if event.stage == Stage::AfterCommit {
                fs::rename(fixture.output.parent().unwrap(), &moved).unwrap();
            }
            Ok(())
        });
        assert!(result.is_err());
        if force {
            assert_eq!(
                fs::read_to_string(moved.join("profile.yml")).unwrap(),
                PREVIOUS
            );
        } else {
            assert!(!moved.join("profile.yml").exists());
        }
        fixture.clean();
    }
}

#[test]
fn failure_at_each_publication_boundary_restores_previous_or_removes_new_directories() {
    for stage in [
        Stage::BeforeMkdir,
        Stage::AfterMkdir,
        Stage::BeforeStageOpen,
        Stage::StagingOpened,
        Stage::BeforeWrite,
        Stage::AfterWrite,
        Stage::BeforeSync,
        Stage::AfterSync,
        Stage::AfterStageClose,
        Stage::BeforeCommit,
        Stage::AfterCommit,
        Stage::BeforeStageCleanup,
        Stage::BeforeFinalVerify,
    ] {
        for force in [false, true] {
            let fixture = Fixture::new();
            if force {
                fixture.previous();
            }
            let mut failed = false;
            let result = fixture.hooked(force, None, &mut |event| {
                if event.stage == stage && !failed {
                    failed = true;
                    return Err(injected());
                }
                Ok(())
            });
            assert!(failed && result.is_err(), "{stage:?}, force={force}");
            if force {
                assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
            } else {
                assert!(!fixture.project.join("profiles").exists(), "{stage:?}");
            }
            fixture.clean();
        }
    }
}

#[test]
fn backup_cleanup_failure_restores_the_previous_document() {
    let fixture = Fixture::new();
    fixture.previous();
    let result = fixture.hooked(true, None, &mut |event| {
        if event.stage == Stage::BeforeBackupCleanup {
            return Err(injected());
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
    fixture.clean();
}

#[test]
fn rollback_failure_retains_only_recoverable_previous_backup() {
    let fixture = Fixture::new();
    fixture.previous();
    let result = fixture.hooked(true, None, &mut |event| {
        if matches!(event.stage, Stage::AfterCommit | Stage::BeforeRollback) {
            return Err(injected());
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), DOCUMENT);
    let staging = fixture.staging();
    assert_eq!(staging.len(), 1);
    assert!(
        staging[0]
            .file_name()
            .unwrap()
            .as_bytes()
            .starts_with(b".failforge-profile-backup-")
    );
    assert_eq!(fs::read_to_string(&staging[0]).unwrap(), PREVIOUS);
}

#[test]
fn missing_backup_before_commit_preserves_previous_destination() {
    let fixture = Fixture::new();
    fixture.previous();
    let result = fixture.hooked(true, None, &mut |event| {
        if event.stage == Stage::AfterBackupLink {
            fs::remove_file(fixture.output.parent().unwrap().join(event.backup.unwrap())).unwrap();
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
    fixture.clean();
}

#[test]
fn concurrent_replacement_after_force_commit_preserves_winner_and_old_backup() {
    let fixture = Fixture::new();
    fixture.previous();
    let winner = fixture.root.join("winner.yml");
    fs::write(&winner, "concurrent winner").unwrap();
    let result = fixture.hooked(true, None, &mut |event| {
        if event.stage == Stage::AfterCommit {
            fs::rename(&winner, &fixture.output).unwrap();
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(&fixture.output).unwrap(),
        "concurrent winner"
    );
    let staging = fixture.staging();
    assert_eq!(staging.len(), 1);
    assert_eq!(fs::read_to_string(&staging[0]).unwrap(), PREVIOUS);
}

#[test]
fn in_place_destination_edits_with_restored_mtime_are_detected_before_and_during_backup() {
    for stage in [Stage::AfterSync, Stage::AfterBackupLink] {
        let fixture = Fixture::new();
        fixture.previous();
        let metadata = fixture.output.metadata().unwrap();
        let concurrent = "X".repeat(PREVIOUS.len());
        let result = fixture.hooked(true, None, &mut |event| {
            if event.stage == stage {
                fs::write(&fixture.output, &concurrent).unwrap();
                File::options()
                    .write(true)
                    .open(&fixture.output)
                    .unwrap()
                    .set_modified(metadata.modified().unwrap())
                    .unwrap();
                let after = fixture.output.metadata().unwrap();
                assert_eq!(after.ino(), metadata.ino());
                assert_eq!(after.len(), metadata.len());
                assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&fixture.output).unwrap(), concurrent);
        fixture.clean();
    }
}

#[test]
fn staging_name_collision_preserves_foreign_file() {
    let fixture = Fixture::new();
    let mut collision = None;
    let result = fixture.hooked(false, None, &mut |event| {
        if event.stage == Stage::BeforeStageOpen {
            let path = fixture
                .output
                .parent()
                .unwrap()
                .join(event.temporary.unwrap());
            fs::write(&path, "foreign temporary").unwrap();
            collision = Some(path);
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(collision.unwrap()).unwrap(),
        "foreign temporary"
    );
    assert!(!fixture.output.exists());
}

#[test]
fn cancelled_publication_cleans_staging_and_new_empty_parents() {
    let fixture = Fixture::new();
    let result = fixture.hooked(false, None, &mut |event| {
        if event.stage == Stage::AfterSync {
            return Err(ProfileError::Cancelled(130));
        }
        Ok(())
    });
    assert!(matches!(result, Err(ProfileError::Cancelled(130))));
    assert!(!fixture.project.join("profiles").exists());
    fixture.clean();
}

#[test]
fn mismatched_binding_outside_and_parent_traversal_paths_never_write() {
    let fixture = Fixture::new();
    let other = fixture.project.join("other.yml");
    let binding = capture_output_binding(&fixture.project, &other).unwrap();
    assert!(fixture.publish(false, Some(&binding)).is_err());
    for output in [
        &fixture.project,
        &fixture.root.join("outside.yml"),
        &fixture.project.join("profiles/../file.yml"),
    ] {
        assert!(write_profile(&fixture.project, output, DOCUMENT, false, None).is_err());
    }
    assert!(!fixture.project.join("profiles").exists());
    fixture.clean();
}

#[test]
fn search_only_ancestor_permissions_need_no_listing() {
    let fixture = Fixture::new();
    let ancestor = fixture.project.parent().unwrap();
    fs::set_permissions(ancestor, fs::Permissions::from_mode(0o111)).unwrap();
    let result = capture_output_binding(&fixture.project, &fixture.output)
        .and_then(|binding| fixture.publish(false, Some(&binding)));
    fs::set_permissions(ancestor, fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), DOCUMENT);
    fixture.clean();
}

#[test]
fn nested_pubspec_introduced_after_commit_forces_rollback() {
    let fixture = Fixture::new();
    fixture.previous();
    let result = fixture.hooked(true, None, &mut |event| {
        if event.stage == Stage::AfterCommit {
            fs::write(
                fixture.output.parent().unwrap().join("pubspec.yaml"),
                "name: nested\n",
            )
            .unwrap();
        }
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(fs::read_to_string(&fixture.output).unwrap(), PREVIOUS);
    fixture.clean();
}

#[test]
fn created_directory_replacement_is_preserved_on_rollback() {
    let fixture = Fixture::new();
    let moved = fixture.root.join("moved");
    let mut replaced = false;
    let result = fixture.hooked(false, None, &mut |event| {
        if event.stage == Stage::AfterMkdir && !replaced {
            let parent = fixture.project.join("profiles");
            fs::rename(&parent, &moved).unwrap();
            fs::create_dir(&parent).unwrap();
            replaced = true;
            return Err(injected());
        }
        Ok(())
    });
    assert!(result.is_err());
    assert!(fixture.project.join("profiles").is_dir());
    assert!(moved.is_dir());
}

#[test]
fn direct_package_output_without_parent_creation_is_supported() {
    let fixture = Fixture::new();
    let output = fixture.project.join("profile.yml");
    write_profile(&fixture.project, &output, DOCUMENT, false, None).unwrap();
    assert_eq!(fs::read_to_string(output).unwrap(), DOCUMENT);
    fixture.clean();
}
