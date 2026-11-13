use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};

const RUN: &str = "0123456789abcdef0123456789abcdef";

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-progress-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn writer(&self) -> Writer {
        Writer::new(&self.0, files::walk(&self.0).unwrap()).unwrap()
    }
    fn read(&self) -> Value {
        serde_json::from_slice(&fs::read(self.0.join("progress.json")).unwrap()).unwrap()
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn state(mode: &str, families: &[&str]) -> State {
    State::new(
        mode,
        RUN,
        Path::new("/tmp/report"),
        &families
            .iter()
            .map(|name| (*name).into())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}
fn discover(state: &mut State, family: &str, total: usize) {
    assert!(
        !state
            .observe(&json!({"kind":"discovery","family":family,
        "value":{"candidates":vec![Value::Null; total]}}))
            .unwrap()
    );
}
fn record(family: &str, id: &str, outcome: &str) -> Value {
    json!({"recordKind":"mutation","family":family,"mutantId":id,
        "outcome":outcome,"reason":format!("reason:{outcome}"),"opaque":{"huge":"ignored"}})
}
fn mutation(state: &mut State, family: &str, id: &str, outcome: &str) {
    assert!(
        state
            .observe(&json!({"kind":"family_event","family":family,
        "event":{"kind":"mutant","record":record(family,id,outcome)}}))
            .unwrap()
    );
}
fn final_direct() -> Value {
    let mut state = state("direct", &["classic"]);
    discover(&mut state, "classic", 1);
    state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":1}))
        .unwrap();
    mutation(&mut state, "classic", "one", "KILLED");
    state.snapshot(0.25, Some("completed")).unwrap()
}

#[test]
fn constructor_rejects_ambiguous_selections_and_paths() {
    for (mode, families) in [
        ("wrong", vec!["classic"]),
        ("direct", vec![]),
        ("direct", vec!["classic", "failure_mode"]),
        ("profile", vec!["classic", "classic"]),
        ("profile", vec!["unknown"]),
    ] {
        assert!(
            State::new(
                mode,
                RUN,
                Path::new("/tmp/report"),
                &families.into_iter().map(str::to_owned).collect::<Vec<_>>()
            )
            .is_err()
        );
    }
    assert!(
        State::new(
            "direct",
            "bad",
            Path::new("/tmp/report"),
            &["classic".into()]
        )
        .is_err()
    );
    assert!(State::new("direct", RUN, Path::new("relative"), &["classic".into()]).is_err());
}

#[test]
fn direct_preserves_unknown_and_zero_discovery_without_scores() {
    let mut state = state("direct", &["language_specific"]);
    let before = state.snapshot(0.0, None).unwrap();
    assert!(before["expectedMutants"].is_null());
    assert_eq!(before["language"], "dart");
    assert_eq!(before["completedMutants"], 0);
    discover(&mut state, "language_specific", 0);
    assert!(
        state
            .observe(&json!({"kind":"discovery_finished","expectedMutants":0}))
            .unwrap()
    );
    let final_value = state.snapshot(0.5, Some("no_mutants")).unwrap();
    assert_eq!(final_value["expectedMutants"], 0);
    assert_eq!(final_value["status"], "finalizing");
    assert_eq!(final_value["intendedStatus"], "no_mutants");
    assert!(final_value.get("mutationScore").is_none());
    validate_final(&final_value, RUN, Path::new("/tmp/report"), true).unwrap();
    assert!(state.snapshot(-1.0, None).is_err());
    assert!(state.snapshot(f64::NAN, None).is_err());
    assert!(state.snapshot(1.0, Some("running")).is_err());
}

#[test]
fn profile_tracks_active_family_validation_stages_and_canonical_counts() {
    let mut state = state("profile", &["classic", "language_specific"]);
    discover(&mut state, "classic", 1);
    discover(&mut state, "language_specific", 1);
    state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":2}))
        .unwrap();
    state
        .observe(
            &json!({"kind":"stage_started","family":"classic","stage":"verify_discovery_inputs"}),
        )
        .unwrap();
    assert_eq!(
        state.snapshot(1.0, None).unwrap()["activeFamily"],
        "classic"
    );
    state
        .observe(&json!({"kind":"stage_started","stage":"verify_profile_inputs"}))
        .unwrap();
    assert!(state.snapshot(1.0, None).unwrap()["activeFamily"].is_null());
    state
        .observe(&json!({"kind":"family_started","family":"classic"}))
        .unwrap();
    mutation(&mut state, "classic", "shared-id", "KILLED");
    state
        .observe(&json!({"kind":"family_finished","family":"classic","status":"completed"}))
        .unwrap();
    state
        .observe(&json!({"kind":"family_started","family":"language_specific"}))
        .unwrap();
    mutation(&mut state, "language_specific", "shared-id", "TIMEOUT");
    let value = state.snapshot(2.5, Some("failed")).unwrap();
    assert!(value["activeFamily"].is_null());
    assert_eq!(value["completedMutants"], 2);
    assert_eq!(value["outcomeCounts"], json!({"KILLED":1,"TIMEOUT":1}));
    assert_eq!(
        value["families"]["failure_mode"],
        json!({"enabled":false,"status":"disabled"})
    );
    assert_eq!(value["families"]["language_specific"]["status"], "running");
    validate_final(&value, RUN, Path::new("/tmp/report"), false).unwrap();
}

#[test]
fn mutation_rejection_is_transactional_and_all_outcomes_are_counted() {
    let mut state = state("direct", &["classic"]);
    discover(&mut state, "classic", 9);
    state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":9}))
        .unwrap();
    for outcome in OUTCOMES {
        mutation(&mut state, "classic", outcome, outcome);
    }
    let original = state.snapshot(0.0, None).unwrap();
    for invalid in [
        record("classic", "KILLED", "KILLED"),
        record("other", "new", "KILLED"),
        record("classic", "new", "BAD"),
        record("classic", "", "KILLED"),
        json!({"recordKind":"mutation","family":"classic","mutantId":"new","outcome":"KILLED","reason":[]}),
    ] {
        assert!(
            state
                .observe(&json!({"kind":"family_event","family":"classic",
            "event":{"kind":"mutant","record":invalid}}))
                .is_err()
        );
        assert_eq!(state.snapshot(0.0, None).unwrap(), original);
    }
    mutation(&mut state, "classic", "ninth", "SURVIVED");
    assert!(
        state
            .observe(&json!({"kind":"family_event","family":"classic",
        "event":{"kind":"mutant","record":record("classic","tenth","KILLED")}}))
            .is_err()
    );
}

#[test]
fn ten_thousand_mutants_keep_only_compact_counters_in_progress() {
    let mut state = state("direct", &["classic"]);
    discover(&mut state, "classic", 10_000);
    state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":10_000}))
        .unwrap();
    for id in 0..10_000 {
        mutation(&mut state, "classic", &id.to_string(), "SURVIVED");
    }
    let value = state.snapshot(1.0, Some("completed")).unwrap();
    assert_eq!(value["completedMutants"], 10_000);
    assert!(serde_json::to_vec(&value).unwrap().len() < 1024);
    assert!(value.get("records").is_none());
    assert!(!serde_json::to_string(&value).unwrap().contains("opaque"));
}

#[test]
fn summary_projection_matches_compact_final_schema_without_attempts() {
    let expected = final_direct();
    let mut summary = expected.clone();
    summary["status"] = "completed".into();
    summary["complete"] = true.into();
    summary["records"] = json!([{"large":"payload not copied"}]);
    assert_eq!(from_summary(&summary).unwrap(), expected);
    let mut state = state("profile", &["classic", "language_specific"]);
    discover(&mut state, "classic", 0);
    discover(&mut state, "language_specific", 0);
    state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":0}))
        .unwrap();
    let expected = state.snapshot(1.0, Some("failed")).unwrap();
    let mut summary = expected.clone();
    summary["status"] = "failed".into();
    for family in ["classic", "language_specific"] {
        let row = expected["families"][family].clone();
        summary["families"][family]["summary"] = row;
    }
    assert_eq!(from_summary(&summary).unwrap(), expected);
}

#[test]
fn final_validation_rejects_malformed_fields_counts_and_completion() {
    let valid = final_direct();
    for (key, bad) in [
        ("kind", json!("wrong")),
        ("version", json!(true)),
        ("runId", json!("bad")),
        ("complete", json!(true)),
        ("status", json!("running")),
        ("intendedStatus", json!("failed")),
        ("expectedMutants", json!(0)),
        ("completedMutants", json!(2)),
        ("wallSeconds", json!(-0.1)),
        ("outcomeCounts", json!({"BAD":1})),
        ("reasonCounts", json!({"reason:KILLED":0})),
    ] {
        let mut value = valid.clone();
        value[key] = bad;
        assert!(
            validate_final(&value, RUN, Path::new("/tmp/report"), true).is_err(),
            "{key}"
        );
    }
    let mut extra = valid.clone();
    extra["records"] = json!([]);
    assert!(validate_final(&extra, RUN, Path::new("/tmp/report"), true).is_err());
    assert!(validate_final(&valid, RUN, Path::new("/tmp/other"), true).is_err());
}

#[test]
fn incomplete_unknown_totals_preserve_completed_direct_and_profile_results() {
    let mut direct = final_direct();
    direct["intendedStatus"] = "failed".into();
    direct["expectedMutants"] = Value::Null;
    validate_final(&direct, RUN, Path::new("/tmp/report"), false).unwrap();
    let mut summary = direct.clone();
    summary["status"] = "failed".into();
    assert_eq!(from_summary(&summary).unwrap(), direct);
    let mut state = state("profile", &["classic", "failure_mode"]);
    discover(&mut state, "classic", 1);
    mutation(&mut state, "classic", "done", "KILLED");
    state
        .observe(&json!({"kind":"family_finished","family":"classic","status":"completed"}))
        .unwrap();
    let profile = state.snapshot(1.0, Some("failed")).unwrap();
    assert!(profile["expectedMutants"].is_null());
    assert_eq!(profile["completedMutants"], 1);
    assert!(profile["families"]["failure_mode"]["expectedMutants"].is_null());
    validate_final(&profile, RUN, Path::new("/tmp/report"), false).unwrap();
    let mut summary = profile.clone();
    summary["status"] = "failed".into();
    for family in ["classic", "failure_mode"] {
        summary["families"][family]["summary"] = profile["families"][family].clone();
    }
    assert_eq!(from_summary(&summary).unwrap(), profile);
}

#[test]
fn profile_final_validation_rejects_inconsistent_family_aggregates() {
    let mut state = state("profile", &["classic"]);
    discover(&mut state, "classic", 1);
    state
        .observe(&json!({"kind":"discovery_finished","expectedMutants":1}))
        .unwrap();
    mutation(&mut state, "classic", "a", "KILLED");
    state
        .observe(&json!({"kind":"family_finished","family":"classic","status":"completed"}))
        .unwrap();
    let value = state.snapshot(1.0, Some("completed")).unwrap();
    validate_final(&value, RUN, Path::new("/tmp/report"), true).unwrap();
    for (pointer, bad) in [
        ("/activeFamily", json!("classic")),
        ("/families/classic/status", json!("invalid")),
        ("/families/classic/status", json!("running")),
        ("/families/classic/status", json!("no_mutants")),
        ("/families/classic/expectedMutants", json!(2)),
        ("/families/classic/outcomeCounts", json!({"SURVIVED":1})),
        ("/families/classic/reasonCounts", json!({"other":1})),
        ("/families/failure_mode/enabled", json!(0)),
        ("/families/failure_mode/status", json!("completed")),
    ] {
        let mut malformed = value.clone();
        *malformed.pointer_mut(pointer).unwrap() = bad;
        assert!(
            validate_final(&malformed, RUN, Path::new("/tmp/report"), true).is_err(),
            "{pointer}"
        );
    }
}

#[test]
fn completed_progress_requires_exact_totals_and_no_mutants_requires_zero() {
    let valid = final_direct();
    for (key, bad) in [
        ("expectedMutants", Value::Null),
        ("expectedMutants", json!(2)),
        ("intendedStatus", json!("no_mutants")),
    ] {
        let mut value = valid.clone();
        value[key] = bad;
        assert!(validate_final(&value, RUN, Path::new("/tmp/report"), true).is_err());
    }
    let mut zero = state("direct", &["classic"]);
    zero.observe(&json!({"kind":"discovery_finished","expectedMutants":0}))
        .unwrap();
    let invalid = zero.snapshot(0.0, Some("completed")).unwrap();
    assert!(validate_final(&invalid, RUN, Path::new("/tmp/report"), true).is_err());
}

#[test]
fn writer_atomically_replaces_private_checkpoints_and_accepts_new_owner() {
    let temp = Temp::new();
    let mut writer = temp.writer();
    writer.write(&json!({"counter":1})).unwrap();
    let first = fs::metadata(temp.0.join("progress.json")).unwrap();
    writer.write(&json!({"counter":2})).unwrap();
    let second = fs::metadata(temp.0.join("progress.json")).unwrap();
    assert_ne!(first.ino(), second.ino());
    assert_eq!(second.mode() & 0o777, 0o600);
    assert_eq!(temp.read(), json!({"counter":2}));
    temp.writer().write(&json!({"counter":3})).unwrap();
    assert_eq!(temp.read(), json!({"counter":3}));
    assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1);
}

#[test]
fn every_write_fault_preserves_old_or_complete_new_json() {
    for label in [
        "create-stage",
        "write-stage",
        "sync-file",
        "verify-stage",
        "sync-root",
        "commit",
        "sync-commit",
    ] {
        let temp = Temp::new();
        let mut writer = temp.writer();
        writer.write(&json!({"old":true})).unwrap();
        let error = writer.write_with_hook(&json!({"new":true}), &mut |step| {
            if step == label {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        });
        assert!(error.is_err(), "{label}");
        assert_eq!(
            temp.read(),
            if label == "sync-commit" {
                json!({"new":true})
            } else {
                json!({"old":true})
            },
            "{label}"
        );
        temp.writer().write(&json!({"retry":true})).unwrap();
        assert_eq!(temp.read(), json!({"retry":true}));
    }
}

#[test]
fn writer_refuses_replaced_root_or_symlink_ancestor_without_foreign_writes() {
    let temp = Temp::new();
    let output = temp.0.join("reports");
    fs::create_dir(&output).unwrap();
    let mut writer = Writer::new(&output, files::walk(&output).unwrap()).unwrap();
    fs::rename(&output, temp.0.join("moved")).unwrap();
    fs::create_dir(&output).unwrap();
    assert!(writer.write(&json!({})).is_err());
    assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
    fs::remove_dir(&output).unwrap();
    symlink(temp.0.join("moved"), &output).unwrap();
    assert!(Writer::new(&output, files::walk(&temp.0.join("moved")).unwrap()).is_err());
}

#[test]
fn writer_refuses_foreign_public_file_replacement_and_in_place_edit() {
    for in_place in [false, true] {
        let temp = Temp::new();
        let mut writer = temp.writer();
        writer.write(&json!({"owned":true})).unwrap();
        if !in_place {
            fs::rename(temp.0.join("progress.json"), temp.0.join("original")).unwrap();
        }
        fs::write(temp.0.join("progress.json"), b"foreign").unwrap();
        fs::set_permissions(
            temp.0.join("progress.json"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(writer.write(&json!({"new":true})).is_err());
        assert_eq!(fs::read(temp.0.join("progress.json")).unwrap(), b"foreign");
    }
}

#[test]
fn writer_rechecks_stage_and_destination_immediately_before_commit() {
    for mode in [
        "stage-replace",
        "stage-edit",
        "destination-replace",
        "root-replace",
    ] {
        let temp = Temp::new();
        let output = temp.0.join("reports");
        fs::create_dir(&output).unwrap();
        let mut writer = Writer::new(&output, files::walk(&output).unwrap()).unwrap();
        writer.write(&json!({"old":true})).unwrap();
        let error = writer.write_with_hook(&json!({"new":true}), &mut |step| {
            if step == "commit" {
                match mode {
                    "stage-replace" | "stage-edit" => {
                        let stage = fs::read_dir(&output)?
                            .map(|entry| entry.unwrap().path())
                            .find(|path| path.extension().is_some_and(|ext| ext == "tmp"))
                            .unwrap();
                        if mode == "stage-replace" {
                            fs::rename(&stage, output.join("removed-stage"))?;
                        }
                        fs::write(&stage, b"foreign")?;
                    }
                    "destination-replace" => {
                        fs::rename(output.join("progress.json"), output.join("old"))?;
                        fs::write(output.join("progress.json"), b"foreign")?;
                    }
                    _ => {
                        fs::rename(&output, temp.0.join("moved"))?;
                        fs::create_dir(&output)?;
                    }
                }
            }
            Ok(())
        });
        assert!(error.is_err(), "{mode}");
        if mode == "destination-replace" {
            assert_eq!(fs::read(output.join("progress.json")).unwrap(), b"foreign");
        } else if mode == "root-replace" {
            assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
        } else {
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(output.join("progress.json")).unwrap())
                    .unwrap(),
                json!({"old":true})
            );
        }
    }
}

#[test]
fn writer_rejects_same_size_stage_edit_before_first_fingerprint() {
    let temp = Temp::new();
    let mut writer = temp.writer();
    writer.write(&json!({"old":true})).unwrap();
    let error = writer.write_with_hook(&json!({"new":true}), &mut |step| {
        if step == "sync-file" {
            let stage = fs::read_dir(&temp.0)?
                .map(|entry| entry.unwrap().path())
                .find(|path| path.extension().is_some_and(|extension| extension == "tmp"))
                .unwrap();
            fs::write(stage, b"{\"bad\":true}\n")?;
        }
        Ok(())
    });
    assert!(error.is_err());
    assert_eq!(temp.read(), json!({"old":true}));
}

#[test]
fn special_existing_entries_and_non_directory_roots_are_refused_without_blocking() {
    for kind in ["link", "fifo", "directory", "hardlink", "public"] {
        let temp = Temp::new();
        let path = temp.0.join("progress.json");
        match kind {
            "link" => symlink("/tmp/nonexistent-progress", &path).unwrap(),
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                // SAFETY: test creates a FIFO at one owned temporary pathname.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            "directory" => fs::create_dir(&path).unwrap(),
            _ => {
                fs::write(&path, b"{}").unwrap();
                if kind == "hardlink" {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                    fs::hard_link(&path, temp.0.join("alias")).unwrap();
                } else {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                }
            }
        }
        assert!(
            Writer::new(&temp.0, files::walk(&temp.0).unwrap()).is_err(),
            "{kind}"
        );
    }
    let temp = Temp::new();
    fs::write(temp.0.join("file"), b"{}").unwrap();
    assert!(Writer::new(&temp.0, File::open(temp.0.join("file")).unwrap()).is_err());
}

#[test]
fn read_only_output_returns_error_without_overwriting_checkpoint() {
    // Root can bypass ordinary DAC permissions; the production test user cannot.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let temp = Temp::new();
    let mut writer = temp.writer();
    writer.write(&json!({"old":true})).unwrap();
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(writer.write(&json!({"new":true})).is_err());
    assert_eq!(temp.read(), json!({"old":true}));
}
