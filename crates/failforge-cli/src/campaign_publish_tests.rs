use super::*;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    wire: Value,
    payloads: Vec<Vec<u8>>,
}

impl Fixture {
    fn new(profile: bool, complete: bool) -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-publish-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let root = base.join("output");
        fs::create_dir(&root).unwrap();
        let mut paths = vec!["summary.json".to_owned()];
        if profile {
            fs::create_dir_all(root.join(".profile-reports/pending/families")).unwrap();
            fs::create_dir(root.join("families")).unwrap();
            symlink(
                ".profile-reports/pending",
                root.join(".profile-reports-current"),
            )
            .unwrap();
            symlink(
                ".profile-reports-current/summary.json",
                root.join("summary.json"),
            )
            .unwrap();
            fs::write(
                root.join(".profile-reports/pending/summary.json"),
                b"pending\n",
            )
            .unwrap();
            for family in FAMILIES {
                fs::create_dir(root.join("families").join(family)).unwrap();
                fs::create_dir(root.join(".profile-reports/pending/families").join(family))
                    .unwrap();
                fs::write(
                    root.join(".profile-reports/pending/families")
                        .join(family)
                        .join("summary.json"),
                    b"pending\n",
                )
                .unwrap();
                symlink(
                    format!("../../.profile-reports-current/families/{family}/summary.json"),
                    root.join("families").join(family).join("summary.json"),
                )
                .unwrap();
                paths.push(format!("families/{family}/summary.json"));
            }
        } else {
            fs::write(root.join("summary.json"), b"pending\n").unwrap();
        }
        let payloads = paths
            .iter()
            .map(|path| {
                let mut raw = serde_json::to_vec_pretty(
                    &json!({"path":path,"complete":complete,"text":"тест"}),
                )
                .unwrap();
                raw.push(b'\n');
                raw
            })
            .collect::<Vec<_>>();
        let files = paths
            .iter()
            .zip(&payloads)
            .map(|(path, raw)| {
                json!({
                    "path":path,"bytes":raw.len(),"sha256":format!("{:x}", Sha256::digest(raw))
                })
            })
            .collect::<Vec<_>>();
        let mut names = vec!["".to_owned()];
        if profile {
            names.extend(
                [
                    ".profile-reports",
                    ".profile-reports/pending",
                    "families",
                    ".profile-reports-current",
                    "summary.json",
                ]
                .map(str::to_owned),
            );
            for family in FAMILIES {
                names.push(format!("families/{family}"));
                names.push(format!("families/{family}/summary.json"));
            }
        }
        let bindings = names
            .iter()
            .map(|name| {
                let metadata = fs::symlink_metadata(root.join(name)).unwrap();
                let mut value = json!({"path":name,"device":metadata.dev(),"inode":metadata.ino(),
                "kind":if metadata.is_dir() {"directory"} else {"symlink"}});
                if metadata.file_type().is_symlink() {
                    value["target"] = fs::read_link(root.join(name))
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .into();
                }
                value
            })
            .collect::<Vec<_>>();
        let mut summary = json!({
            "runId":"b".repeat(32),"output":root,
            "status":if complete {"no_mutants"} else {"failed"},
            "complete":complete,"expectedMutants":0,"completedMutants":0,
            "outcomeCounts":{},"reasonCounts":{},"wallSeconds":0.25,
        });
        if profile {
            summary["kind"] = "profile".into();
            let mut families = serde_json::Map::new();
            for family in FAMILIES {
                let mut entry = json!({
                    "enabled":true,"status":"no_mutants","expectedMutants":0,
                    "summary":{"expectedMutants":0,"completedMutants":0,"outcomeCounts":{},"reasonCounts":{}},
                });
                if family == "language_specific" {
                    entry["language"] = "dart".into();
                    entry["summary"]["language"] = "dart".into();
                }
                families.insert(family.into(), entry);
            }
            summary["families"] = families.into();
        } else {
            summary["family"] = "classic".into();
        }
        let progress = crate::campaign_progress::from_summary(&summary).unwrap();
        Self {
            base,
            root: root.clone(),
            payloads,
            wire: json!({
                "operation":"campaign-publish","output":root,"token":"a".repeat(32),
                "profile":profile,"complete":complete,"runId":"b".repeat(32),"bindings":bindings,
                "files":files,"progress":progress
            }),
        }
    }

    fn request(&self) -> Request {
        decode(&serde_json::to_vec(&self.wire).unwrap()).unwrap()
    }

    fn private(&self, raw: &[u8]) -> File {
        let path = self
            .base
            .join(format!("payload-{}", NEXT.fetch_add(1, Ordering::Relaxed)));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(raw).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        fs::remove_file(path).unwrap();
        file
    }

    fn caps(&self) -> Vec<File> {
        let mut result = vec![
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&self.root)
                .unwrap(),
        ];
        result.extend(self.payloads.iter().map(|raw| self.private(raw)));
        result
    }

    fn pending(&self) {
        assert_eq!(
            fs::read(self.root.join("summary.json")).unwrap(),
            b"pending\n"
        );
        if self.wire["profile"] == true {
            assert_eq!(
                fs::read_link(self.root.join(".profile-reports-current")).unwrap(),
                PathBuf::from(".profile-reports/pending")
            );
            for family in FAMILIES {
                assert_eq!(
                    fs::read(self.root.join("families").join(family).join("summary.json")).unwrap(),
                    b"pending\n"
                );
            }
        }
    }

    fn published(&self) {
        if !self.wire["progress"].is_null() {
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &fs::read(self.root.join("progress.json")).unwrap()
                )
                .unwrap(),
                self.wire["progress"]
            );
        }
        for (payload, metadata) in self
            .payloads
            .iter()
            .zip(self.wire["files"].as_array().unwrap())
        {
            let path = self.root.join(metadata["path"].as_str().unwrap());
            assert_eq!(&fs::read(&path).unwrap(), payload);
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        }
        if self.wire["profile"] == true {
            let prefix = if self.wire["complete"] == true {
                "final"
            } else {
                "partial"
            };
            let generation = format!(
                ".profile-reports/{prefix}-{}",
                self.wire["token"].as_str().unwrap()
            );
            assert_eq!(
                fs::read_link(self.root.join(".profile-reports-current")).unwrap(),
                PathBuf::from(&generation)
            );
            assert_eq!(
                fs::metadata(self.root.join(generation)).unwrap().mode() & 0o777,
                0o700
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn complete_and_incomplete_direct_and_profile_publish_exact_private_files() {
    for profile in [false, true] {
        for complete in [false, true] {
            let fixture = Fixture::new(profile, complete);
            let mut steps = Vec::new();
            publish(&fixture.request(), fixture.caps(), &mut |step| {
                steps.push(step.to_owned());
                Ok(())
            })
            .unwrap();
            fixture.published();
            assert_eq!(steps.last().unwrap(), "commit");
            if profile {
                assert!(publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).is_err());
                fixture.published();
            }
        }
    }
}

#[test]
fn every_precommit_failure_keeps_old_reports_and_allows_a_fresh_retry() {
    for profile in [false, true] {
        let reference = Fixture::new(profile, true);
        let mut steps = Vec::new();
        publish(&reference.request(), reference.caps(), &mut |step| {
            steps.push(step.to_owned());
            Ok(())
        })
        .unwrap();
        assert!(steps.len() >= if profile { 25 } else { 5 });
        for (index, name) in steps.iter().enumerate() {
            let mut fixture = Fixture::new(profile, true);
            let mut seen = 0;
            let error = publish(&fixture.request(), fixture.caps(), &mut |_| {
                let selected = seen;
                seen += 1;
                if selected == index {
                    Err(io::Error::other("injected publication fault"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "injected publication fault", "{name}");
            fixture.pending();
            fixture.wire["token"] = "c".repeat(32).into();
            publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).unwrap();
            fixture.published();
        }
    }
}

#[test]
fn final_progress_sync_failure_prevents_any_final_summary_staging() {
    for profile in [false, true] {
        for failure in [
            "progress:sync-file",
            "progress:sync-root",
            "progress:sync-commit",
        ] {
            let fixture = Fixture::new(profile, true);
            let mut steps = Vec::new();
            let error = publish(&fixture.request(), fixture.caps(), &mut |step| {
                steps.push(step.to_owned());
                if step == failure {
                    Err(io::Error::other("progress sync failed"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(error.to_string(), "progress sync failed", "{failure}");
            assert!(steps.iter().all(|step| step.starts_with("progress:")));
            assert!(
                !fixture
                    .root
                    .join(".summary-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tmp")
                    .exists()
            );
            assert!(
                !fixture
                    .root
                    .join(".profile-reports/final-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                    .exists()
            );
            fixture.pending();
        }
    }
}

#[test]
fn incomplete_retry_can_preserve_an_unwritable_progress_target() {
    for profile in [false, true] {
        let mut fixture = Fixture::new(profile, false);
        let target = fixture.root.join("progress.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"foreign").unwrap();
        assert!(publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).is_err());
        fixture.pending();
        fixture.wire["progress"] = Value::Null;
        let mut steps = Vec::new();
        publish(&fixture.request(), fixture.caps(), &mut |step| {
            steps.push(step.to_owned());
            Ok(())
        })
        .unwrap();
        assert!(!steps.iter().any(|step| step.starts_with("progress:")));
        fixture.published();
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"foreign");
        assert_eq!(fs::read_dir(target).unwrap().count(), 1);
    }
}

#[test]
fn unsupported_progress_outcome_is_rejected_before_any_filesystem_operation() {
    let mut fixture = Fixture::new(false, true);
    let progress = &mut fixture.wire["progress"];
    progress["intendedStatus"] = "completed".into();
    progress["expectedMutants"] = 1.into();
    progress["completedMutants"] = 1.into();
    progress["outcomeCounts"] = json!({"FUTURE_OUTCOME":1});
    progress["reasonCounts"] = json!({"reason":1});
    assert!(decode(&serde_json::to_vec(&fixture.wire).unwrap()).is_err());
    assert!(!fixture.root.join("progress.json").exists());
    fixture.pending();
}

#[test]
fn malformed_request_paths_shapes_bindings_and_file_order_fail_before_writes() {
    let fixture = Fixture::new(true, true);
    for (pointer, value) in [
        ("/operation", json!("other")),
        ("/token", json!("A".repeat(32))),
        ("/runId", json!("short")),
        ("/output", json!("/a/../b")),
        ("/output", json!("/")),
        ("/files/0/path", json!("../summary.json")),
        ("/files/0/bytes", json!(0)),
        ("/files/0/sha256", json!("bad")),
        ("/bindings/0", json!([])),
        ("/bindings/0/kind", json!("file")),
        ("/bindings/1/target", Value::Null),
        ("/files/1/path", json!("families/rust/summary.json")),
        ("/progress", Value::Null),
        ("/progress", json!([])),
        ("/progress/runId", json!("c".repeat(32))),
        ("/progress/output", json!("/different")),
        ("/progress/complete", json!(true)),
        ("/progress/status", json!("completed")),
        ("/progress/intendedStatus", json!("failed")),
    ] {
        let mut wire = fixture.wire.clone();
        if pointer == "/bindings/1/target" {
            wire["bindings"][1]["target"] = value;
        } else {
            *wire.pointer_mut(pointer).unwrap() = value;
        }
        assert!(
            decode(&serde_json::to_vec(&wire).unwrap()).is_err(),
            "{pointer}"
        );
    }
    for pointer in ["", "/files/0", "/bindings/0", "/progress"] {
        let mut wire = fixture.wire.clone();
        wire.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), true.into());
        assert!(decode(&serde_json::to_vec(&wire).unwrap()).is_err());
    }
    let mut wire = fixture.wire.clone();
    wire["files"].as_array_mut().unwrap().swap(1, 2);
    assert!(decode(&serde_json::to_vec(&wire).unwrap()).is_err());
    let mut wire = fixture.wire.clone();
    wire["bindings"].as_array_mut().unwrap().pop();
    assert!(decode(&serde_json::to_vec(&wire).unwrap()).is_err());
    let mut wire = fixture.wire.clone();
    wire.as_object_mut().unwrap().remove("progress");
    assert!(decode(&serde_json::to_vec(&wire).unwrap()).is_err());
    assert!(decode(b"{\"operation\":1,\"operation\":2}").is_err());
    fixture.pending();
}

#[test]
fn root_and_profile_directory_replacements_never_receive_publication_writes() {
    for name in [
        "",
        ".profile-reports",
        ".profile-reports/pending",
        "families",
        "families/classic",
    ] {
        let fixture = Fixture::new(true, true);
        let request = fixture.request();
        let caps = fixture.caps();
        let original = fixture.root.join(name);
        let moved = fixture.base.join("moved");
        fs::rename(&original, &moved).unwrap();
        fs::create_dir(&original).unwrap();
        fs::write(original.join("sentinel"), b"foreign").unwrap();
        assert!(publish(&request, caps, &mut |_| Ok(())).is_err(), "{name}");
        assert_eq!(fs::read(original.join("sentinel")).unwrap(), b"foreign");
        assert_eq!(fs::read_dir(original).unwrap().count(), 1);
    }
}

#[test]
fn profile_alias_replacement_and_ancestor_alias_are_rejected_without_following_them() {
    for name in [
        ".profile-reports-current",
        "summary.json",
        "families/classic/summary.json",
    ] {
        let fixture = Fixture::new(true, true);
        let request = fixture.request();
        let caps = fixture.caps();
        let foreign = fixture.base.join("foreign");
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("summary.json"), b"sentinel").unwrap();
        fs::remove_file(fixture.root.join(name)).unwrap();
        symlink(&foreign, fixture.root.join(name)).unwrap();
        assert!(publish(&request, caps, &mut |_| Ok(())).is_err());
        assert_eq!(fs::read(foreign.join("summary.json")).unwrap(), b"sentinel");
        assert_eq!(fs::read_dir(foreign).unwrap().count(), 1);
    }
    let mut fixture = Fixture::new(false, true);
    let alias = fixture.base.join("alias");
    symlink(&fixture.root, &alias).unwrap();
    fixture.wire["output"] = json!(&alias);
    fixture.wire["progress"]["output"] = json!(&alias);
    assert!(publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).is_err());
    fixture.pending();
}

#[test]
fn replacements_at_commit_fail_closed_and_preserve_both_old_and_foreign_trees() {
    for name in [
        ".profile-reports",
        "families/classic",
        ".profile-reports/pending",
    ] {
        let fixture = Fixture::new(true, true);
        let request = fixture.request();
        let caps = fixture.caps();
        let target = fixture.root.join(name);
        let moved = fixture.base.join("moved");
        assert!(
            publish(&request, caps, &mut |step| {
                if step == "commit" {
                    fs::rename(&target, &moved)?;
                    fs::create_dir(&target)?;
                    fs::write(target.join("sentinel"), b"foreign")?;
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"foreign");
        assert_eq!(fs::read_dir(target).unwrap().count(), 1);
        assert!(moved.is_dir());
    }
}

#[test]
fn staged_file_pointer_and_generation_substitution_cannot_be_committed_or_deleted() {
    for name in [
        ".profile-reports/final-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ".profile-reports/final-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/summary.json",
        ".profile-reports-current.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tmp",
    ] {
        let fixture = Fixture::new(true, true);
        let target = fixture.root.join(name);
        let moved = fixture.base.join("moved");
        assert!(
            publish(&fixture.request(), fixture.caps(), &mut |step| {
                if step == "commit" {
                    fs::rename(&target, &moved)?;
                    if moved.is_dir() {
                        fs::create_dir(&target)?;
                        fs::write(target.join("sentinel"), b"foreign")?;
                    } else {
                        fs::write(&target, b"foreign")?;
                    }
                }
                Ok(())
            })
            .is_err()
        );
        fixture.pending();
        assert!(target.symlink_metadata().is_ok());
        assert!(moved.symlink_metadata().is_ok());
    }
}

#[test]
fn payload_type_links_permissions_offset_size_and_hash_are_validated_before_writes() {
    for variant in [
        "linked",
        "mode",
        "offset",
        "size",
        "hash",
        "directory",
        "write-only",
    ] {
        let mut fixture = Fixture::new(false, true);
        let mut caps = fixture.caps();
        match variant {
            "linked" => {
                let path = fixture.base.join("linked");
                fs::write(&path, &fixture.payloads[0]).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                caps[1] = File::open(path).unwrap();
            }
            "mode" => caps[1]
                .set_permissions(fs::Permissions::from_mode(0o644))
                .unwrap(),
            "offset" => {
                caps[1].seek(SeekFrom::Start(1)).unwrap();
            }
            "size" => fixture.wire["files"][0]["bytes"] = 1.into(),
            "hash" => fixture.wire["files"][0]["sha256"] = "0".repeat(64).into(),
            "directory" => caps[1] = File::open(&fixture.base).unwrap(),
            "write-only" => {
                let path = fixture.base.join("write-only");
                caps[1] = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .unwrap();
                caps[1].write_all(&fixture.payloads[0]).unwrap();
                caps[1].seek(SeekFrom::Start(0)).unwrap();
                fs::remove_file(path).unwrap();
            }
            _ => unreachable!(),
        }
        let mut mutated = false;
        assert!(
            publish(&fixture.request(), caps, &mut |_| {
                mutated = true;
                Ok(())
            })
            .is_err(),
            "{variant}"
        );
        assert!(!mutated, "{variant}");
        fixture.pending();
    }
}

#[test]
fn payload_mutation_after_initial_hash_does_not_publish() {
    let fixture = Fixture::new(false, true);
    let caps = fixture.caps();
    let source = caps[1].try_clone().unwrap();
    assert!(
        publish(&fixture.request(), caps, &mut |step| {
            if step == "write:summary" {
                source.write_all_at(b"x", 0)?;
            }
            Ok(())
        })
        .is_err()
    );
    fixture.pending();
}

#[test]
fn in_place_staged_content_mutation_before_commit_is_rejected() {
    for profile in [false, true] {
        for phase in ["commit", "copied"] {
            let fixture = Fixture::new(profile, true);
            let name = if profile {
                ".profile-reports/final-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/summary.json"
            } else {
                ".summary-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tmp"
            };
            assert!(
                publish(&fixture.request(), fixture.caps(), &mut |step| {
                    if (phase == "commit" && step == "commit")
                        || (phase == "copied" && step.starts_with("copied:summary"))
                    {
                        let mut forged = fixture.payloads[0].clone();
                        forged[0] = b'[';
                        fs::write(fixture.root.join(name), forged)?;
                    }
                    Ok(())
                })
                .is_err()
            );
            fixture.pending();
        }
    }
}

#[test]
fn staged_directory_replacement_after_hashing_starts_cannot_be_published() {
    for suffix in ["", "/families", "/families/classic"] {
        let fixture = Fixture::new(true, true);
        let target = fixture.root.join(format!(
            ".profile-reports/final-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{suffix}"
        ));
        let moved = fixture.base.join("moved-staged-directory");
        let result = publish(&fixture.request(), fixture.caps(), &mut |step| {
            if step == "verified:summary.json" {
                fs::rename(&target, &moved)?;
                fs::create_dir(&target)?;
                fs::write(target.join("sentinel"), b"foreign")?;
            }
            Ok(())
        });
        assert!(
            result.is_err(),
            "replaced staged directory was published: {suffix}"
        );
        fixture.pending();
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"foreign");
        assert!(moved.is_dir());
    }
}

#[test]
fn earlier_staged_file_changes_during_later_hashes_cannot_be_published() {
    for replace in [false, true] {
        let fixture = Fixture::new(true, true);
        let target = fixture
            .root
            .join(".profile-reports/final-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/summary.json");
        let moved = fixture.base.join("moved-staged-file");
        let result = publish(&fixture.request(), fixture.caps(), &mut |step| {
            if step == "verified:families/classic/summary.json" {
                if replace {
                    fs::rename(&target, &moved)?;
                }
                let mut forged = fixture.payloads[0].clone();
                forged[0] = b'[';
                fs::write(&target, forged)?;
            }
            Ok(())
        });
        assert!(
            result.is_err(),
            "changed earlier file was published: replace={replace}"
        );
        fixture.pending();
        assert!(target.is_file());
    }
}

#[test]
fn direct_partial_initialization_alias_is_replaced_without_touching_its_target() {
    let fixture = Fixture::new(false, false);
    let foreign = fixture.base.join("foreign-summary");
    fs::write(&foreign, b"sentinel").unwrap();
    fs::remove_file(fixture.root.join("summary.json")).unwrap();
    symlink(&foreign, fixture.root.join("summary.json")).unwrap();
    publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).unwrap();
    fixture.published();
    assert_eq!(fs::read(foreign).unwrap(), b"sentinel");
    let fixture = Fixture::new(false, false);
    fs::remove_file(fixture.root.join("summary.json")).unwrap();
    symlink(
        ".profile-reports-current/summary.json",
        fixture.root.join("summary.json"),
    )
    .unwrap();
    publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).unwrap();
    fixture.published();
}

#[test]
fn direct_directory_and_fifo_targets_are_rejected_without_modification() {
    for fifo in [false, true] {
        let fixture = Fixture::new(false, true);
        let target = fixture.root.join("summary.json");
        fs::remove_file(&target).unwrap();
        if fifo {
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
            // SAFETY: mkfifo receives a valid terminated fixture path.
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        } else {
            fs::create_dir(&target).unwrap();
            fs::write(target.join("sentinel"), b"foreign").unwrap();
        }
        assert!(publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).is_err());
        assert!(fs::symlink_metadata(target).is_ok());
    }
}

#[test]
fn aggregate_payload_larger_than_private_json_limit_is_streamed() {
    let mut fixture = Fixture::new(false, true);
    let count = 64 * 1024 * 1024 + 1;
    fixture.payloads[0] = vec![b' '; count];
    fixture.payloads[0][0] = b'[';
    fixture.payloads[0][count - 1] = b']';
    fixture.wire["files"][0]["bytes"] = (count as u64).into();
    fixture.wire["files"][0]["sha256"] =
        format!("{:x}", Sha256::digest(&fixture.payloads[0])).into();
    publish(&fixture.request(), fixture.caps(), &mut |_| Ok(())).unwrap();
    assert_eq!(
        fs::metadata(fixture.root.join("summary.json"))
            .unwrap()
            .len(),
        count as u64
    );
    let mut file = File::open(fixture.root.join("summary.json")).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    assert_eq!(
        format!("{:x}", digest.finalize()),
        fixture.wire["files"][0]["sha256"].as_str().unwrap()
    );
}

#[test]
fn in_process_payloads_are_unlinked_private_files_at_offset_zero() {
    let fixture = Fixture::new(false, false);
    let root = files::walk(&fixture.root).unwrap();
    let before = fs::read_dir(&fixture.root).unwrap().count();
    let raw = b"full retained reporting evidence\n";
    let mut file = anonymous_payload(&root, raw).unwrap();
    let hash = format!("{:x}", Sha256::digest(raw));
    files::payload(&file, raw.len() as u64, &hash).unwrap();
    let mut actual = Vec::new();
    file.read_to_end(&mut actual).unwrap();
    assert_eq!(actual, raw);
    assert_eq!(file.metadata().unwrap().nlink(), 0);
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), before);
}

#[test]
fn in_process_payload_substitution_before_unlink_preserves_foreign_entry() {
    for alias in [false, true] {
        let fixture = Fixture::new(false, false);
        let root = files::walk(&fixture.root).unwrap();
        let mut substituted = None;
        let result = anonymous_payload_with_hook(&root, b"report", &mut |step| {
            assert_eq!(step, "unlink:payload");
            let path = fs::read_dir(&fixture.root)?
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".report-payload-")
                })
                .unwrap();
            fs::rename(&path, fixture.root.join("held-payload"))?;
            if alias {
                symlink("foreign-target", &path)?;
            } else {
                fs::write(&path, b"foreign")?;
            }
            substituted = Some(path);
            Ok(())
        });
        assert!(result.is_err());
        let path = substituted.unwrap();
        if alias {
            assert_eq!(
                fs::read_link(path).unwrap(),
                PathBuf::from("foreign-target")
            );
        } else {
            assert_eq!(fs::read(path).unwrap(), b"foreign");
        }
        assert!(fixture.root.join("held-payload").is_file());
        fixture.pending();
    }
}

#[test]
fn multiply_linked_in_process_payload_is_not_unlinked() {
    let fixture = Fixture::new(false, false);
    let root = files::walk(&fixture.root).unwrap();
    let mut staged = None;
    assert!(
        anonymous_payload_with_hook(&root, b"report", &mut |_| {
            let path = fs::read_dir(&fixture.root)?
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".report-payload-")
                })
                .unwrap();
            fs::hard_link(&path, fixture.root.join("extra-payload-link"))?;
            staged = Some(path);
            Ok(())
        })
        .is_err()
    );
    assert_eq!(fs::metadata(staged.unwrap()).unwrap().nlink(), 2);
    assert!(fixture.root.join("extra-payload-link").is_file());
    fixture.pending();
}
