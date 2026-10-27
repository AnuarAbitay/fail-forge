use super::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const RUN: &str = "0123456789abcdef0123456789abcdef";
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    base: PathBuf,
    output: PathBuf,
    reports: Reports,
    families: Vec<String>,
}
impl Fixture {
    fn new(mode: &str, families: &[&str]) -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-reports-{}-{}-{}",
            std::process::id(),
            crate::campaign_publish::token().unwrap(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let output = base.join("output");
        #[cfg(unix)]
        fs::create_dir(&output).unwrap();
        #[cfg(windows)]
        {
            let parent = failforge_workspace::windows_fs::directory(&base).unwrap();
            drop(
                failforge_workspace::windows_fs::create_directory_at(
                    &parent,
                    output.file_name().unwrap(),
                )
                .unwrap(),
            );
        }
        if mode == "profile" {
            for family in families {
                fs::create_dir_all(output.join("families").join(family)).unwrap();
            }
        }
        let mut reports = Reports::new(
            mode,
            json!({"project":base.join("project"),"source":"lib/source.dart"}),
            families
                .iter()
                .map(|family| {
                    (
                        (*family).into(),
                        json!({"family":family,"project":base.join("project")}),
                    )
                })
                .collect(),
        )
        .unwrap();
        reports
            .bind_output(crate::campaign_publish_fs::walk(&output).unwrap())
            .unwrap();
        Self {
            base,
            output,
            reports,
            families: families.iter().map(|name| (*name).into()).collect(),
        }
    }
    fn prepared(&self) -> Value {
        json!({"kind":"prepared","runId":RUN,"output":self.output,
        "runtime":{"runner":"/bin/failforge","flutterSdk":null},"originalHashes":{"lib/source.dart":"abc"},
        "families":self.families.iter().map(|family|json!({"family":family,"rules":{"operators":["EXAMPLE"]}})).collect::<Vec<_>>()})
    }
    fn prepare(&mut self) {
        self.reports.event(&self.prepared()).unwrap();
    }
    fn event(&mut self, value: Value) -> io::Result<()> {
        self.reports
            .event(&json!({"kind":"campaign_event","event":value}))
    }
    fn discover(&mut self, total: usize) {
        self.reports
            .event(&json!({"kind":"bindings","value":{"hashes":{"source":"a"}}}))
            .unwrap();
        for family in self.families.clone() {
            self.event(json!({"kind":"discovery","family":family,"value":{"candidates":vec![Value::Null;total]}})).unwrap();
        }
        self.event(
            json!({"kind":"discovery_finished","expectedMutants":total*self.families.len()}),
        )
        .unwrap();
    }
    fn record(&mut self, family: &str, id: &str) -> io::Result<()> {
        self.event(json!({"kind":"family_event","family":family,"event":{"kind":"mutant","record":{
        "recordKind":"mutation","family":family,"mutantId":id,"operatorId":"EXAMPLE","outcome":"KILLED","reason":"test_failure","evidence":{"message":"полные факты","nested":[1,2,3]}}}}))
    }
    fn complete(&mut self, total: usize) {
        for family in self.families.clone() {
            self.event(json!({"kind":"family_started","family":family}))
                .unwrap();
            self.event(json!({"kind":"family_event","family":family,"event":{"kind":"baseline","attempt":{"opaque":"baseline"}}})).unwrap();
            for id in (0..total).rev() {
                self.record(&family, &format!("mutant-{id}")).unwrap();
            }
            self.event(json!({"kind":"family_event","family":family,"event":{"kind":"original_after","attempt":{"opaque":"after"}}})).unwrap();
            self.event(json!({"kind":"family_finished","family":family,"status":if total==0 {"no_mutants"} else {"completed"},"error":null,"wallSeconds":0.125})).unwrap();
        }
        self.cleanup();
    }
    fn cleanup(&mut self) {
        self.reports.event(&json!({"kind":"cleanup","originalUnchanged":true,"snapshotCleaned":true,"cleanupDeferred":false,"workspaceDirectory":null,"errors":[],"skipped":[]})).unwrap();
    }
    fn read(&self, path: &str) -> Value {
        serde_json::from_slice(&fs::read(self.output.join(path)).unwrap()).unwrap()
    }
    fn assert_generation(&self, prefix: &str) {
        #[cfg(unix)]
        let generation = {
            let relative = fs::read_link(self.output.join(".profile-reports-current")).unwrap();
            assert!(
                relative
                    .to_str()
                    .unwrap()
                    .starts_with(&format!(".profile-reports/{prefix}"))
            );
            self.output.join(relative)
        };
        #[cfg(windows)]
        let generation = {
            assert!(!self.output.join(".profile-reports-current").exists());
            let matching: Vec<_> = fs::read_dir(self.output.join(".profile-reports"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with(prefix)
                })
                .collect();
            assert_eq!(matching.len(), 1);
            matching.into_iter().next().unwrap()
        };
        for relative in std::iter::once("summary.json".to_owned()).chain(
            self.families
                .iter()
                .map(|family| format!("families/{family}/summary.json")),
        ) {
            let public = self.output.join(&relative);
            #[cfg(windows)]
            {
                let metadata = fs::symlink_metadata(&public).unwrap();
                assert!(metadata.is_file());
                assert!(!failforge_workspace::windows_fs::is_reparse(&metadata));
            }
            assert_eq!(
                fs::read(public).unwrap(),
                fs::read(generation.join(relative)).unwrap()
            );
        }
    }
}
fn rename_directory(from: &Path, to: &Path) -> bool {
    #[cfg(unix)]
    {
        fs::rename(from, to).unwrap();
        true
    }
    #[cfg(windows)]
    {
        let directory = failforge_workspace::windows_fs::directory(from).unwrap();
        let parent = failforge_workspace::windows_fs::directory(to.parent().unwrap()).unwrap();
        match failforge_workspace::windows_fs::rename_handle(
            &directory,
            &parent,
            to.file_name().unwrap(),
            false,
        ) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                // NTFS rejects renaming parents of retained open files. The
                // failed attack must leave both the identity and name intact.
                let named = failforge_workspace::windows_fs::directory(from).unwrap();
                assert_eq!(
                    failforge_workspace::windows_fs::identity(&directory).unwrap(),
                    failforge_workspace::windows_fs::identity(&named).unwrap()
                );
                assert!(!to.exists());
                false
            }
            Err(error) => panic!("directory substitution failed unexpectedly: {error}"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn retained_replay_keeps_binding_and_both_original_controls_in_final_publication() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    let binding = json!({"transcriptSha256":"retained-digest","staticValidationReused":true,
        "operators":[{"id":"EXAMPLE","family":"classic"}],"candidates":1,
        "retainedCandidates":[{"mutantId":"mutant-0","operatorId":"EXAMPLE"}]});
    let mut prepared = fixture.prepared();
    prepared["retainedReplay"] = binding.clone();
    fixture.reports.event(&prepared).unwrap();
    fixture.discover(1);
    fixture
        .event(json!({"kind":"family_event","family":"classic","event":{
        "kind":"original_before","attempt":{"opaque":"second-original-before"}}}))
        .unwrap();
    fixture
        .event(json!({"kind":"family_started","family":"classic"}))
        .unwrap();
    fixture
        .event(json!({"kind":"family_event","family":"classic","event":{
        "kind":"baseline","attempt":{"opaque":"baseline"}}}))
        .unwrap();
    fixture
        .event(json!({"kind":"family_event","family":"classic","event":{
        "kind":"mutant","record":{"recordKind":"mutation","family":"classic","mutantId":"mutant-0",
            "operatorId":"EXAMPLE","outcome":"KILLED","reason":"test_failure"}}}))
        .unwrap();
    fixture
        .event(json!({"kind":"family_event","family":"classic","event":{
        "kind":"original_after","attempt":{"opaque":"after"}}}))
        .unwrap();
    fixture
        .event(
            json!({"kind":"family_finished","family":"classic","status":"completed",
        "error":null,"wallSeconds":0.125}),
        )
        .unwrap();
    fixture.cleanup();
    let result = fixture.reports.finish("completed", None).unwrap();
    assert_eq!(result.summary["retainedReplay"], binding);
    assert_eq!(result.summary["staticValidationReused"], true);
    assert_eq!(result.summary["allDiscoveredMutantsProcessed"], true);
    assert_eq!(
        result.summary["resultsByClassicalCategory"],
        json!({"unclassified":{"KILLED":1}})
    );
    assert_eq!(
        result.summary["resultsByOperator"],
        json!({"EXAMPLE":{"KILLED":1}})
    );
    assert_eq!(
        result.summary["originalControls"],
        json!([
        {"opaque":"second-original-before"}, {"opaque":"after"}])
    );
    assert_eq!(fixture.read("replay-binding.json"), binding);
    assert_eq!(fixture.read("manifest.json")["retainedReplay"], binding);
    assert_eq!(fixture.read("summary.json"), result.summary);
}

#[test]
fn failed_replay_control_is_retained_without_a_mutation_score() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    fixture.discover(1);
    fixture
        .event(json!({"kind":"family_event","family":"classic","event":{
        "kind":"original_before","attempt":{"outcome":"NO_TESTS","diagnostic":"retained"}}}))
        .unwrap();
    fixture.cleanup();
    let result = fixture
        .reports
        .finish("failed", Some("original control did not pass"))
        .unwrap();
    assert_eq!(result.summary["complete"], false);
    assert!(result.summary["mutationScore"].is_null());
    assert_eq!(
        result.summary["originalControls"][0]["diagnostic"],
        "retained"
    );
    assert_eq!(result.summary["records"], json!([]));
}

#[test]
fn classic_discovery_groups_survive_direct_and_profile_publication() {
    for mode in ["direct", "profile"] {
        let mut fixture = Fixture::new(mode, &["classic"]);
        fixture.prepare();
        fixture
            .reports
            .event(&json!({"kind":"bindings","value":{"hashes":{}}}))
            .unwrap();
        fixture
            .event(json!({"kind":"discovery","family":"classic","value":{
            "steps":{"handshake":{"operators":[
                {"id":"EXAMPLE","family":"classic","category":"Boolean"},
                {"id":"UNUSED","family":"classic","category":"Relational"}]}},
            "candidates":[{"mutant":{"id":"mutant-0","operatorId":"EXAMPLE"}}]}}))
            .unwrap();
        fixture
            .event(json!({"kind":"discovery_finished","expectedMutants":1}))
            .unwrap();
        fixture.complete(1);
        let result = fixture.reports.finish("completed", None).unwrap();
        let summary = if mode == "profile" {
            &result.summary["families"]["classic"]["summary"]
        } else {
            &result.summary
        };
        assert_eq!(
            summary["resultsByClassicalCategory"],
            json!({"Boolean":{"KILLED":1}})
        );
        assert_eq!(
            summary["resultsByOperator"],
            json!({"EXAMPLE":{"KILLED":1}})
        );
        assert_eq!(summary["notApplicableOperators"], json!(["UNUSED"]));
        assert_eq!(
            summary["outcomeReasonCounts"],
            json!({"KILLED":{"test_failure":1}})
        );
        assert_eq!(summary["allDiscoveredMutantsProcessed"], true);
        assert!(summary.get("staticValidationReused").is_none());
        assert_eq!(
            fixture.read(if mode == "profile" {
                "families/classic/summary.json"
            } else {
                "summary.json"
            }),
            *summary
        );
    }
}

#[test]
fn validates_configuration_and_requires_retained_output() {
    for (mode, families) in [
        ("unknown", vec!["classic"]),
        ("direct", vec![]),
        ("direct", vec!["classic", "failure_mode"]),
        ("profile", vec!["unknown"]),
    ] {
        assert!(
            Reports::new(
                mode,
                json!({}),
                families
                    .into_iter()
                    .map(|name| (name.into(), json!({})))
                    .collect()
            )
            .is_err()
        );
    }
    let fixture = Fixture::new("direct", &["classic"]);
    let mut reports = Reports::new(
        "direct",
        json!({}),
        BTreeMap::from([("classic".into(), json!({}))]),
    )
    .unwrap();
    assert!(
        reports
            .event(&fixture.prepared())
            .unwrap_err()
            .to_string()
            .contains("capability")
    );
    assert!(reports.failure().is_some());
}
#[test]
fn duplicate_preparation_and_changed_selection_are_rejected() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    let mut prepared = fixture.prepared();
    prepared["families"][0]["family"] = "failure_mode".into();
    assert!(fixture.reports.event(&prepared).is_err());
    assert!(!fixture.reports.prepared());
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    assert!(fixture.reports.event(&fixture.prepared()).is_err());
    assert!(
        fixture
            .reports
            .bind_output(crate::campaign_publish_fs::walk(&fixture.output).unwrap())
            .is_err()
    );
}
#[test]
fn direct_metadata_initial_report_and_full_final_evidence_are_native() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    let manifest = fixture.read("manifest.json");
    assert_eq!(manifest["executionEngine"], "rust");
    assert_eq!(manifest["runner"], "/bin/failforge");
    assert_eq!(manifest["effectiveRules"]["operators"], json!(["EXAMPLE"]));
    assert_eq!(manifest["configuration"]["flutter_sdk"], Value::Null);
    assert!(manifest.get("adapterHelper").is_none());
    fixture.discover(2);
    assert_eq!(fixture.read("summary.json")["status"], "running");
    assert!(fixture.read("summary.json")["mutationScore"].is_null());
    fixture.complete(2);
    let result = fixture.reports.finish("completed", None).unwrap();
    assert_eq!(result.summary["completedMutants"], 2);
    assert_eq!(result.summary["mutationScore"]["percent"], 100.0);
    assert_eq!(result.summary["records"][0]["mutantId"], "mutant-0");
    assert_eq!(
        result.summary["records"][0]["evidence"]["message"],
        "полные факты"
    );
    assert_eq!(result.summary["baseline"], json!({"opaque":"baseline"}));
    assert_eq!(
        result.summary["originalControls"],
        json!([{"opaque":"after"}])
    );
    assert!(result.terminal.contains("KILLED"));
    assert_eq!(fixture.read("summary.json"), result.summary);
    assert_eq!(fixture.read("progress.json")["intendedStatus"], "completed");
    assert!(fixture.reports.finish("completed", None).is_err());
    assert!(fixture.reports.event(&json!({"kind":"cleanup"})).is_err());
}
#[test]
fn profile_layout_and_all_family_scores_commit_together() {
    let mut fixture = Fixture::new("profile", &FAMILIES);
    fixture.prepare();
    assert_eq!(fixture.read("manifest.json")["kind"], "profile");
    for family in FAMILIES {
        assert_eq!(
            fixture.read(&format!("families/{family}/manifest.json"))["profileRunId"],
            RUN
        );
    }
    fixture.discover(1);
    fixture.assert_generation("pending");
    for family in FAMILIES {
        assert_eq!(
            fixture.read(&format!("families/{family}/input-bindings.json")),
            fixture.read("input-bindings.json")
        );
    }
    fixture.complete(1);
    let summary = fixture.reports.finish("completed", None).unwrap().summary;
    assert_eq!(summary["completedMutants"], 3);
    assert!(summary["mutationScore"].is_null());
    fixture.assert_generation("final-");
    for (family, score) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        let child = fixture.read(&format!("families/{family}/summary.json"));
        assert_eq!(child, summary["families"][family]["summary"]);
        assert_eq!(child[score]["percent"], 100.0);
        assert_eq!(child["profileComplete"], true);
    }
}
#[test]
fn journal_sequence_and_utf8_fields_remain_readable() {
    let mut fixture = Fixture::new("direct", &["language_specific"]);
    fixture.prepare();
    fixture
        .reports
        .event(&json!({"kind":"stage_started","stage":"snapshot"}))
        .unwrap();
    fixture
        .reports
        .event(&json!({"kind":"stage_finished","stage":"snapshot","wallSeconds":0.25}))
        .unwrap();
    fixture.reports.event(&json!({"kind":"workspace","directory":fixture.base.join("workspace"),"recovered":["old"],"skipped":[{"name":"имя","nameBytes":[1],"reason":"locked"}]})).unwrap();
    fixture.discover(1);
    fixture.complete(1);
    let summary = fixture.reports.finish("completed", None).unwrap().summary;
    assert_eq!(summary["timings"]["snapshot"], 0.25);
    assert_eq!(
        summary["workspaceRecovery"]["skipped"],
        json!([{"name":"имя","reason":"locked"}])
    );
    let text = fs::read_to_string(fixture.output.join("events.jsonl")).unwrap();
    let rows = text
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["sequence"], index + 1);
        assert_eq!(row["version"], 1);
        assert_eq!(row["runId"], RUN);
        assert!(row["systemNs"].as_u64().unwrap() > 0);
        assert!(row["utc"].as_str().unwrap().ends_with("+00:00"));
    }
    assert_eq!(rows.last().unwrap()["kind"], "campaign_finished");
    assert!(
        rows.iter()
            .any(|row| row["kind"] == "mutant_completed" && row["completed"] == 1)
    );
    assert!(rows.iter().all(|row| row["kind"] != "native_completed"));
}
#[test]
fn timing_targets_distinguish_direct_profile_and_global_stages() {
    for mode in ["direct", "profile"] {
        let mut fixture = Fixture::new(mode, &["classic"]);
        fixture.prepare();
        fixture.discover(0);
        fixture
            .event(json!({"kind":"stage_finished","stage":"global","wallSeconds":1.0}))
            .unwrap();
        for _ in 0..2 {
            fixture.event(json!({"kind":"family_event","family":"classic","event":{"kind":"stage_finished","stage":"tests","wallSeconds":0.5}})).unwrap();
        }
        fixture.complete(0);
        let summary = fixture.reports.finish("no_mutants", None).unwrap().summary;
        assert_eq!(summary["timings"]["global"], 1.0);
        if mode == "direct" {
            assert_eq!(summary["timings"]["tests"], 1.0);
        } else {
            assert_eq!(
                summary["families"]["classic"]["summary"]["timings"]["tests"],
                1.0
            );
            assert!(summary["timings"].get("tests").is_none());
        }
    }
}
#[test]
fn persistent_progress_failure_publishes_partial_without_touching_foreign_directory() {
    let mut fixture = Fixture::new("profile", &["classic"]);
    fixture.prepare();
    fixture.discover(1);
    fixture.complete(1);
    fs::create_dir(fixture.output.join("progress.json")).unwrap();
    fs::write(fixture.output.join("progress.json/sentinel"), "foreign").unwrap();
    let summary = fixture.reports.finish("completed", None).unwrap().summary;
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["complete"], false);
    assert_eq!(summary["completedMutants"], 1);
    assert!(summary["families"]["classic"]["summary"]["mutationScore"].is_null());
    assert!(
        summary["error"]
            .as_str()
            .unwrap()
            .contains("report publication failed")
    );
    assert_eq!(
        fs::read_to_string(fixture.output.join("progress.json/sentinel")).unwrap(),
        "foreign"
    );
    assert_eq!(fixture.read("summary.json"), summary);
    fixture.assert_generation("partial-");
}
#[test]
fn journal_failure_latches_and_keeps_late_worker_records() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    fixture.discover(2);
    fixture
        .event(json!({"kind":"family_started","family":"classic"}))
        .unwrap();
    fs::rename(
        fixture.output.join("events.jsonl"),
        fixture.output.join("old-journal"),
    )
    .unwrap();
    fs::create_dir(fixture.output.join("events.jsonl")).unwrap();
    fs::write(fixture.output.join("events.jsonl/sentinel"), "foreign").unwrap();
    assert!(fixture.record("classic", "first").is_err());
    let original = fixture.reports.failure().unwrap().to_owned();
    fixture.record("classic", "second").unwrap();
    fixture.cleanup();
    let summary = fixture
        .reports
        .finish("cancelled", Some("cancelled after report fault"))
        .unwrap()
        .summary;
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["error"], original);
    assert_eq!(summary["completedMutants"], 2);
    assert!(summary["mutationScore"].is_null());
    assert_eq!(
        fs::read_to_string(fixture.output.join("events.jsonl/sentinel")).unwrap(),
        "foreign"
    );
}
#[cfg(unix)]
#[test]
fn final_journal_failure_overrides_success_but_preserves_execution_failure() {
    for status in ["completed", "failed"] {
        let mut fixture = Fixture::new("direct", &["classic"]);
        fixture.prepare();
        fixture.discover(1);
        fixture.complete(1);
        fs::rename(
            fixture.output.join("events.jsonl"),
            fixture.output.join("original-journal"),
        )
        .unwrap();
        symlink("/dev/null", fixture.output.join("events.jsonl")).unwrap();
        let summary = fixture
            .reports
            .finish(status, (status == "failed").then_some("execution failure"))
            .unwrap()
            .summary;
        assert_eq!(summary["status"], "failed");
        assert!(summary["mutationScore"].is_null());
        if status == "failed" {
            assert_eq!(summary["error"], "execution failure");
        } else {
            assert!(
                summary["error"]
                    .as_str()
                    .unwrap()
                    .contains("campaign report publication")
            );
        }
        assert_eq!(
            fs::read_link(fixture.output.join("events.jsonl")).unwrap(),
            PathBuf::from("/dev/null")
        );
    }
}
#[test]
fn duplicate_result_is_rejected_without_destroying_retained_facts() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    fixture.discover(2);
    fixture.record("classic", "one").unwrap();
    assert!(fixture.record("classic", "one").is_err());
    fixture.record("classic", "two").unwrap();
    fixture.cleanup();
    let summary = fixture
        .reports
        .finish("failed", Some("duplicate result"))
        .unwrap()
        .summary;
    assert_eq!(summary["completedMutants"], 2);
    assert_eq!(summary["records"][0]["mutantId"], "one");
}
#[test]
fn initial_manifest_collision_preserves_foreign_file_and_finalizes_failure() {
    for mode in ["direct", "profile"] {
        let mut fixture = Fixture::new(mode, &["classic"]);
        let manifest = if mode == "profile" {
            "families/classic/manifest.json"
        } else {
            "manifest.json"
        };
        fs::write(fixture.output.join(manifest), "foreign").unwrap();
        assert!(fixture.reports.event(&fixture.prepared()).is_err());
        fixture.cleanup();
        let summary = fixture.reports.finish("cancelled", None).unwrap().summary;
        assert_eq!(summary["status"], "failed");
        assert_eq!(summary["expectedMutants"], Value::Null);
        assert_eq!(
            fs::read_to_string(fixture.output.join(manifest)).unwrap(),
            "foreign"
        );
        assert_eq!(fixture.read("summary.json"), summary);
    }
}
#[test]
fn failed_profile_layout_uses_incomplete_root_only_report() {
    let mut fixture = Fixture::new("profile", &["classic"]);
    fs::create_dir(fixture.output.join(".profile-reports")).unwrap();
    fs::write(fixture.output.join(".profile-reports/sentinel"), "foreign").unwrap();
    assert!(fixture.reports.event(&fixture.prepared()).is_err());
    fixture.cleanup();
    let summary = fixture
        .reports
        .finish("failed", Some("layout failure"))
        .unwrap()
        .summary;
    assert_eq!(summary["kind"], "profile");
    assert_eq!(summary["complete"], false);
    assert_eq!(fixture.read("summary.json"), summary);
    assert_eq!(
        fs::read_to_string(fixture.output.join(".profile-reports/sentinel")).unwrap(),
        "foreign"
    );
}
#[test]
fn replacement_report_root_is_never_written() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    fixture.discover(1);
    fixture.complete(1);
    if !rename_directory(&fixture.output, &fixture.base.join("held-output")) {
        let result = fixture.reports.finish("completed", None).unwrap();
        assert_eq!(result.summary["complete"], true);
        assert_eq!(fixture.read("summary.json"), result.summary);
        return;
    }
    fs::create_dir(&fixture.output).unwrap();
    fs::write(fixture.output.join("sentinel"), "foreign").unwrap();
    assert!(fixture.reports.finish("completed", None).is_err());
    assert_eq!(fs::read_dir(&fixture.output).unwrap().count(), 1);
}
#[test]
fn replacement_family_directory_cannot_receive_metadata_or_final_report() {
    let mut fixture = Fixture::new("profile", &["classic"]);
    fixture.prepare();
    if !rename_directory(
        &fixture.output.join("families/classic"),
        &fixture.output.join("families/held"),
    ) {
        fixture.discover(0);
        fixture.complete(0);
        let summary = fixture.reports.finish("no_mutants", None).unwrap().summary;
        assert_eq!(summary["complete"], true);
        assert_eq!(
            fixture.read("families/classic/summary.json"),
            summary["families"]["classic"]["summary"]
        );
        return;
    }
    fs::create_dir(fixture.output.join("families/classic")).unwrap();
    assert!(
        fixture
            .reports
            .event(&json!({"kind":"bindings","value":{}}))
            .is_err()
    );
    fixture.cleanup();
    assert!(
        fixture
            .reports
            .finish("failed", Some("replacement"))
            .is_err()
    );
    assert_eq!(
        fs::read_dir(fixture.output.join("families/classic"))
            .unwrap()
            .count(),
        0
    );
}
#[test]
fn invalid_timing_is_rejected_without_serializing_nonfinite_values() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    assert!(
        fixture
            .reports
            .event(&json!({"kind":"stage_finished","stage":"bad","wallSeconds":-1}))
            .is_err()
    );
    fixture.cleanup();
    let summary = fixture
        .reports
        .finish("failed", Some("invalid timing"))
        .unwrap()
        .summary;
    assert!(summary["timings"].get("bad").is_none());
}
#[test]
fn report_files_are_private() {
    let mut fixture = Fixture::new("profile", &["classic"]);
    fixture.prepare();
    fixture.discover(0);
    fixture.complete(0);
    fixture.reports.finish("no_mutants", None).unwrap();
    for path in [
        "manifest.json",
        "input-bindings.json",
        "events.jsonl",
        "summary.json",
        "progress.json",
        "families/classic/summary.json",
    ] {
        #[cfg(windows)]
        failforge_workspace::windows_fs::require_private(
            &failforge_workspace::windows_fs::regular(&fixture.output.join(path), false).unwrap(),
            false,
        )
        .unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(fixture.output.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn full_ten_thousand_mutant_report_uses_compact_projection_and_exact_sorted_totals() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    fixture.discover(10_000);
    let run = fixture.reports.families.get_mut("classic").unwrap();
    for index in (0..10_000).rev() {
        run.record(
            "classic",
            &json!({"recordKind":"mutation","family":"classic",
            "mutantId":format!("{index:064x}"),"outcome":"KILLED","reason":"test_failure",
            "opaque":{"capture":"retained","catalogue":[1,2,3]}}),
        )
        .unwrap();
    }
    let projected = run.projected("classic");
    assert!(serde_json::to_vec(&projected).unwrap().len() < 2 * 1024 * 1024);
    assert!(
        projected["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("opaque").is_none())
    );
    run.status = "completed".into();
    fixture.cleanup();
    let summary = fixture.reports.summary("completed", None).unwrap();
    assert_eq!(summary["completedMutants"], 10_000);
    assert_eq!(summary["outcomeCounts"], json!({"KILLED":10_000}));
    assert_eq!(summary["mutationScore"]["percent"], 100.0);
    assert_eq!(summary["records"][0]["mutantId"], format!("{:064x}", 0));
    assert_eq!(
        summary["records"][9999]["mutantId"],
        format!("{:064x}", 9999)
    );
    assert_eq!(
        summary["records"][9999]["opaque"]["catalogue"],
        json!([1, 2, 3])
    );
}

#[test]
fn opaque_evidence_larger_than_private_request_limit_is_hydrated_locally() {
    let mut fixture = Fixture::new("direct", &["classic"]);
    fixture.prepare();
    fixture.discover(1);
    let size = 64 * 1024 * 1024 + 1;
    let mut record = json!({"recordKind":"mutation","family":"classic","mutantId":"large",
        "outcome":"KILLED","reason":"test_failure"});
    record["opaque"] = Value::String("x".repeat(size));
    // Move the retained fixture payload once. Production record ingestion is
    // covered separately; this case isolates final projection and hydration.
    let run = fixture.reports.families.get_mut("classic").unwrap();
    run.records.push(record);
    run.ids.insert("large".into());
    assert!(serde_json::to_vec(&run.projected("classic")).unwrap().len() < 1024);
    let summary = fixture
        .reports
        .summary("failed", Some("interrupted"))
        .unwrap();
    let payload = summary["records"][0]["opaque"].as_str().unwrap();
    assert_eq!(payload.len(), size);
    assert!(payload.bytes().all(|byte| byte == b'x'));
    assert_eq!(summary["completedMutants"], 1);
    assert!(summary["mutationScore"].is_null());
}
