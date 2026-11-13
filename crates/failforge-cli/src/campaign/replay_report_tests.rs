use super::*;

fn binding() -> Value {
    json!({"candidates":3,"operators":[
        {"id":"A","family":"classic","category":"Boolean"},
        {"id":"B","family":"classic","category":"Relational"},
        {"id":"C","family":"classic","category":"Relational"},
        {"id":"IGNORED","family":"failure_mode"}],
        "operatorTree":{"classic":{"Boolean":["A"],"Relational":["B","C"]}},
        "retainedCandidates":[{"mutantId":"a","operatorId":"A"},
            {"mutantId":"b","operatorId":"B"},{"mutantId":"c","operatorId":"B"}]})
}
fn summary() -> Value {
    json!({"complete":true,"mutationScore":{"percent":50.0},"records":[
        {"mutantId":"a","operatorId":"A","outcome":"KILLED","reason":"user_test_failure",
            "timeoutDiagnostics":{"kind":"inner_test_timeout"}},
        {"mutantId":"b","operatorId":"B","outcome":"SURVIVED","reason":"user_tests_passed"},
        {"mutantId":"c","operatorId":"B","outcome":"TIMEOUT","reason":"runner_timeout",
            "timeoutDiagnostics":{"kind":"outer_runner_timeout"}}]})
}
#[test]
fn complete_grouping_keeps_independent_counts_reasons_and_score() {
    let mut summary = summary();
    annotate(&mut summary, &binding()).unwrap();
    assert_eq!(
        summary["resultsByClassicalCategory"],
        json!({
        "Boolean":{"KILLED":1},"Relational":{"SURVIVED":1,"TIMEOUT":1}})
    );
    assert_eq!(
        summary["resultsByOperator"],
        json!({"A":{"KILLED":1},"B":{"SURVIVED":1,"TIMEOUT":1}})
    );
    assert_eq!(summary["withoutRetainedCandidates"], json!(["C"]));
    assert_eq!(
        summary["outcomeReasonCounts"],
        json!({
        "KILLED":{"user_test_failure":1},"SURVIVED":{"user_tests_passed":1},"TIMEOUT":{"runner_timeout":1}})
    );
    assert_eq!(
        summary["timeoutKindCounts"],
        json!({"inner_test_timeout":1,"outer_runner_timeout":1})
    );
    assert_eq!(summary["allDiscoveredMutantsProcessed"], true);
    assert_eq!(summary["staticValidationReused"], true);
    assert_eq!(summary["records"][1]["category"], "Relational");
    assert_eq!(summary["mutationScore"], json!({"percent":50.0}));
}
#[test]
fn partial_run_uses_retained_operators_even_before_their_first_result() {
    let mut report = summary();
    report["records"].as_array_mut().unwrap().truncate(1);
    report["complete"] = false.into();
    report["mutationScore"] = Value::Null;
    annotate(&mut report, &binding()).unwrap();
    assert_eq!(report["allDiscoveredMutantsProcessed"], false);
    assert_eq!(report["withoutRetainedCandidates"], json!(["C"]));
    assert_eq!(report["resultsByOperator"], json!({"A":{"KILLED":1}}));
    assert!(report["mutationScore"].is_null());
    // All facts may have arrived before cleanup fails: processed is factual,
    // independent of successful completion or score eligibility.
    let mut failed = summary();
    failed["complete"] = false.into();
    failed["mutationScore"] = Value::Null;
    annotate(&mut failed, &binding()).unwrap();
    assert_eq!(failed["allDiscoveredMutantsProcessed"], true);
    assert!(failed["mutationScore"].is_null());
}
#[test]
fn unknown_operators_mismatched_ids_and_duplicate_results_fail_closed() {
    for (pointer, value) in [
        ("/records/0/operatorId", json!("UNKNOWN")),
        ("/records/0/operatorId", json!("B")),
        ("/records/0/mutantId", json!("unknown")),
        ("/records/2/mutantId", json!("b")),
    ] {
        let mut summary = summary();
        *summary.pointer_mut(pointer).unwrap() = value;
        assert!(annotate(&mut summary, &binding()).is_err(), "{pointer}");
    }
    let mut bad = binding();
    bad["retainedCandidates"][0]["operatorId"] = "unknown".into();
    assert!(annotate(&mut summary(), &bad).is_err());
}
#[test]
fn missing_optional_category_and_tree_are_explicitly_unclassified() {
    let mut binding = binding();
    binding.as_object_mut().unwrap().remove("operatorTree");
    for row in binding["operators"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("category");
    }
    let mut summary = summary();
    annotate(&mut summary, &binding).unwrap();
    assert_eq!(
        summary["resultsByClassicalCategory"],
        json!({"unclassified":{"KILLED":1,"SURVIVED":1,"TIMEOUT":1}})
    );
    assert_eq!(summary["withoutRetainedCandidates"], json!(["C"]));
}
#[test]
fn empty_replay_and_non_timeout_diagnostics_do_not_invent_timeout_counts() {
    let mut metadata = binding();
    metadata["candidates"] = 0.into();
    metadata["retainedCandidates"] = json!([]);
    let mut empty = json!({"records":[],"complete":false,"mutationScore":null});
    annotate(&mut empty, &metadata).unwrap();
    assert_eq!(empty["allDiscoveredMutantsProcessed"], true);
    assert_eq!(empty["withoutRetainedCandidates"], json!(["A", "B", "C"]));
    assert_eq!(empty["timeoutKindCounts"], json!({}));
    let mut rows = summary();
    rows["records"][1]["timeoutDiagnostics"] = json!({"kind":"not_a_timeout"});
    rows["records"][2]["outcome"] = "INVALID".into();
    annotate(&mut rows, &binding()).unwrap();
    assert_eq!(rows["timeoutKindCounts"], json!({"inner_test_timeout":1}));
}

#[test]
fn ordinary_classic_discovery_reuses_projection_without_replay_claims() {
    let retained = binding();
    let discovery = json!({"steps":{"handshake":{"operators":retained["operators"],
        "operatorTree":retained["operatorTree"]}},"candidates":[
        {"mutant":{"id":"a","operatorId":"A"},"largeOpaqueEvidence":"not retained"},
        {"mutant":{"id":"b","operatorId":"B"}},{"mutant":{"id":"c","operatorId":"B"}}]});
    let metadata = discovered_metadata(&discovery).unwrap();
    assert_eq!(metadata, retained);
    let mut report = summary();
    report["records"].as_array_mut().unwrap().truncate(1);
    report["mutationScore"] = Value::Null;
    annotate_discovered(&mut report, &metadata).unwrap();
    assert_eq!(report["notApplicableOperators"], json!(["C"]));
    assert_eq!(
        report["resultsByClassicalCategory"],
        json!({"Boolean":{"KILLED":1}})
    );
    assert_eq!(report["allDiscoveredMutantsProcessed"], false);
    assert!(report.get("staticValidationReused").is_none());
    assert!(report.get("withoutRetainedCandidates").is_none());
    assert!(report["mutationScore"].is_null());
    assert!(discovered_metadata(&json!({"candidates":[]})).is_none());
}
