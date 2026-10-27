use super::*;
use crate::ReporterMode;
use crate::test_catalog::{analyze_catalogue, apply_selection, parse_selection_context};

fn emit(events: &mut Vec<Value>, kind: &str, mut fields: Value) {
    fields["type"] = json!(kind);
    fields["time"] = json!(events.len());
    events.push(fields);
}
fn test(
    events: &mut Vec<Value>,
    identity: (u64, &str, u64, &[u64], Option<u64>),
    hidden: bool,
    failed: bool,
) {
    let (id, name, suite, groups, line) = identity;
    let url = line.map(|_| {
        format!(
            "file:///work/test/{}.dart",
            if suite == 0 { "a" } else { "b" }
        )
    });
    emit(
        events,
        "testStart",
        json!({"test":{"id":id,"name":name,"suiteID":suite,"groupIDs":groups,"metadata":{"skip":false},"line":line,"column":line.map(|_|3),"url":url}}),
    );
    if failed {
        emit(
            events,
            "error",
            json!({"testID":id,"error":"'package:subject/loader.dart': Failed assertion: line 74 pos 12: 'valid': is not true.","stackTrace":"dart:core _AssertionError._throwNew\npackage:subject/loader.dart 74:12 Loader.load\ntest/a.dart 25:12 main.<fn>\n","isFailure":false}),
        );
    }
    emit(
        events,
        "testDone",
        json!({"testID":id,"result":if failed{"error"}else{"success"},"skipped":false,"hidden":hidden}),
    );
}
fn events(failed: bool, ordinary: bool) -> Vec<Value> {
    let mut rows = Vec::new();
    emit(
        &mut rows,
        "start",
        json!({"protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":100}),
    );
    emit(
        &mut rows,
        "suite",
        json!({"suite":{"id":0,"platform":"vm","path":"test/a.dart"}}),
    );
    emit(
        &mut rows,
        "suite",
        json!({"suite":{"id":1,"platform":"vm","path":"test/b.dart"}}),
    );
    emit(
        &mut rows,
        "group",
        json!({"group":{"id":2,"suiteID":0,"parentID":null,"name":"","testCount":4,"metadata":{"skip":false},"line":null,"column":null,"url":null}}),
    );
    test(
        &mut rows,
        (3, "(setUpAll)", 0, &[2], Some(24)),
        !failed,
        failed,
    );
    if !failed {
        test(&mut rows, (4, "same", 0, &[2], Some(30)), false, false);
        test(&mut rows, (5, "same", 0, &[2], Some(40)), false, false);
        emit(
            &mut rows,
            "group",
            json!({"group":{"id":6,"suiteID":0,"parentID":2,"name":"nested","testCount":2,"metadata":{"skip":false},"line":50,"column":3,"url":"file:///work/test/a.dart"}}),
        );
        test(
            &mut rows,
            (7, "nested (setUpAll)", 0, &[2, 6], Some(51)),
            true,
            false,
        );
        test(
            &mut rows,
            (8, "nested same", 0, &[2, 6], Some(60)),
            false,
            false,
        );
        test(
            &mut rows,
            (9, "nested same", 0, &[2, 6], Some(70)),
            false,
            false,
        );
        test(
            &mut rows,
            (10, "nested (tearDownAll)", 0, &[2, 6], None),
            true,
            false,
        );
    }
    test(&mut rows, (11, "(tearDownAll)", 0, &[2], None), true, false);
    emit(
        &mut rows,
        "group",
        json!({"group":{"id":12,"suiteID":1,"parentID":null,"name":"","testCount":1,"metadata":{"skip":false},"line":null,"column":null,"url":null}}),
    );
    test(&mut rows, (13, "outside", 1, &[12], Some(10)), false, false);
    if ordinary {
        let at = rows
            .iter()
            .position(|row| row["type"] == "testDone" && row["testID"] == 13)
            .unwrap();
        rows.insert(at,json!({"type":"error","testID":13,"error":"StateError: changed subject","stackTrace":"package:subject/loader.dart 74:12 Loader.load\ntest/b.dart 11:7 main.<fn>\n","isFailure":false}));
        rows[at + 1]["result"] = json!("error");
    }
    emit(&mut rows, "allSuites", json!({"count":2}));
    emit(&mut rows, "done", json!({"success":!failed}));
    retime(&mut rows);
    rows
}
fn retime(events: &mut [Value]) {
    for (i, row) in events.iter_mut().enumerate() {
        row["time"] = json!(i);
    }
}
fn raw(rows: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row).unwrap();
        bytes.push(b'\n');
    }
    bytes
}
fn snapshot(rows: &[Value]) -> CatalogSnapshot {
    analyze_catalogue(&raw(rows), Path::new("/work"), ReporterMode::Dart).unwrap()
}
fn numeric_metadata_capture(failed: bool, token: &str) -> Vec<u8> {
    let mut rows = events(failed, failed);
    let test = rows
        .iter_mut()
        .find(|row| row["type"] == "testStart" && row["test"]["id"] == 13)
        .unwrap();
    test["test"]["metadata"]["identity"] = json!("NUMBER_TOKEN");
    String::from_utf8(raw(&rows))
        .unwrap()
        .replace("\"NUMBER_TOKEN\"", token)
        .into_bytes()
}
fn numeric_metadata_snapshot(failed: bool, token: &str) -> Result<CatalogSnapshot, CatalogError> {
    analyze_catalogue(
        &numeric_metadata_capture(failed, token),
        Path::new("/work"),
        ReporterMode::Dart,
    )
}
fn fixture(ordinary: bool) -> (CatalogSnapshot, SelectionContext) {
    let baseline = snapshot(&events(false, false));
    let current = snapshot(&events(true, ordinary));
    let context = SelectionContext {
        expected: Some(baseline.identities.clone()),
        baseline: Some(baseline),
        mutant: Some(json!({"file":"lib/event.dart"})),
        alias: "subject".into(),
        initialization: None,
    };
    (current, context)
}

#[test]
fn suppression_explains_duplicate_names_and_descendant_hooks_without_claiming_kill() {
    let (current, context) = fixture(false);
    let proof = explain_setup_suppression(&current, &context).unwrap();
    assert_eq!(proof["recommendedOutcome"], "UNCLASSIFIED");
    assert_eq!(proof["catalogueIdentityProven"], false);
    assert_eq!(proof["missingEntries"], 6);
    assert_eq!(proof["expectedEntries"], 9);
    assert_eq!(proof["observedEntries"], 3);
    assert_eq!(
        proof["missingIdentities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "same")
            .unwrap()["count"],
        2
    );
    assert_eq!(
        proof["baselineRawSha256"],
        context.baseline.as_ref().unwrap().raw_sha256
    );
    assert_eq!(proof["rawSha256"], current.raw_sha256);
    assert!(ordinary_failure_witness(&current, &context).is_none());
}

#[test]
fn separately_observed_ordinary_failure_preserves_incomplete_catalogue() {
    let (current, context) = fixture(true);
    let witness = ordinary_failure_witness(&current, &context).unwrap();
    assert_eq!(witness["suite"], "test/b.dart");
    assert_eq!(witness["name"], "outside");
    assert_eq!(witness["location"], json!([10, 3, "test/b.dart"]));
    assert_eq!(witness["witnessIdentityProven"], true);
    assert_eq!(witness["catalogueIdentityProven"], false);
    assert_eq!(witness["baselineTestID"], 13);
    assert_eq!(witness["rawTestID"], 13);
    let mut result = json!({"outcome":"KILLED","reason":"user_test_failure"});
    apply_selection(&mut result, &current, &context).unwrap();
    assert_eq!(result["outcome"], "KILLED");
    assert_eq!(
        result["reason"],
        "ordinary_author_failure_with_setup_suppression"
    );
    assert_eq!(result["observedOutcome"], "KILLED");
    assert_eq!(result["testIdentities"], json!(current.identities));
    assert_ne!(result["testIdentities"], json!(context.expected));
}

fn native_reporter_rows(ordinary: bool) -> Vec<Value> {
    let mut rows = events(true, ordinary);
    for row in &mut rows {
        if row["type"] == "suite" {
            row["suite"]["path"] = json!(
                row["suite"]["path"]
                    .as_str()
                    .unwrap()
                    .replace('/', std::path::MAIN_SEPARATOR_STR)
            );
        } else if row["type"] == "error" {
            // Package URIs keep `/`; native reporter author frames use the
            // platform separator, including `test\a.dart` on Windows.
            row["stackTrace"] = json!(
                row["stackTrace"]
                    .as_str()
                    .unwrap()
                    .replace("test/", &format!("test{}", std::path::MAIN_SEPARATOR))
            );
        }
    }
    rows
}

#[test]
fn native_relative_reporter_frames_preserve_setup_classification() {
    let (_, context) = fixture(false);
    for ordinary in [false, true] {
        let current = snapshot(&native_reporter_rows(ordinary));
        let mut result = json!({"outcome":"KILLED","reason":"user_test_failure"});
        apply_selection(&mut result, &current, &context).unwrap();
        assert_eq!(
            result["outcome"],
            if ordinary { "KILLED" } else { "UNCLASSIFIED" }
        );
        assert_eq!(
            result["reason"],
            if ordinary {
                "ordinary_author_failure_with_setup_suppression"
            } else {
                "author_test_catalogue_incomplete_after_setup_failure"
            }
        );
        assert_eq!(
            result["testCatalogueSuppression"]["catalogueIdentityProven"],
            false
        );
        assert_eq!(result["ordinaryFailureWitness"].is_object(), ordinary);
    }
}

#[test]
fn native_relative_setup_frames_reject_wrong_authors_and_traversal() {
    let (_, context) = fixture(false);
    for path in [
        "test/wrong.dart",
        "test/sub/../a.dart",
        "test/../test/a.dart",
    ] {
        let mut rows = native_reporter_rows(false);
        let error = rows
            .iter_mut()
            .find(|row| row["type"] == "error" && row["testID"] == 3)
            .unwrap();
        error["stackTrace"] = json!(format!(
            "package:subject/loader.dart 74:12 Loader.load\n{} 25:12 main.<fn>\n",
            path.replace('/', std::path::MAIN_SEPARATOR_STR)
        ));
        let mut result = json!({"outcome":"KILLED","reason":"user_test_failure"});
        apply_selection(&mut result, &snapshot(&rows), &context).unwrap();
        assert_eq!(result["outcome"], "INFRA_ERROR", "{path}");
        assert_eq!(result["reason"], "author_test_selection_changed", "{path}");
    }
}

#[test]
fn native_relative_ordinary_frames_reject_wrong_authors_and_traversal() {
    let (_, context) = fixture(false);
    for path in ["test/a.dart", "test/sub/../b.dart", "test/../test/b.dart"] {
        let mut rows = native_reporter_rows(true);
        let error = rows
            .iter_mut()
            .find(|row| row["type"] == "error" && row["testID"] == 13)
            .unwrap();
        error["stackTrace"] = json!(format!(
            "package:subject/loader.dart 74:12 Loader.load\n{} 11:7 main.<fn>\n",
            path.replace('/', std::path::MAIN_SEPARATOR_STR)
        ));
        let mut result = json!({"outcome":"KILLED","reason":"user_test_failure"});
        apply_selection(&mut result, &snapshot(&rows), &context).unwrap();
        assert_eq!(result["outcome"], "UNCLASSIFIED", "{path}");
        assert_eq!(
            result["reason"], "author_test_catalogue_incomplete_after_setup_failure",
            "{path}"
        );
        assert!(result["ordinaryFailureWitness"].is_null(), "{path}");
    }
}

#[test]
fn selection_policy_guards_primary_outcome_expected_selection_and_baseline() {
    let (current, context) = fixture(false);
    for (outcome, reason, expected) in [
        ("KILLED", "user_test_failure", "UNCLASSIFIED"),
        ("KILLED", "test_timeout", "INFRA_ERROR"),
        ("SURVIVED", "user_tests_passed", "INFRA_ERROR"),
        (
            "LOAD_FAILURE",
            "suite_load_failure_unattributed",
            "LOAD_FAILURE",
        ),
        ("TIMEOUT", "runner_timeout", "TIMEOUT"),
        ("INFRA_ERROR", "capture_digest_mismatch", "INFRA_ERROR"),
        ("INVALID", "compiler_load_failure", "INVALID"),
    ] {
        let mut result = json!({"outcome":outcome,"reason":reason});
        apply_selection(&mut result, &current, &context).unwrap();
        assert_eq!(result["outcome"], expected);
        assert_eq!(
            result.get("testIdentities").is_some(),
            matches!(outcome, "KILLED" | "SURVIVED" | "LOAD_FAILURE")
        );
    }
    for change in ["baseline", "mutant", "expected", "alias"] {
        let mut context = context.clone();
        match change {
            "baseline" => context.baseline = None,
            "mutant" => context.mutant = None,
            "expected" => context.expected = Some(vec![["wrong".into(), "selection".into()]]),
            _ => context.alias = "other".into(),
        };
        let mut result = json!({"outcome":"KILLED","reason":"user_test_failure"});
        apply_selection(&mut result, &current, &context).unwrap();
        assert_eq!(result["outcome"], "INFRA_ERROR", "{change}");
    }
    for expected in [None, Some(current.identities.clone())] {
        let context = SelectionContext {
            expected,
            ..SelectionContext::default()
        };
        let mut result = json!({"outcome":"SURVIVED","reason":"user_tests_passed"});
        apply_selection(&mut result, &current, &context).unwrap();
        assert_eq!(result["outcome"], "SURVIVED");
    }
}

#[test]
fn duplicate_full_keys_are_ambiguous_but_same_name_different_locations_are_not() {
    for same_location in [true, false] {
        let mut rows = [events(false, false), events(true, true)];
        for events in &mut rows {
            let mut start = events
                .iter()
                .find(|row| row["type"] == "testStart" && row["test"]["id"] == 13)
                .unwrap()
                .clone();
            start["test"]["id"] = json!(14);
            if !same_location {
                start["test"]["line"] = json!(20);
            }
            let done = json!({"type":"testDone","testID":14,"result":"success","skipped":false,"hidden":false});
            let at = events.len() - 2;
            events.splice(at..at, [start, done]);
            events
                .iter_mut()
                .find(|row| row["type"] == "group" && row["group"]["id"] == 12)
                .unwrap()["group"]["testCount"] = json!(2);
            retime(events);
        }
        let before = snapshot(&rows[0]);
        let after = snapshot(&rows[1]);
        let context = SelectionContext {
            expected: Some(before.identities.clone()),
            baseline: Some(before),
            mutant: Some(json!({"file":"lib/event.dart"})),
            alias: "subject".into(),
            initialization: None,
        };
        assert_eq!(
            ordinary_failure_witness(&after, &context).is_some(),
            !same_location
        );
    }
}

#[test]
fn outside_location_hidden_metadata_and_declared_count_drift_are_rejected() {
    let (_, context) = fixture(false);
    for change in [
        "outside_name",
        "outside_line",
        "outside_hidden",
        "group_count",
        "group_metadata",
        "missing_teardown",
    ] {
        let mut rows = events(true, false);
        match change {
            "outside_name" | "outside_line" => {
                let row = rows
                    .iter_mut()
                    .find(|row| row["type"] == "testStart" && row["test"]["id"] == 13)
                    .unwrap();
                row["test"][if change == "outside_name" {
                    "name"
                } else {
                    "line"
                }] = if change == "outside_name" {
                    json!("renamed")
                } else {
                    json!(11)
                };
            }
            "outside_hidden" => {
                rows.iter_mut()
                    .find(|row| row["type"] == "testDone" && row["testID"] == 13)
                    .unwrap()["hidden"] = json!(true)
            }
            "group_count" | "group_metadata" => {
                let row = rows
                    .iter_mut()
                    .find(|row| row["type"] == "group" && row["group"]["id"] == 2)
                    .unwrap();
                row["group"][if change == "group_count" {
                    "testCount"
                } else {
                    "metadata"
                }] = if change == "group_count" {
                    json!(3)
                } else {
                    json!({"skip":true})
                };
            }
            _ => rows.retain(|row| {
                row["testID"] != 11 && !(row["type"] == "testStart" && row["test"]["id"] == 11)
            }),
        }
        retime(&mut rows);
        assert!(
            explain_setup_suppression(&snapshot(&rows), &context).is_none(),
            "{change}"
        );
    }
}

#[test]
fn runtime_and_assertion_witnesses_require_bound_ordered_source_frames() {
    let (_, context) = fixture(true);
    for change in [
        "assertion",
        "compiler",
        "subject",
        "author",
        "early",
        "unknown",
        "failure_flag",
        "reverse_frames",
    ] {
        let mut rows = events(true, true);
        let error = rows
            .iter_mut()
            .find(|row| row["type"] == "error" && row["testID"] == 13)
            .unwrap();
        match change {
            "assertion" => {
                error["error"] = json!("Expected: <42>\n  Actual: <43>");
                error["isFailure"] = json!(true);
                error["stackTrace"] = json!("package:matcher expect\ntest/b.dart 11:7 main\n");
            }
            "compiler" => {
                error["error"] = json!("StateError: problem\ntest/b.dart:6:3: Error: bad compile")
            }
            "subject" => {
                error["stackTrace"] = json!("package:other/a.dart 1:2 run\ntest/b.dart 11:7 main")
            }
            "author" => {
                error["stackTrace"] = json!("package:subject/a.dart 1:2 run\ntest/a.dart 11:7 main")
            }
            "early" => {
                error["stackTrace"] = json!("package:subject/a.dart 1:2 run\ntest/b.dart 9:7 main")
            }
            "unknown" => error["error"] = json!("Something happened"),
            "failure_flag" => error["isFailure"] = json!(true),
            _ => {
                error["stackTrace"] = json!("test/b.dart 11:7 main\npackage:subject/a.dart 1:2 run")
            }
        }
        assert_eq!(
            ordinary_failure_witness(&snapshot(&rows), &context).is_some(),
            change == "assertion",
            "{change}"
        );
    }
}

#[test]
fn diagnostic_digest_matches_python_ascii_json_including_controls_and_astral_unicode() {
    let diagnostic = Diagnostic {
        error: "é🙂\n\u{8}\u{c}\u{0}\"\\/".into(),
        stack_trace: "\u{2028}\u{7f}".into(),
        is_failure: true,
    };
    let encoded = python_diagnostics(&[diagnostic]);
    assert_eq!(
        encoded,
        "[{\"error\": \"\\u00e9\\ud83d\\ude42\\n\\b\\f\\u0000\\\"\\\\/\", \"isFailure\": true, \"stackTrace\": \"\\u2028\\u007f\"}]"
    );
    // Golden produced independently with Python json.dumps(sort_keys=True).
    assert_eq!(
        crate::sha256_hex(encoded.as_bytes()),
        "99a2754842583d3ca75701cd3d487f77b035d3187cbbbad796e7ed3a1871175a"
    );
    assert_eq!(python_diagnostics(&[]), "[]");
}

#[test]
fn integer_metadata_never_rounds_distinct_author_keys_into_a_failure_witness() {
    let read = numeric_metadata_snapshot;
    // These are exact adjacent u64 values, beyond f64's exact integer range.
    let baseline = read(false, "18446744073709551614").unwrap();
    let context = SelectionContext {
        expected: Some(baseline.identities.clone()),
        baseline: Some(baseline),
        mutant: Some(json!({"file":"lib/event.dart"})),
        alias: "subject".into(),
        initialization: None,
    };
    let unchanged = read(true, "18446744073709551614").unwrap();
    assert!(ordinary_failure_witness(&unchanged, &context).is_some());
    let changed = read(true, "18446744073709551615").unwrap();
    assert!(ordinary_failure_witness(&changed, &context).is_none());
    let mut verdict = json!({"outcome":"KILLED","reason":"user_test_failure"});
    apply_selection(&mut verdict, &changed, &context).unwrap();
    assert_eq!(verdict["outcome"], "INFRA_ERROR");

    // Before the token guard, serde_json rounded each pair to the same f64,
    // allowing changed metadata to receive a false ordinary-failure witness.
    for integer in [
        "18446744073709551616",
        "18446744073709551617",
        "-9223372036854775809",
        "-9223372036854775810",
    ] {
        for failed in [false, true] {
            assert_eq!(
                read(failed, integer).unwrap_err().0,
                "integer_limit_exceeded"
            );
        }
    }
}

#[test]
fn numeric_metadata_preserves_float_precision_type_and_signed_zero_identity() {
    for (before, after, same_identity) in [
        ("51.248178375505404", "51.24817837550541", false),
        ("-93.31137037688033", "-93.31137037688032", false),
        ("-36.573994842753436", "-36.57399484275344", false),
        ("52.314008204106244", "52.31400820410624", false),
        ("97.45365320034685", "97.45365320034684", false),
        ("1", "1.0", false),
        ("-0", "0", true),
        ("-0.0", "0.0", false),
        ("1e0", "1.0", true),
        ("1.0000000000000001", "1.0", true),
        ("1.0000000000000002", "1.0", false),
    ] {
        let baseline = numeric_metadata_snapshot(false, before).unwrap();
        let context = SelectionContext {
            expected: Some(baseline.identities.clone()),
            baseline: Some(baseline),
            mutant: Some(json!({"file":"lib/event.dart"})),
            alias: "subject".into(),
            initialization: None,
        };
        // The opaque baseline is transported through JSON before reuse.
        let context = parse_selection_context(&serde_json::to_vec(&context).unwrap()).unwrap();
        let unchanged = numeric_metadata_snapshot(true, before).unwrap();
        assert!(
            ordinary_failure_witness(&unchanged, &context).is_some(),
            "{before}"
        );
        let changed = numeric_metadata_snapshot(true, after).unwrap();
        assert_eq!(
            ordinary_failure_witness(&changed, &context).is_some(),
            same_identity,
            "{before} -> {after}"
        );
        let mut verdict = json!({"outcome":"KILLED","reason":"user_test_failure"});
        apply_selection(&mut verdict, &changed, &context).unwrap();
        assert_eq!(
            verdict["outcome"],
            if same_identity {
                "KILLED"
            } else {
                "INFRA_ERROR"
            }
        );
    }
}

#[test]
fn context_rejects_duplicate_keys_unknown_fields_types_and_corrupted_snapshot() {
    for bytes in [
        b"{}".as_slice(),
        b"{\"alias\":\"subject\",\"alias\":\"subject\"}",
        b"{\"alias\":\"subject\",\"extra\":true}",
        b"{\"alias\":true}",
        b"{\"alias\":\"subject\",\"expected\":[[\"one\"]]}",
        b"{\"alias\":\"subject\",\"expected\":[[1,2]]}",
    ] {
        assert!(parse_selection_context(bytes).is_err());
    }
    let (_, context) = fixture(false);
    let original = serde_json::to_value(&context).unwrap();
    assert!(parse_selection_context(&serde_json::to_vec(&original).unwrap()).is_ok());
    for change in [
        "digest",
        "cwd",
        "duplicates",
        "key",
        "metadata",
        "counter",
        "location",
        "result",
        "identity_order",
    ] {
        let mut value = original.clone();
        let base = &mut value["baseline"];
        match change {
            "digest" => base["rawSha256"] = json!("0"),
            "cwd" => base["cwd"] = json!("relative"),
            "duplicates" => {
                let group = base["view"]["groups"][0].clone();
                base["view"]["groups"].as_array_mut().unwrap().push(group);
            }
            "key" => base["view"]["tests"][0]["key"]["name"] = json!("forged"),
            "metadata" => base["view"]["tests"][0]["metadata"]["skip"] = json!("false"),
            "counter" => base["view"]["failed"] = json!(1),
            "location" => base["view"]["tests"][0]["location"] = json!([1, 1, "lib/a.dart"]),
            "result" => base["view"]["tests"][0]["done"]["result"] = json!("error"),
            _ => base["identities"].as_array_mut().unwrap().reverse(),
        }
        assert!(
            parse_selection_context(&serde_json::to_vec(&value).unwrap()).is_err(),
            "{change}"
        );
    }
}

#[test]
fn group_index_rejects_overlapping_suppression_but_reduces_witness_ancestors() {
    let (_, context) = fixture(false);
    let view = context.baseline.as_ref().unwrap().view.as_ref().unwrap();
    let parent = view.groups[0].key.clone();
    let child = view.groups[1].key.clone();
    assert!(GroupIndex::new(vec![parent.clone(), child.clone()], true).is_none());
    let index = GroupIndex::new(vec![child.clone(), parent.clone()], false).unwrap();
    assert_eq!(index.groups.len(), 1);
    assert_eq!(index.containing(&child.suite, &child.chain), Some(0));
    assert_eq!(
        index.containing(&view.groups[2].key.suite, &view.groups[2].key.chain),
        None
    );
    let groups: Vec<_> = (0..10_000)
        .map(|i| GroupKey {
            suite: SuiteKey(format!("test/{i:05}.dart"), "vm".into()),
            chain: parent.chain.clone(),
        })
        .collect();
    let index = GroupIndex::new(groups, true).unwrap();
    for i in [0, 1, 4999, 9999] {
        assert_eq!(
            index.containing(
                &SuiteKey(format!("test/{i:05}.dart"), "vm".into()),
                &parent.chain
            ),
            Some(i)
        );
    }
}

#[test]
fn invalid_mutant_alias_or_unbounded_frame_text_cannot_support_a_witness() {
    let (current, context) = fixture(true);
    for file in [
        "lib/../a.dart",
        "lib/a/../../b.dart",
        "test/a.dart",
        "lib/a\\b.dart",
        "lib/a\0.dart",
    ] {
        let mut changed = context.clone();
        changed.mutant = Some(json!({"file":file}));
        assert!(ordinary_failure_witness(&current, &changed).is_none());
    }
    for alias in ["Subject", "subject/other", "subject.*", ""] {
        let mut changed = context.clone();
        changed.alias = alias.into();
        assert!(ordinary_failure_witness(&current, &changed).is_none());
    }
    assert!(!at_or_after("١١", 10));
    assert!(!at_or_after(&"9".repeat(21), 10));
    assert!(at_or_after("99999999999999999999", 10));
    let mut matcher = ErrorMatcher::new("subject", "/work").unwrap();
    assert!(
        matcher
            .prepare_author(&format!(
                "test/{}.dart",
                "x".repeat(MAX_AUTHOR_PATTERN_BYTES)
            ))
            .is_none()
    );
}

#[test]
#[ignore = "requires FAILFORGE_CATALOGUE_CORPUS pointing to the frozen migration oracle"]
fn frozen_python_catalogue_corpus() {
    let source = std::env::var_os("FAILFORGE_CATALOGUE_CORPUS").expect("corpus required");
    let mut differences = Vec::new();
    let mut compared = 0;
    for line in std::fs::read_to_string(source).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        let bytes = |key: &str| {
            row[key]["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| u8::try_from(b.as_u64().unwrap()).unwrap())
                .collect::<Vec<_>>()
        };
        let before = analyze_catalogue(
            &bytes("baselineRaw"),
            Path::new(row["baselineCwd"].as_str().unwrap()),
            ReporterMode::Dart,
        );
        let after = analyze_catalogue(
            &bytes("raw"),
            Path::new(row["cwd"].as_str().unwrap()),
            ReporterMode::Dart,
        );
        let observed = before
            .ok()
            .zip(after.ok())
            .and_then(|(baseline, mut current)| {
                current.identities = serde_json::from_value(row["observed"].clone()).unwrap();
                let context = SelectionContext {
                    expected: Some(serde_json::from_value(row["expected"].clone()).unwrap()),
                    baseline: Some(baseline),
                    mutant: Some(row["mutant"].clone()),
                    alias: row["alias"].as_str().unwrap().into(),
                    initialization: None,
                };
                match row["kind"].as_str().unwrap() {
                    "explain_setup_suppression" => explain_setup_suppression(&current, &context),
                    "ordinary_failure_witness" => ordinary_failure_witness(&current, &context),
                    other => panic!("{other}"),
                }
            })
            .unwrap_or(Value::Null);
        compared += 1;
        if observed != row["expectedResult"] {
            differences.push(json!({"caseId":row["caseId"],"sourceTest":row["sourceTest"],"expected":row["expectedResult"],"observed":observed}));
        }
    }
    let report = json!({"compared":compared,"differences":differences});
    if let Some(path) = std::env::var_os("FAILFORGE_CATALOGUE_PARITY_RESULT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    assert!(
        differences.is_empty(),
        "{}",
        serde_json::to_string_pretty(&report).unwrap()
    );
    assert!(compared > 0);
}
