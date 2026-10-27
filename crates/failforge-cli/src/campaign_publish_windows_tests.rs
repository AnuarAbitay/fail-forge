use super::*;
use serde_json::json;
const RUN: &str = "0123456789abcdef0123456789abcdef";
struct Fixture {
    path: PathBuf,
    root: File,
    profile: bool,
    families: Vec<String>,
}
impl Fixture {
    fn new(profile: bool) -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-publish-{}", token().unwrap()));
        std::fs::create_dir(&path).unwrap();
        let root = files::walk(&path).unwrap();
        let families: Vec<String> = if profile {
            vec!["classic".into(), "failure_mode".into()]
        } else {
            vec!["classic".into()]
        };
        if profile {
            files::create_directory(&root, ".profile-reports").unwrap();
            let family_root = files::create_directory(&root, "families").unwrap();
            for family in &families {
                let parent = files::create_directory(&family_root, family).unwrap();
                let mut file = files::create_file(&parent, "summary.json").unwrap();
                file.write_all(b"initial family\n").unwrap();
                file.sync_all().unwrap();
            }
        }
        let mut file = files::create_file(&root, "summary.json").unwrap();
        file.write_all(b"initial root\n").unwrap();
        file.sync_all().unwrap();
        Self {
            path,
            root,
            profile,
            families,
        }
    }
    fn publisher(&self) -> Publisher {
        Publisher::new(
            &self.path,
            self.root.try_clone().unwrap(),
            self.profile,
            &self.families,
        )
        .unwrap()
    }
    fn summary(&self) -> Value {
        let mut value = json!({"complete":false,"runId":RUN,"output":self.path,"status":"failed"});
        if self.profile {
            value["kind"] = "profile".into();
            for family in &self.families {
                value["families"][family]["summary"] =
                    json!({"runId":RUN,"family":family,"complete":false,"status":"failed"});
            }
        }
        value
    }
    fn assert_initial(&self) {
        assert_eq!(
            std::fs::read(self.path.join("summary.json")).unwrap(),
            b"initial root\n"
        );
        if self.profile {
            for family in &self.families {
                assert_eq!(
                    std::fs::read(self.path.join("families").join(family).join("summary.json"))
                        .unwrap(),
                    b"initial family\n"
                );
            }
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
#[test]
fn direct_summary_is_atomic_and_profile_mirrors_have_identical_generation_bytes() {
    for profile in [false, true] {
        let fixture = Fixture::new(profile);
        let summary = fixture.summary();
        fixture.publisher().publish(&summary, None).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(
                &std::fs::read(fixture.path.join("summary.json")).unwrap()
            )
            .unwrap(),
            summary
        );
        if profile {
            for family in &fixture.families {
                let bytes = std::fs::read(
                    fixture
                        .path
                        .join("families")
                        .join(family)
                        .join("summary.json"),
                )
                .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap(),
                    summary["families"][family]["summary"]
                );
                assert!(
                    !std::fs::symlink_metadata(
                        fixture
                            .path
                            .join("families")
                            .join(family)
                            .join("summary.json")
                    )
                    .unwrap()
                    .file_type()
                    .is_symlink()
                );
            }
        }
    }
}
#[test]
fn failures_before_root_commit_restore_all_regular_family_mirrors() {
    for step in [
        "create:/summary.json",
        "write:/summary.json",
        "commit:families/failure_mode",
        "commit",
    ] {
        let fixture = Fixture::new(true);
        assert!(
            fixture
                .publisher()
                .publish_with_hook(
                    &fixture.summary(),
                    None,
                    &mut |current| if current == step {
                        Err(io::Error::other("injected"))
                    } else {
                        Ok(())
                    }
                )
                .is_err()
        );
        fixture.assert_initial();
    }
}
#[test]
fn failed_actual_root_rename_also_rolls_back_family_mirrors() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let fixture = Fixture::new(true);
    let publisher = fixture.publisher();
    let _deny_delete = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(fixture.path.join("summary.json"))
        .unwrap();
    assert!(publisher.publish(&fixture.summary(), None).is_err());
    fixture.assert_initial();
}
#[test]
fn changed_root_and_concurrent_publisher_cannot_commit() {
    let fixture = Fixture::new(false);
    let publisher = fixture.publisher();
    let lock = publisher.lock().unwrap();
    assert!(publisher.publish(&fixture.summary(), None).is_err());
    fixture.assert_initial();
    drop(lock);
    let moved = fixture.path.with_extension("moved");
    std::fs::rename(&fixture.path, &moved).unwrap();
    std::fs::create_dir(&fixture.path).unwrap();
    std::fs::write(fixture.path.join("foreign"), "keep").unwrap();
    assert!(publisher.publish(&fixture.summary(), None).is_err());
    assert_eq!(
        std::fs::read(fixture.path.join("foreign")).unwrap(),
        b"keep"
    );
    drop(publisher);
    let _ = std::fs::remove_dir_all(moved);
}
#[test]
fn staged_substitution_is_rejected_before_authoritative_commit() {
    let fixture = Fixture::new(true);
    let publisher = fixture.publisher();
    assert!(
        publisher
            .publish_with_hook(&fixture.summary(), None, &mut |step| {
                if step == "commit" {
                    let staged = std::fs::read_dir(&fixture.path)?
                        .map(|entry| entry.unwrap().path())
                        .find(|path| {
                            path.file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(".summary-")
                                && !path
                                    .file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .starts_with(".summary-backup-")
                        })
                        .unwrap();
                    std::fs::write(staged, "corrupt")?;
                }
                Ok(())
            })
            .is_err()
    );
    fixture.assert_initial();
}
#[test]
fn summary_identity_and_selected_family_identity_are_required() {
    let fixture = Fixture::new(true);
    let publisher = fixture.publisher();
    let mut summary = fixture.summary();
    summary["families"]["classic"]["summary"]["runId"] = "forged".into();
    assert!(publisher.publish(&summary, None).is_err());
    fixture.assert_initial();
    let mut summary = fixture.summary();
    summary["runId"] = "forged".into();
    assert!(publisher.publish(&summary, None).is_err());
    fixture.assert_initial();
}
