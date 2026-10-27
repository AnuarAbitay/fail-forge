use super::*;

fn healthy_profile_summary() -> Value {
    let record = json!({
        "mutantId":"candidate-a", "family":"classic", "operatorId":"BOOLEAN_LITERAL_FLIP",
        "file":"lib/a.dart", "edit":{"replacement":"false"},
        "outcome":"KILLED", "temporaryCopyCleaned":true, "runner":{"cleanup":"complete"}
    });
    let control = json!({
        "outcome":"SURVIVED", "temporaryCopyCleaned":true, "runner":{"cleanup":"complete"}
    });
    let score = json!({
        "family":"classic", "killed":1, "survived":0, "denominator":1,
        "excluded":{}, "percent":100.0
    });
    json!({
        "version":2, "kind":"profile", "output":"/tmp/public-regression-report",
        "complete":true, "status":"completed",
        "expectedMutants":1, "completedMutants":1, "outcomeCounts":{"KILLED":1},
        "originalProjectFilesUnchanged":true, "temporarySnapshotCleaned":true,
        "workspaceCleanupDeferred":false, "records":[record], "mutationScore":null,
        "failureModeScore":null, "languageSpecificScore":null,
        "families":{"classic":{"enabled":true,"status":"completed","summary":{
            "family":"classic", "status":"completed", "complete":true, "profileComplete":true,
            "records":[record], "baseline":control, "originalControls":[control],
            "mutationScore":score, "expectedMutants":1, "completedMutants":1,
            "outcomeCounts":{"KILLED":1}
        }}}
    })
}

fn terminal(summary: &Value) -> Vec<u8> {
    format!(
        "FailForge profile summary\n{}\n",
        Path::new(summary["output"].as_str().unwrap())
            .join("summary.json")
            .display()
    )
    .into_bytes()
}

#[test]
fn aggregate_cannot_disagree_with_family_verdict_even_when_each_score_is_consistent() {
    let mut summary = healthy_profile_summary();
    report_contract(&summary, &terminal(&summary), false).unwrap();
    let family = &mut summary["families"]["classic"]["summary"];
    family["records"][0]["outcome"] = "SURVIVED".into();
    family["outcomeCounts"] = json!({"SURVIVED":1});
    family["mutationScore"] = json!({
        "family":"classic", "killed":0, "survived":1, "denominator":1,
        "excluded":{}, "percent":0.0
    });
    assert!(
        report_contract(&summary, &terminal(&summary), false).is_err(),
        "individually consistent scores cannot authenticate conflicting verdicts"
    );
}

#[test]
fn aggregate_cannot_disagree_with_family_identity_or_patch_metadata() {
    for (field, changed) in [
        ("mutantId", json!("candidate-foreign")),
        ("operatorId", json!("THROW_REMOVAL")),
        ("file", json!("lib/other.dart")),
        ("edit", json!({"replacement":"true"})),
        ("family", json!("failure_mode")),
    ] {
        let mut summary = healthy_profile_summary();
        summary["families"]["classic"]["summary"]["records"][0][field] = changed;
        assert!(
            report_contract(&summary, &terminal(&summary), false).is_err(),
            "aggregate must retain family {field}"
        );
    }
}

#[test]
fn generated_applicability_map_does_not_excuse_invalid_or_misdirected_profile() {
    let temp = common::Temporary::new("ff-generated-contract").unwrap();
    let generation = Generation {
        file: "lib/a.dart".into(),
        families: vec!["classic".to_owned()],
        operators: vec!["BOOLEAN_LITERAL_FLIP".to_owned()],
    };
    let profile = json!({
        "mutations":{"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}}},
        "rules":{"include":["lib/a.dart"]}
    });
    generated_contract(&profile, &generation, &temp.0, &temp.0).unwrap();
    for include in [
        json!(["lib/other.dart"]),
        json!(["lib/a.dart", "lib/other.dart"]),
        json!(["lib/*.dart"]),
    ] {
        let mut wrong = profile.clone();
        wrong["rules"]["include"] = include;
        assert!(generated_contract(&wrong, &generation, &temp.0, &temp.0).is_err());
    }
    let mut unknown = profile.clone();
    unknown["silentlyIgnored"] = true.into();
    assert!(generated_contract(&unknown, &generation, &temp.0, &temp.0).is_err());
    let mut malformed = profile;
    malformed["workers"] = "four".into();
    assert!(generated_contract(&malformed, &generation, &temp.0, &temp.0).is_err());
}

fn cancelled_profile_with_completed_child() -> Value {
    let mut summary = healthy_profile_summary();
    summary["status"] = "cancelled".into();
    summary["complete"] = false.into();
    summary["expectedMutants"] = Value::Null;
    summary["mutationScore"] = Value::Null;
    let child = &mut summary["families"]["classic"]["summary"];
    child["profileComplete"] = false.into();
    child["mutationScore"] = Value::Null;
    summary["families"]["failure_mode"] = json!({
        "enabled":true, "status":"not_started", "complete":false,
        "summary":{"family":"failure_mode","status":"not_started",
            "complete":false,"profileComplete":false,"records":[],
            "completedMutants":0,"outcomeCounts":{},"baseline":null,
            "originalControls":[],"failureModeScore":null}
    });
    summary
}

#[test]
fn cancelled_profile_retains_completed_child_without_publishing_any_score() {
    let summary = cancelled_profile_with_completed_child();
    assert_eq!(summary["families"]["classic"]["summary"]["complete"], true);
    assert_eq!(
        summary["families"]["classic"]["summary"]["completedMutants"],
        1
    );
    report_contract(&summary, &terminal(&summary), true).unwrap();
}

#[test]
fn partial_child_or_enclosing_profile_cannot_publish_even_an_accurate_score() {
    let healthy = healthy_profile_summary();
    let score = healthy["families"]["classic"]["summary"]["mutationScore"].clone();
    let mut summary = cancelled_profile_with_completed_child();
    summary["families"]["classic"]["summary"]["mutationScore"] = score.clone();
    assert!(report_contract(&summary, &terminal(&summary), true).is_err());
    for complete in [false, true] {
        let mut child = healthy["families"]["classic"]["summary"].clone();
        child["complete"] = complete.into();
        child["profileComplete"] = false.into();
        assert!(score_contract(&child).is_err());
    }
    let mut direct = healthy["families"]["classic"]["summary"].clone();
    direct.as_object_mut().unwrap().remove("profileComplete");
    direct["complete"] = false.into();
    assert!(score_contract(&direct).is_err());
}

#[test]
fn completed_report_still_requires_score_and_cannot_forge_parent_completion() {
    let mut complete = healthy_profile_summary();
    complete["families"]["classic"]["summary"]["mutationScore"] = Value::Null;
    assert!(report_contract(&complete, &terminal(&complete), false).is_err());
    complete["families"]["classic"]["summary"]["profileComplete"] = false.into();
    assert!(report_contract(&complete, &terminal(&complete), false).is_err());

    let mut partial = cancelled_profile_with_completed_child();
    partial["families"]["classic"]["summary"]["profileComplete"] = true.into();
    assert!(report_contract(&partial, &terminal(&partial), true).is_err());
    for value in [Value::Null, json!(0), json!("false")] {
        let mut child = partial["families"]["classic"]["summary"].clone();
        child["profileComplete"] = value;
        assert!(score_contract(&child).is_err());
    }
}

fn native_profile(text: &str) -> (Value, Vec<u8>) {
    let summary: Value = serde_json::from_str(text).unwrap();
    let stdout = terminal(&summary);
    (summary, stdout)
}

#[test]
fn serialized_native_profiles_validate_three_families_and_zero_denominators() {
    for text in [
        include_str!("../tests/fixtures/native-profile-all-families.json"),
        include_str!("../tests/fixtures/native-profile-zero.json"),
    ] {
        let (summary, stdout) = native_profile(text);
        report_contract(&summary, &stdout, false).unwrap();
        for (family, field) in [
            ("classic", "mutationScore"),
            ("failure_mode", "failureModeScore"),
            ("language_specific", "languageSpecificScore"),
        ] {
            assert_eq!(summary[field], Value::Null);
            assert!(summary["families"][family]["summary"][field].is_object());
        }
    }
}

#[test]
fn serialized_native_profile_rejects_fabricated_aggregate_scores() {
    let (mut summary, stdout) = native_profile(include_str!(
        "../tests/fixtures/native-profile-all-families.json"
    ));
    for (family, field) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        summary[field] = summary["families"][family]["summary"][field].clone();
    }
    assert!(report_contract(&summary, &stdout, false).is_err());
}

#[test]
fn zero_denominator_profile_cannot_hide_a_selected_family_summary() {
    let (summary, stdout) =
        native_profile(include_str!("../tests/fixtures/native-profile-zero.json"));
    for family in ["classic", "failure_mode", "language_specific"] {
        let mut missing = summary.clone();
        missing["families"][family]["summary"] = Value::Null;
        assert!(report_contract(&missing, &stdout, false).is_err());
    }
}

#[test]
fn partial_profile_withholds_each_of_three_completed_native_family_scores() {
    let (mut summary, stdout) = native_profile(include_str!(
        "../tests/fixtures/native-profile-all-families.json"
    ));
    let healthy = summary.clone();
    summary["complete"] = false.into();
    summary["status"] = "cancelled".into();
    summary["expectedMutants"] = Value::Null;
    for (family, field) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        summary["families"][family]["summary"]["profileComplete"] = false.into();
        summary["families"][family]["summary"][field] = Value::Null;
    }
    report_contract(&summary, &stdout, true).unwrap();
    for (family, field) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        let mut forged = summary.clone();
        forged["families"][family]["summary"][field] =
            healthy["families"][family]["summary"][field].clone();
        assert!(report_contract(&forged, &stdout, true).is_err());
    }
}
