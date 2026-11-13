use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff-html-data-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn put(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn summary(&self, value: &Value) {
        self.put("summary.json", serde_json::to_vec(value).unwrap());
    }

    fn load(&self) -> Value {
        load(&self.0).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn retained_source_context_cannot_mislabel_lines_or_hide_the_mutation() {
    let valid = json!({"version":1,"sourceHash":"a".repeat(64),"line":3,
        "column":4,"endLine":3,"startLine":2,"lines":["before", "α👭true", "after"],
        "truncated":false});
    assert!(valid_context(&valid, None));
    for (field, bad) in [
        ("line", json!(5)),
        ("column", json!(9)),
        ("endLine", json!(5)),
        ("lines", json!(["before\nextra", "α👭true", "after"])),
        ("lines", json!(["before\rextra", "α👭true", "after"])),
        ("lines", json!(["x".repeat(2049), "α👭true", "after"])),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = bad;
        assert!(!valid_context(&invalid, None), "{field}");
    }
    let mut clipped = valid.clone();
    clipped["truncated"] = true.into();
    clipped["column"] = 3000.into();
    clipped["endLine"] = 20.into();
    assert!(valid_context(&clipped, None));
}

fn record(family: &str, id: &str, outcome: &str) -> Value {
    json!({"recordKind":"mutation","family":family,"mutantId":id,
        "operatorId":"RELATIONAL_BOUNDARY","file":"lib/é.dart","outcome":outcome,
        "reason":"retained_reason","edit":{"offsetBytes":12,"lengthBytes":3,
            "expected":"é!","replacement":"é?"},"opaque":{"n":9007199254740993u64}})
}

fn direct(family: &str, records: Vec<Value>) -> Value {
    let mut counts = BTreeMap::<String, u64>::new();
    for record in &records {
        *counts
            .entry(record["outcome"].as_str().unwrap().into())
            .or_default() += 1;
    }
    let killed = counts.get("KILLED").copied().unwrap_or(0);
    let survived = counts.get("SURVIVED").copied().unwrap_or(0);
    let denominator = killed + survived;
    let excluded = counts
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "KILLED" | "SURVIVED"))
        .map(|(k, v)| (k.clone(), *v))
        .collect::<BTreeMap<_, _>>();
    let mut result = json!({"version":1,"runId":"test-run","family":family,
        "status":if records.is_empty(){"no_mutants"}else{"completed"},"complete":true,
        "error":null,"records":records,"expectedMutants":records.len(),
        "completedMutants":records.len(),"wallSeconds":1.5,"outcomeCounts":counts,
        "originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,
        "workspaceCleanupDeferred":false});
    let key = match family {
        "classic" => "mutationScore",
        "failure_mode" => "failureModeScore",
        _ => "languageSpecificScore",
    };
    result[key] = json!({"family":family,"killed":killed,"survived":survived,"denominator":denominator,
        "percent":if denominator==0{Value::Null}else{json!(100.0*killed as f64/denominator as f64)},"excluded":excluded});
    result
}

#[test]
fn quality_gate_must_agree_with_retained_results_and_available_configuration() {
    use failforge_config::quality_gate::{QualityGate, ScoreThreshold};
    let fixture = Fixture::new();
    let mut summary = direct("classic", vec![record("classic", "survivor", "SURVIVED")]);
    let config = QualityGate {
        min_score: Some(ScoreThreshold::parse("80").unwrap()),
        stop_when_unreachable: false,
    };
    summary["qualityGate"] = crate::campaign::quality_gate::family(
        &config,
        None,
        "completed",
        "completed",
        Some(1),
        &[("SURVIVED".to_owned(), 1)].into(),
        1,
    )
    .unwrap()
    .unwrap();
    fixture.summary(&summary);
    assert_eq!(
        fixture.load()["families"][0]["qualityGate"]["status"],
        "failed"
    );
    fixture.put(
        "manifest.json",
        serde_json::to_vec(&json!({"runId":"test-run","configuration":{}})).unwrap(),
    );
    assert!(load(&fixture.0).is_err());
    let mut manifest = json!({"runId":"test-run","configuration":{"quality_gate":{"min_score":80,"stop_when_unreachable":false}}});
    fixture.put("manifest.json", serde_json::to_vec(&manifest).unwrap());
    assert_eq!(fixture.load()["run"]["qualityGate"]["status"], "failed");
    let gate = summary
        .as_object_mut()
        .unwrap()
        .remove("qualityGate")
        .unwrap();
    fixture.summary(&summary);
    assert!(load(&fixture.0).is_err());
    summary["qualityGate"] = gate;
    summary["qualityGate"]["status"] = json!("passed");
    fixture.summary(&summary);
    assert!(load(&fixture.0).is_err());
    summary["qualityGate"]["status"] = json!("failed");
    fixture.summary(&summary);
    manifest["configuration"]["quality_gate"]["min_score"] = json!(75);
    fixture.put("manifest.json", serde_json::to_vec(&manifest).unwrap());
    assert!(load(&fixture.0).is_err());
}

fn profile(classic: Value, failure: Value) -> Value {
    let records = classic["records"]
        .as_array()
        .unwrap()
        .iter()
        .chain(failure["records"].as_array().unwrap())
        .cloned()
        .collect::<Vec<_>>();
    let mut counts = BTreeMap::<String, u64>::new();
    for record in &records {
        *counts
            .entry(record["outcome"].as_str().unwrap().into())
            .or_default() += 1;
    }
    json!({"kind":"profile","version":2,"runId":"test-run","status":"completed","complete":true,
    "records":records,"completedMutants":records.len(),"expectedMutants":records.len(),
    "outcomeCounts":counts,"families":{
        "classic":{"enabled":true,"status":classic["status"],"complete":true,"summary":classic},
        "failure_mode":{"enabled":true,"status":failure["status"],"complete":true,"summary":failure},
        "language_specific":{"enabled":false,"status":"disabled","complete":true,"summary":null}
    }})
}

#[test]
fn direct_summary_preserves_records_and_only_reads_local_captures() {
    let fixture = Fixture::new();
    let mut summary = direct(
        "classic",
        vec![
            record("classic", "one", "KILLED"),
            record("classic", "two", "SURVIVED"),
        ],
    );
    summary["records"][0]["command"] =
        json!(["/outside/tool", "--capture-dir", "/outside/capture"]);
    fixture.summary(&summary);
    fixture.put("mutants/one/capture/stdout.bin", "retained stdout\n");
    fixture.put("mutants/one/capture/stderr.bin", "retained stderr\n");
    fixture.put(
        "manifest.json",
        json!({"runId":"test-run","configuration":{"workers":2}}).to_string(),
    );
    let value = fixture.load();
    assert_eq!(value["configuration"]["workers"], 2);
    assert_eq!(value["run"]["id"], "test-run");
    assert_eq!(value["families"][0]["score"]["percent"], 50.0);
    let records = value["families"][0]["records"].as_array().unwrap();
    assert_eq!(records[0]["opaque"], summary["records"][0]["opaque"]);
    assert_eq!(records[0]["reportLogs"][0]["text"], "retained stdout\n");
    assert_eq!(records[0]["reportLogs"][1]["text"], "retained stderr\n");
    assert_eq!(
        records[1]["reportLogs"][0]["unavailable"],
        "Capture was not retained."
    );
    assert_eq!(value, load(&fixture.0.join("summary.json")).unwrap());
}

#[test]
fn profiles_have_independent_scores_and_suppress_every_score_when_incomplete() {
    let fixture = Fixture::new();
    let mut summary = profile(
        direct("classic", vec![record("classic", "same-id", "KILLED")]),
        direct(
            "failure_mode",
            vec![record("failure_mode", "same-id", "SURVIVED")],
        ),
    );
    fixture.summary(&summary);
    fixture.put(
        "families/classic/mutants/same-id/capture/stdout.bin",
        "classic capture",
    );
    fixture.put(
        "families/failure_mode/mutants/same-id/capture/stdout.bin",
        "failure capture",
    );
    let complete = fixture.load();
    assert_eq!(complete["families"][0]["score"]["percent"], 100.0);
    assert_eq!(complete["families"][1]["score"]["percent"], 0.0);
    assert_eq!(complete["families"][2]["enabled"], false);
    assert_eq!(
        complete["families"][0]["records"][0]["reportLogs"][0]["text"],
        "classic capture"
    );
    assert_eq!(
        complete["families"][1]["records"][0]["reportLogs"][0]["text"],
        "failure capture"
    );
    for status in ["failed", "cancelled", "running"] {
        summary["status"] = status.into();
        summary["complete"] = false.into();
        fixture.summary(&summary);
        let incomplete = fixture.load();
        assert_eq!(incomplete["run"]["complete"], false);
        for family in incomplete["families"].as_array().unwrap() {
            assert!(family["score"].is_null());
        }
        assert_eq!(
            incomplete["families"][0]["records"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn no_mutants_and_all_excluded_keep_null_percent() {
    let fixture = Fixture::new();
    for rows in [
        vec![],
        vec![
            record("classic", "invalid", "INVALID"),
            record("classic", "timeout", "TIMEOUT"),
        ],
    ] {
        fixture.summary(&direct("classic", rows));
        let value = fixture.load();
        assert_eq!(value["families"][0]["score"]["denominator"], 0);
        assert!(value["families"][0]["score"]["percent"].is_null());
    }
}

#[test]
fn standalone_family_of_an_incomplete_profile_is_not_scored() {
    let fixture = Fixture::new();
    let mut summary = direct("classic", vec![record("classic", "one", "KILLED")]);
    summary["profileComplete"] = false.into();
    fixture.summary(&summary);
    let value = fixture.load();
    assert_eq!(value["run"]["complete"], false);
    assert!(value["families"][0]["score"].is_null());
}

#[test]
fn main_summary_rejects_unknown_duplicate_inconsistent_or_oversized_data() {
    let fixture = Fixture::new();
    let original = direct("classic", vec![record("classic", "one", "KILLED")]);
    for (pointer, replacement) in [
        ("/version", json!(99)),
        ("/complete", json!(false)),
        ("/outcomeCounts/KILLED", json!(2)),
        ("/completedMutants", json!(0)),
        ("/expectedMutants", json!(10001)),
        ("/mutationScore/percent", json!(0)),
        ("/records/0/family", json!("failure_mode")),
        ("/records/0/outcome", json!("NOT_COVERED")),
        ("/records/0/edit/lengthBytes", json!(2)),
        ("/records/0/mutantId", json!("../outside")),
        ("/records/0/mutantId", json!(r"..\outside")),
        ("/records/0/mutantId", json!("C:outside")),
    ] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = replacement;
        fixture.summary(&value);
        assert!(load(&fixture.0).is_err(), "{pointer}");
    }
    let mut value = original.clone();
    let duplicate = value["records"][0].clone();
    value["records"].as_array_mut().unwrap().push(duplicate);
    fixture.summary(&value);
    assert!(load(&fixture.0).is_err());
    fixture.put("summary.json", r#"{"version":1,"version":1}"#);
    assert!(load(&fixture.0).is_err());
    fixture.put("summary.json", vec![b' '; MAX_SUMMARY_BYTES + 1]);
    assert!(load(&fixture.0).is_err());
}

#[test]
fn root_profile_cannot_disagree_with_nested_records_or_claim_combined_score() {
    let fixture = Fixture::new();
    let original = profile(
        direct("classic", vec![record("classic", "one", "KILLED")]),
        direct("failure_mode", vec![]),
    );
    for pointer in ["/records/0/outcome", "/families/classic/status"] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = "cancelled".into();
        fixture.summary(&value);
        assert!(load(&fixture.0).is_err());
    }
    let mut value = original.clone();
    value["mutationScore"] = json!({"percent":100});
    fixture.summary(&value);
    assert!(load(&fixture.0).is_err());
}

#[test]
fn completed_profiles_require_completed_children_and_consistent_publication_flags() {
    let fixture = Fixture::new();
    let original = profile(direct("classic", vec![]), direct("failure_mode", vec![]));
    let mut incomplete = original.clone();
    incomplete["families"]["classic"]["status"] = "not_started".into();
    incomplete["families"]["classic"]["complete"] = false.into();
    incomplete["families"]["classic"]["summary"]["status"] = "not_started".into();
    incomplete["families"]["classic"]["summary"]["complete"] = false.into();
    fixture.summary(&incomplete);
    assert!(load(&fixture.0).is_err());
    let mut unpublished = original;
    unpublished["families"]["classic"]["summary"]["profileComplete"] = false.into();
    fixture.summary(&unpublished);
    assert!(load(&fixture.0).is_err());
    let mut nonempty = direct("classic", vec![record("classic", "one", "KILLED")]);
    nonempty["status"] = "no_mutants".into();
    fixture.summary(&nonempty);
    assert!(load(&fixture.0).is_err());
}

#[test]
fn optional_manifest_failure_or_different_identity_does_not_fail_export() {
    let fixture = Fixture::new();
    fixture.summary(&direct("classic", vec![]));
    for bytes in [
        b"not JSON".to_vec(),
        json!({"runId":"other","configuration":{"workers":8}})
            .to_string()
            .into_bytes(),
        vec![b' '; MAX_MANIFEST_BYTES + 1],
    ] {
        fixture.put("manifest.json", bytes);
        assert!(fixture.load()["configuration"].is_null());
    }
}

#[test]
fn capture_previews_are_bounded_and_invalid_utf8_respects_the_total_budget() {
    let fixture = Fixture::new();
    fixture.summary(&direct(
        "classic",
        vec![record("classic", "one", "SURVIVED")],
    ));
    fixture.put(
        "mutants/one/capture/stdout.bin",
        vec![b'a'; MAX_LOG_BYTES + 20],
    );
    let value = fixture.load();
    let log = &value["families"][0]["records"][0]["reportLogs"][0];
    assert_eq!(log["text"].as_str().unwrap().len(), MAX_LOG_BYTES);
    assert_eq!(log["truncated"], true);
    fixture.put("binary", vec![0xff; 100]);
    let mut budget = 17;
    let first = super::log(&fixture.0, &fixture.0.join("binary"), "binary", &mut budget);
    assert!(first["text"].as_str().unwrap().len() <= 17);
    assert_eq!(17 - first["text"].as_str().unwrap().len(), budget);
    let mut budget = 0;
    let exhausted = super::log(&fixture.0, &fixture.0.join("binary"), "binary", &mut budget);
    assert_eq!(exhausted["truncated"], true);
    assert!(exhausted["unavailable"].is_string());
}

#[cfg(unix)]
#[test]
fn capture_symlinks_cannot_escape_the_run_but_internal_summary_aliases_work() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let outside = Fixture::new();
    let summary = direct("classic", vec![record("classic", "one", "KILLED")]);
    fixture.put(".profile-reports/generation/root.json", summary.to_string());
    symlink(
        ".profile-reports/generation/root.json",
        fixture.0.join("summary.json"),
    )
    .unwrap();
    outside.put("stdout.bin", "SECRET MUST NOT BE EMBEDDED");
    fixture.put("mutants/one/capture/stderr.bin", "");
    symlink(
        outside.0.join("stdout.bin"),
        fixture.0.join("mutants/one/capture/stdout.bin"),
    )
    .unwrap();
    let value = fixture.load();
    let log = &value["families"][0]["records"][0]["reportLogs"][0];
    assert_eq!(log["text"], "");
    assert!(log["unavailable"].is_string());
    assert!(!value.to_string().contains("SECRET MUST NOT BE EMBEDDED"));
}

#[cfg(unix)]
#[test]
fn fifo_capture_is_unavailable_without_opening_a_blocking_stream() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    fixture.summary(&direct("classic", vec![record("classic", "one", "KILLED")]));
    fixture.put("mutants/one/capture/stderr.bin", "");
    let path = CString::new(
        fixture
            .0
            .join("mutants/one/capture/stdout.bin")
            .as_os_str()
            .as_bytes(),
    )
    .unwrap();
    // SAFETY: path is a terminated C string naming this test's unique temporary directory.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(
        fixture.load()["families"][0]["records"][0]["reportLogs"][0]["unavailable"].is_string()
    );
}
