use serde_json::{Value, json};

use super::*;

fn start() -> Value {
    json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":"1.27.0","pid":42})
}

fn suite(id: u64) -> Value {
    json!({"type":"suite","time":1,"suite":{"id":id,"platform":"vm","path":"test/example_test.dart"}})
}

fn group(id: u64, suite: u64) -> Value {
    json!({"type":"group","time":2,"group":{"id":id,"suiteID":suite,"parentID":null,"name":"","testCount":1}})
}

fn test(id: u64, suite: u64, groups: &[u64], name: &str) -> Value {
    json!({"type":"testStart","time":3,"test":{"id":id,"suiteID":suite,"name":name,"groupIDs":groups,"metadata":{"skip":false}}})
}

fn diagnostic(id: u64, message: &str) -> Value {
    json!({"type":"error","time":4,"testID":id,"error":message,"stackTrace":"test/example_test.dart 4:1 main","isFailure":true})
}

fn done(id: u64, result: &str, skipped: bool, hidden: bool) -> Value {
    json!({"type":"testDone","time":5,"testID":id,"result":result,"skipped":skipped,"hidden":hidden})
}

fn finish(success: bool) -> Value {
    json!({"type":"done","time":6,"success":success})
}

fn ready() -> Reporter {
    let mut reporter = Reporter::new("dart");
    for event in [
        start(),
        suite(0),
        group(2, 0),
        json!({"type":"allSuites","time":2,"count":1}),
    ] {
        reporter.consume(&event).unwrap();
    }
    reporter
}

fn active() -> Reporter {
    let mut reporter = ready();
    reporter.consume(&test(3, 0, &[2], "works")).unwrap();
    reporter
}

fn rejected(reporter: &mut Reporter, event: Value, reason: &str) {
    let error = reporter.consume(&event).unwrap_err();
    assert_eq!(error.0, reason, "event {event}");
    assert!(!error.is_interrupted_candidate());
}

#[test]
fn loading_and_authored_results_have_independent_counts() {
    for (result, skipped, hidden, expected) in [
        (
            "success",
            false,
            false,
            Counts {
                started: 1,
                passed: 1,
                loading_passed: 1,
                ..Counts::default()
            },
        ),
        (
            "success",
            true,
            false,
            Counts {
                started: 1,
                skipped: 1,
                loading_passed: 1,
                ..Counts::default()
            },
        ),
        (
            "success",
            false,
            true,
            Counts {
                started: 1,
                hidden: 1,
                loading_passed: 1,
                ..Counts::default()
            },
        ),
        (
            "failure",
            false,
            false,
            Counts {
                started: 1,
                failed: 1,
                loading_passed: 1,
                ..Counts::default()
            },
        ),
        // A failed hidden setup is evidence of an authored test failure.
        (
            "error",
            false,
            true,
            Counts {
                started: 1,
                failed: 1,
                loading_passed: 1,
                ..Counts::default()
            },
        ),
    ] {
        let mut reporter = ready();
        reporter
            .consume(&test(1, 0, &[], "loading test/example_test.dart"))
            .unwrap();
        reporter.consume(&test(3, 0, &[2], "works")).unwrap();
        if result != "success" {
            reporter
                .consume(&diagnostic(3, "Expected true, actual false"))
                .unwrap();
        }
        reporter.consume(&done(1, "success", false, true)).unwrap();
        reporter.consume(&done(3, result, skipped, hidden)).unwrap();
        reporter.consume(&finish(result == "success")).unwrap();
        assert_eq!(reporter.counts, expected);
        assert!(reporter.finished);
        assert!(reporter.load_failures.is_empty());
    }
}

#[test]
fn no_tests_and_loading_only_are_valid_protocols_without_authored_passes() {
    let mut empty = Reporter::new("dart");
    for event in [
        start(),
        json!({"type":"allSuites","time":1,"count":0}),
        finish(true),
    ] {
        empty.consume(&event).unwrap();
    }
    assert_eq!(empty.counts, Counts::default());
    let mut loading = ready();
    loading
        .consume(&test(1, 0, &[], "loading test/example_test.dart"))
        .unwrap();
    loading.consume(&done(1, "success", false, true)).unwrap();
    loading.consume(&finish(true)).unwrap();
    assert_eq!(loading.counts.started, 0);
    assert_eq!(loading.counts.passed, 0);
    assert_eq!(loading.counts.loading_passed, 1);
}

#[test]
fn load_failures_retain_diagnostics_and_require_both_compiler_signatures() {
    for (message, kind) in [
        (
            "Failed to load test/example_test.dart:\nlib/a.dart:1:2: Error: Expected ';'.",
            "compiler",
        ),
        (
            "Failed to load x: lib/a.dart:١:٢: Error: bad syntax",
            "compiler",
        ),
        (
            "Failed to load test/example_test.dart: Bad state: mutant",
            "unknown",
        ),
        (
            "Failed to load x:\nlib/a.dart:1:2: error: lowercase",
            "unknown",
        ),
        (
            "lib/a.dart:1:2: Error: compiler-shaped user error",
            "unknown",
        ),
        (
            "prefix Failed to load x:\nlib/a.dart:1:2: Error: wrong prefix",
            "unknown",
        ),
        (
            "Failed to load x:\n.dart:1:2: Error: no file stem",
            "unknown",
        ),
    ] {
        let mut reporter = ready();
        reporter
            .consume(&test(1, 0, &[], "loading test/example_test.dart"))
            .unwrap();
        reporter.consume(&diagnostic(1, message)).unwrap();
        reporter.consume(&done(1, "error", false, true)).unwrap();
        reporter.consume(&finish(false)).unwrap();
        assert_eq!(reporter.counts.loading_failed, 1);
        assert_eq!(reporter.counts.failed, 0);
        assert_eq!(
            reporter.compile_load_failures,
            usize::from(kind == "compiler")
        );
        assert_eq!(reporter.other_load_failures, usize::from(kind == "unknown"));
        assert_eq!(
            serde_json::to_value(&reporter.load_failures).unwrap(),
            json!([{
                "suitePath":"test/example_test.dart", "testName":"loading test/example_test.dart",
                "errors":[{"error":message,"stackTrace":"test/example_test.dart 4:1 main","isFailure":true}], "kind":kind
            }])
        );
    }
}

#[test]
fn counts_use_the_existing_json_schema() {
    assert_eq!(
        serde_json::to_value(Counts::default()).unwrap(),
        json!({
            "started":0,"passed":0,"failed":0,"skipped":0,"hidden":0,"loadingPassed":0,"loadingFailed":0
        })
    );
}

#[test]
fn test_order_follows_insertion_rather_than_numeric_id_or_completion_order() {
    let mut reporter = ready();
    for id in [99, 7, 3] {
        reporter.consume(&test(id, 0, &[2], "works")).unwrap();
    }
    reporter.consume(&done(7, "success", false, false)).unwrap();
    assert_eq!(
        reporter.tests.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        [99, 7, 3]
    );
    assert!(reporter.test(7).unwrap().done);
    assert!(!reporter.test(99).unwrap().done);
    assert!(reporter.test(999).is_none());
}

#[test]
fn prints_and_debug_events_are_validated_without_counting_tests() {
    let mut reporter = active();
    reporter.consume(&json!({"type":"print","time":4,"testID":3,"message":"\u{2028}line\u{2029}","messageType":"print"})).unwrap();
    reporter
        .consume(&json!({"type":"debug","time":4,"suiteID":0}))
        .unwrap();
    assert_eq!(
        reporter.counts,
        Counts {
            started: 1,
            ..Counts::default()
        }
    );
    rejected(
        &mut reporter,
        json!({"type":"debug","time":4,"suiteID":true}),
        "unknown_debug_suite",
    );
    rejected(
        &mut reporter,
        json!({"type":"debug","time":4,"suiteID":9}),
        "unknown_debug_suite",
    );
    rejected(
        &mut reporter,
        json!({"type":"print","time":4,"testID":3,"message":"x"}),
        "invalid_print_event",
    );
    rejected(
        &mut reporter,
        json!({"type":"print","time":4,"testID":999,"message":"x","messageType":"print"}),
        "unknown_event_test",
    );
}

#[test]
fn event_envelopes_require_start_strict_times_and_known_types() {
    for (event, reason) in [
        (json!(null), "non_object_event"),
        (json!([]), "non_object_event"),
        (json!({"time":0}), "missing_event_type"),
        (json!({"type":false,"time":0}), "missing_event_type"),
        (json!({"type":"start"}), "invalid_event_time"),
        (json!({"type":"start","time":false}), "invalid_event_time"),
        (json!({"type":"start","time":0.0}), "invalid_event_time"),
        (json!({"type":"start","time":-1}), "invalid_event_time"),
        (suite(0), "missing_start_event"),
    ] {
        rejected(&mut Reporter::new("dart"), event, reason);
    }
    let mut reporter = active();
    rejected(
        &mut reporter,
        json!({"type":"unknown","time":3}),
        "unknown_event_type",
    );
    rejected(
        &mut reporter,
        json!({"type":"debug","time":2,"suiteID":0}),
        "invalid_event_time",
    );
    reporter.consume(&done(3, "success", false, false)).unwrap();
    reporter.consume(&finish(true)).unwrap();
    rejected(
        &mut reporter,
        json!({"type":"start","time":0}),
        "events_after_done",
    );
}

#[test]
fn start_fields_and_protocol_are_checked_independently() {
    for (key, value, reason) in [
        (
            "protocolVersion",
            json!("0.1.2"),
            "unsupported_reporter_protocol",
        ),
        (
            "protocolVersion",
            json!(null),
            "unsupported_reporter_protocol",
        ),
        ("pid", json!(0), "invalid_reporter_pid"),
        ("pid", json!(true), "invalid_reporter_pid"),
        ("runnerVersion", json!(null), "invalid_runner_version"),
        ("runnerVersion", json!(1), "invalid_runner_version"),
    ] {
        let mut event = start();
        event[key] = value;
        rejected(&mut Reporter::new("dart"), event, reason);
    }
    let mut reporter = Reporter::new("dart");
    reporter.consume(&start()).unwrap();
    rejected(&mut reporter, start(), "duplicate_start_event");
}

#[test]
fn suite_group_and_test_ids_share_one_namespace() {
    let mut reporter = ready();
    rejected(&mut reporter, group(0, 0), "duplicate_event_id");
    rejected(
        &mut reporter,
        test(2, 0, &[2], "works"),
        "duplicate_event_id",
    );
    let mut event = test(4, 0, &[2], "works");
    event["test"]["id"] = json!(true);
    rejected(&mut reporter, event, "invalid_event_id");
    let mut event = test(4, 0, &[2], "works");
    event["test"]["id"] = json!(4.0);
    rejected(&mut reporter, event, "invalid_event_id");
}

#[test]
fn group_parent_and_test_groups_must_belong_to_the_same_suite() {
    let mut reporter = Reporter::new("dart");
    for event in [start(), suite(0), suite(1), group(2, 0), group(4, 1)] {
        reporter.consume(&event).unwrap();
    }
    let mut wrong_parent = group(5, 1);
    wrong_parent["group"]["parentID"] = json!(2);
    rejected(&mut reporter, wrong_parent, "unknown_parent_group");
    let mut nested = group(6, 0);
    nested["group"]["parentID"] = json!(2);
    reporter.consume(&nested).unwrap();
    for (id, suite, groups) in [
        (10, 0, vec![4]),
        (11, 0, vec![2, 2]),
        (12, 0, vec![99]),
        (13, 99, vec![2]),
    ] {
        rejected(
            &mut reporter,
            test(id, suite, &groups, "works"),
            if suite == 99 {
                "unknown_test_suite"
            } else {
                "unknown_test_group"
            },
        );
    }
    reporter.consume(&test(14, 0, &[2, 6], "nested")).unwrap();
    assert_eq!(reporter.counts.started, 1);
}

#[test]
fn malformed_nested_event_metadata_is_rejected() {
    let cases = [
        (
            json!({"type":"suite","time":3,"suite":null}),
            "invalid_suite_event",
        ),
        (
            json!({"type":"suite","time":3,"suite":{"id":9,"path":"test/a.dart","platform":null}}),
            "invalid_suite_metadata",
        ),
        (
            json!({"type":"group","time":3,"group":[]}),
            "invalid_group_event",
        ),
        (
            json!({"type":"group","time":3,"group":{"id":9,"suiteID":99}}),
            "unknown_group_suite",
        ),
        (
            json!({"type":"group","time":3,"group":{"id":9,"suiteID":0,"name":"x","testCount":true}}),
            "invalid_group_metadata",
        ),
        (
            json!({"type":"testStart","time":3,"test":null}),
            "invalid_test_start",
        ),
        (
            json!({"type":"testStart","time":3,"test":{"id":9,"suiteID":0,"name":"x","groupIDs":null}}),
            "invalid_test_metadata",
        ),
        (
            test(9, 0, &[], "user synthetic name"),
            "unknown_synthetic_test",
        ),
    ];
    for (event, reason) in cases {
        rejected(&mut ready(), event, reason);
    }
    for bad_skip in [json!(null), json!(0), json!("false")] {
        let mut event = test(9, 0, &[2], "works");
        event["test"]["metadata"]["skip"] = bad_skip;
        rejected(&mut ready(), event, "invalid_test_metadata");
    }
}

#[test]
fn failed_event_retains_partial_state_but_never_finishes_reporter() {
    let mut reporter = Reporter::new("dart");
    reporter.consume(&start()).unwrap();
    let mut bad = suite(8);
    bad["time"] = json!(10);
    bad["suite"]["path"] = json!(null);
    rejected(&mut reporter, bad, "invalid_suite_metadata");
    assert_eq!(reporter.last_time, 10);
    let mut retry = suite(8);
    retry["time"] = json!(10);
    rejected(&mut reporter, retry, "duplicate_event_id");
    assert!(reporter.suites.is_empty());
    assert!(!reporter.finished);

    let mut reporter = ready();
    reporter
        .consume(&test(1, 0, &[], "loading test/a.dart"))
        .unwrap();
    rejected(
        &mut reporter,
        done(1, "success", true, true),
        "skipped_loading_test",
    );
    assert!(reporter.test(1).unwrap().done);
    assert_eq!(reporter.test(1).unwrap().result.as_deref(), Some("success"));
    assert_eq!(reporter.counts, Counts::default());
}

#[test]
fn error_and_print_after_completion_cannot_add_evidence() {
    let mut reporter = active();
    reporter.consume(&done(3, "success", false, false)).unwrap();
    let mut error = diagnostic(3, "late failure");
    error["time"] = json!(5);
    rejected(&mut reporter, error, "event_after_test_done");
    rejected(
        &mut reporter,
        done(3, "success", false, false),
        "duplicate_test_done",
    );
    assert!(reporter.test(3).unwrap().errors.is_empty());
    rejected(
        &mut active(),
        done(999, "success", false, false),
        "unknown_completed_test",
    );
    for (field, value) in [
        ("error", json!(null)),
        ("stackTrace", json!(false)),
        ("isFailure", json!(1)),
    ] {
        let mut bad = diagnostic(3, "failure");
        bad[field] = value;
        rejected(&mut active(), bad, "invalid_error_event");
    }
}

#[test]
fn completion_requires_consistent_result_and_error_evidence() {
    for (result, skipped, with_error, reason) in [
        ("failure", false, false, "missing_failure_evidence"),
        ("error", false, false, "missing_failure_evidence"),
        ("success", false, true, "successful_test_has_errors"),
        ("success", true, true, "contradictory_skipped_result"),
        ("error", true, true, "contradictory_skipped_result"),
        ("unsupported", false, false, "invalid_test_done"),
    ] {
        let mut reporter = active();
        if with_error {
            reporter.consume(&diagnostic(3, "failure")).unwrap();
        }
        rejected(&mut reporter, done(3, result, skipped, false), reason);
        assert!(!reporter.test(3).unwrap().done);
        assert_eq!(reporter.counts.failed, 0);
    }
    for field in ["hidden", "skipped"] {
        let mut event = done(3, "success", false, false);
        event[field] = json!(0);
        rejected(&mut active(), event, "invalid_test_done");
    }
}

#[test]
fn terminal_event_requires_complete_consistent_suite_and_test_state() {
    let mut reporter = Reporter::new("dart");
    reporter.consume(&start()).unwrap();
    rejected(&mut reporter, finish(true), "missing_all_suites_event");
    let mut reporter = ready();
    rejected(
        &mut reporter,
        json!({"type":"allSuites","time":2,"count":1}),
        "duplicate_all_suites_event",
    );
    rejected(
        &mut reporter,
        json!({"type":"done","time":6,"success":null}),
        "incomplete_reporter_done",
    );
    let mut reporter = ready();
    reporter.all_suites = Some(2);
    rejected(&mut reporter, finish(true), "suite_count_mismatch");
    rejected(&mut active(), finish(true), "unfinished_tests");
    let mut reporter = ready();
    rejected(&mut reporter, finish(false), "contradictory_done_success");
    assert_eq!(reporter.success, Some(false));
    assert!(!reporter.finished);
    let mut reporter = active();
    reporter.consume(&diagnostic(3, "failure")).unwrap();
    reporter.consume(&done(3, "failure", false, false)).unwrap();
    rejected(&mut reporter, finish(true), "contradictory_done_success");
    let mut reporter = Reporter::new("dart");
    reporter.consume(&start()).unwrap();
    rejected(
        &mut reporter,
        json!({"type":"allSuites","time":1,"count":true}),
        "invalid_suite_count",
    );
}

#[test]
fn flutter_accepts_only_exact_tool_event_schema_and_nullable_version() {
    let valid = json!([{"event":"test.startedProcess","params":{"vmServiceUri":null}}]);
    rejected(
        &mut Reporter::new("flutter"),
        valid.clone(),
        "missing_start_event",
    );
    rejected(
        &mut Reporter::new("dart"),
        valid.clone(),
        "non_object_event",
    );
    let mut reporter = Reporter::new("flutter");
    let mut event = start();
    event["runnerVersion"] = Value::Null;
    reporter.consume(&event).unwrap();
    reporter.consume(&valid).unwrap();
    reporter.consume(&json!([{"event":"test.startedProcess","params":{"vmServiceUri":"http://127.0.0.1:1234/token"}}])).unwrap();
    for (event, reason) in [
        (json!([]), "invalid_flutter_tool_event"),
        (json!([null]), "invalid_flutter_tool_event"),
        (
            json!([{"event":"test.startedProcess","params":{"vmServiceUri":null},"extra":0}]),
            "invalid_flutter_tool_event",
        ),
        (
            json!([{"event":"unknown","params":{"vmServiceUri":null}}]),
            "unknown_flutter_tool_event",
        ),
        (
            json!([{"event":"test.startedProcess","params":{}}]),
            "invalid_flutter_tool_event",
        ),
        (
            json!([{"event":"test.startedProcess","params":{"vmServiceUri":false}}]),
            "invalid_flutter_tool_event",
        ),
        (
            json!([{"event":"test.startedProcess","params":{"vmServiceUri":null,"extra":true}}]),
            "invalid_flutter_tool_event",
        ),
    ] {
        rejected(&mut reporter, event, reason);
    }
    reporter
        .consume(&json!({"type":"allSuites","time":1,"count":0}))
        .unwrap();
    reporter.consume(&finish(true)).unwrap();
    rejected(&mut reporter, valid, "events_after_done");
    let mut no_version = start();
    no_version.as_object_mut().unwrap().remove("runnerVersion");
    rejected(
        &mut Reporter::new("flutter"),
        no_version,
        "invalid_runner_version",
    );
}

fn timeout(message: &str) -> Diagnostic {
    Diagnostic {
        error: message.to_owned(),
        stack_trace: "dart:isolate  _RawReceivePort._handleMessage".to_owned(),
        is_failure: false,
    }
}

#[test]
fn timeout_diagnostic_recognizes_exact_duration_format_and_elapsed_time() {
    for (duration, pretty, millis) in [
        ("0:00:01.000000", "1 seconds", 1000),
        ("0:01:02.300000", "1 minutes, 2.3 seconds", 62300),
        ("0:01:00.300000", "1 minutes", 60300),
        ("1:00:00.000000", "60 minutes", 3600000),
        ("0:00:00.100000", "0.1 seconds", 100),
        ("0:00:00.000001", "0 seconds", 1),
    ] {
        let error = timeout(&format!(
            "TimeoutException after {duration}: Test timed out after {pretty}."
        ));
        assert!(
            test_timeout_diagnostic(&error, 10, 10 + millis),
            "{duration}"
        );
        assert!(
            !test_timeout_diagnostic(&error, 10, 10 + millis - 1),
            "{duration}"
        );
    }
    let thirty = timeout(
        "TimeoutException after 0:00:30.000000: Test timed out after 30 seconds. See https://pub.dev/packages/test#timeouts",
    );
    assert!(test_timeout_diagnostic(&thirty, 0, 30000));
    assert!(!test_timeout_diagnostic(
        &timeout("TimeoutException after 0:00:30.000000: Test timed out after 30 seconds."),
        0,
        30000
    ));
    for ending in ["", "\n", "\r\n", " \t\r\n"] {
        let mut error = thirty.clone();
        error.stack_trace.push_str(ending);
        assert!(test_timeout_diagnostic(&error, 0, 30000));
    }
}

#[test]
fn timeout_diagnostic_rejects_spoofed_shapes_and_overflow_without_panicking() {
    let canonical = "TimeoutException after 0:00:01.000000: Test timed out after 1 seconds.";
    for message in [
        format!("prefix {canonical}"),
        format!("{canonical}\n"),
        format!("{canonical} suffix"),
        "TimeoutException after 00:00:01.000000: Test timed out after 1 seconds.".to_owned(),
        "TimeoutException after ٠:00:01.000000: Test timed out after 1 seconds.".to_owned(),
        "TimeoutException after 0:60:00.000000: Test timed out after 60 minutes.".to_owned(),
        "TimeoutException after 0:00:60.000000: Test timed out after 1 minutes.".to_owned(),
        "TimeoutException after 0:00:00.000000: Test timed out after 0 seconds.".to_owned(),
        "TimeoutException after 9999999999:59:59.999999: Test timed out after 1 seconds."
            .to_owned(),
    ] {
        assert!(
            !test_timeout_diagnostic(&timeout(&message), 0, u64::MAX),
            "{message}"
        );
    }
    assert!(!test_timeout_diagnostic(&timeout(canonical), 1001, 1000));
    let mut error = timeout(canonical);
    error.is_failure = true;
    assert!(!test_timeout_diagnostic(&error, 0, 1000));
    for stack in [
        "",
        "package:test/test.dart 1:2 main",
        "x dart:isolate  _RawReceivePort._handleMessage",
        "dart:isolate  _RawReceivePort._handleMessage\n\n",
        "dart:isolate\n_RawReceivePort._handleMessage",
    ] {
        let mut error = timeout(canonical);
        error.stack_trace = stack.to_owned();
        assert!(!test_timeout_diagnostic(&error, 0, 1000));
    }
}

#[test]
fn timeout_observation_applies_only_to_authored_tests_and_latches_once() {
    let mut reporter = ready();
    reporter
        .consume(&test(1, 0, &[], "loading test/a.dart"))
        .unwrap();
    reporter.consume(&test(3, 0, &[2], "author")).unwrap();
    for id in [1, 3, 3] {
        let mut event = diagnostic(
            id,
            "TimeoutException after 0:00:01.000000: Test timed out after 1 seconds.",
        );
        event["time"] = json!(1003);
        event["isFailure"] = json!(false);
        event["stackTrace"] = json!("dart:isolate  _RawReceivePort._handleMessage");
        reporter.consume(&event).unwrap();
    }
    assert!(!reporter.test(1).unwrap().timeout_diagnostic_observed);
    assert!(reporter.test(3).unwrap().timeout_diagnostic_observed);
    assert_eq!(reporter.test(3).unwrap().errors.len(), 2);
    assert_eq!(reporter.counts.failed, 0);
}

#[test]
fn strict_json_rejects_duplicates_at_every_depth_even_ignored_metadata() {
    for bytes in [
        br#"{"type":"start","type":"done"}"#.as_slice(),
        br#"{"ignored":{"x":0,"x":1}}"#,
        br#"[{"params":{"vmServiceUri":null,"vmServiceUri":"uri"}}]"#,
        br#"{"a":1,"\u0061":2}"#,
    ] {
        assert_eq!(parse_event(bytes).unwrap_err().0, "duplicate_json_key");
    }
}

#[test]
fn strict_json_bounds_numbers_and_keeps_integer_zero_distinct_from_float_zero() {
    let parsed=parse_event(br#"{"zero":-0,"float":-0.0,"exponent":0e0,"max":18446744073709551615,"min":-9223372036854775808}"#).unwrap();
    assert_eq!(parsed["zero"].as_u64(), Some(0));
    assert_eq!(parsed["max"].as_u64(), Some(u64::MAX));
    assert_eq!(parsed["min"].as_i64(), Some(i64::MIN));
    for field in ["float", "exponent"] {
        assert_eq!(parsed[field].as_u64(), None, "{field}");
    }
    for input in ["100000000000000000000", "-100000000000000000000"] {
        assert_eq!(
            parse_event(input.as_bytes()).unwrap_err().0,
            "integer_limit_exceeded"
        );
    }
    for input in ["NaN", "Infinity", "-Infinity", "1e400", "-1e400"] {
        assert_eq!(
            parse_event(input.as_bytes()).unwrap_err().0,
            "non_json_number"
        );
    }
    assert_eq!(
        parse_event(
            br#"{"literal":"NaN Infinity -Infinity 100000000000000000000 -0","escaped":"\\\"-0"}"#
        )
        .unwrap()["escaped"],
        "\\\"-0"
    );
    let mut reporter = Reporter::new("dart");
    reporter.consume(&parse_event(br#"{"type":"start","time":-0,"pid":1,"protocolVersion":"0.1.1","runnerVersion":"1"}"#).unwrap()).unwrap();
    for input in ["-0.0", "true"] {
        let event = parse_event(
            format!(
                r#"{{"type":"suite","time":1,"suite":{{"id":{input},"path":"x","platform":"vm"}}}}"#
            )
            .as_bytes(),
        )
        .unwrap();
        rejected(&mut reporter, event, "invalid_event_id");
    }
}

#[test]
fn strict_json_rejects_lossy_integral_tokens_before_metadata_identity_comparison() {
    for token in [
        "18446744073709551616",
        "18446744073709551617",
        "99999999999999999999",
        "-9223372036854775809",
        "-9223372036854775810",
        "-18446744073709551615",
        "-99999999999999999999",
    ] {
        for raw in [
            token.to_owned(),
            format!(r#"{{"metadata":{{"nested":[{{"identity":{token}}}]}}}}"#),
        ] {
            assert_eq!(
                parse_event(raw.as_bytes()).unwrap_err(),
                EvidenceError("integer_limit_exceeded", false),
                "{raw}"
            );
            // The private selection context uses the same strict decoder.
            assert_eq!(
                parse_json(raw.as_bytes(), 64 * 1024 * 1024).unwrap_err(),
                EvidenceError("integer_limit_exceeded", false),
                "{raw}"
            );
        }
    }
}

#[test]
fn strict_json_preserves_exact_integer_boundaries_and_finite_float_spellings() {
    let parsed = parse_event(
        br#"[18446744073709551614,18446744073709551615,-9223372036854775808,-9223372036854775807,9223372036854775808]"#,
    )
    .unwrap();
    assert_eq!(parsed[0].as_u64(), Some(u64::MAX - 1));
    assert_eq!(parsed[1].as_u64(), Some(u64::MAX));
    assert_eq!(parsed[2].as_i64(), Some(i64::MIN));
    assert_eq!(parsed[3].as_i64(), Some(i64::MIN + 1));
    assert_eq!(parsed[4].as_u64(), Some(i64::MAX as u64 + 1));
    assert_ne!(parsed[0], parsed[1]);
    assert_ne!(parsed[2], parsed[3]);
    // Decimal and exponent spellings retain JSON floating-point semantics.
    // These values are finite and exactly representable as f64, even though
    // their integral magnitudes exceed the accepted integer token range.
    for token in ["18446744073709551616.0", "-9223372036854775808e1", "1.25"] {
        let parsed = parse_event(token.as_bytes()).unwrap();
        assert!(parsed.is_f64(), "{token}");
        assert_eq!(parsed.as_f64(), Some(token.parse::<f64>().unwrap()));
    }
    let literal = parse_event(br#"{"value":"18446744073709551616 -9223372036854775809"}"#).unwrap();
    assert_eq!(
        literal["value"],
        "18446744073709551616 -9223372036854775809"
    );
}

#[test]
fn strict_json_preserves_correctly_rounded_finite_float_bits() {
    for token in [
        "51.248178375505404",
        "51.24817837550541",
        "-93.31137037688033",
        "-93.31137037688032",
        "9007199254740993.0",
        "1.7976931348623157e308",
        "-1.7976931348623157e308",
        "2.2250738585072011e-308",
        "2.2250738585072014e-308",
        "4.9406564584124654e-324",
        "2.4703282292062327e-324",
        "2.4703282292062328e-324",
        "1e-400",
        "-1e-400",
        "0e99999999999999999999",
        "-0e99999999999999999999",
        "-0.0",
    ] {
        let expected = token.parse::<f64>().unwrap();
        assert!(expected.is_finite());
        let parsed = parse_event(token.as_bytes()).unwrap();
        assert!(parsed.is_f64(), "{token}");
        assert_eq!(
            parsed.as_f64().unwrap().to_bits(),
            expected.to_bits(),
            "{token}"
        );
    }
    for token in ["1.7976931348623159e308", "-1.7976931348623159e308"] {
        assert_eq!(
            parse_event(token.as_bytes()).unwrap_err(),
            EvidenceError("non_json_number", false)
        );
    }
    for token in ["1.e2", "1e+", "-01.0", "+1.0", ".5", "1e1.0"] {
        assert_eq!(
            parse_event(token.as_bytes()).unwrap_err().0,
            "malformed_reporter_output",
            "{token}"
        );
    }
}

#[test]
fn strict_json_finite_float_serialization_roundtrip_is_bit_exact() {
    // Deterministic sampling spans signs, exponents and mantissas without a
    // random dependency or long-running fuzz job. Shortest-decimal formatting
    // must preserve every accepted f64 when a private snapshot is reread.
    let mut state = 0x60e2_6b41_7598_a3d7_u64;
    let mut checked = 0;
    for _ in 0..10_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let value = f64::from_bits(state);
        if !value.is_finite() {
            continue;
        }
        let token = serde_json::to_string(&value).unwrap();
        let decoded = parse_event(token.as_bytes()).unwrap();
        assert!(decoded.is_f64(), "{token}");
        assert_eq!(decoded.as_f64().unwrap().to_bits(), state, "{token}");
        checked += 1;
    }
    assert!(checked > 9_900);
}

#[test]
fn strict_json_rejects_nonfinite_values_in_ignored_metadata() {
    // Python's json.loads accepts overflow exponents as infinity in ignored
    // metadata. Native parsing deliberately rejects this non-finite value.
    assert_eq!(
        parse_event(br#"{"type":"start","time":0,"ignored":1e400}"#).unwrap_err(),
        EvidenceError("non_json_number", false)
    );
}

#[test]
fn strict_json_distinguishes_malformed_input_from_nonrecoverable_depth_limits() {
    for bytes in [
        b"".as_slice(),
        b"{",
        b"{}{}",
        b"[1,]",
        b"01",
        b"-",
        b"{\"x\":\"unterminated",
        b"{\"x\":\"\xff\"}",
        br#"{"x":"\uD800"}"#,
    ] {
        let error = parse_event(bytes).unwrap_err();
        assert_eq!(error.0, "malformed_reporter_output");
        assert!(error.is_interrupted_candidate(), "{bytes:?}");
    }
    let deep = format!("{}0{}", "[".repeat(256), "]".repeat(256));
    let error = parse_event(deep.as_bytes()).unwrap_err();
    assert_eq!(error.0, "malformed_reporter_output");
    assert!(!error.is_interrupted_candidate());
    let oversized = vec![b' '; 8 * 1024 * 1024 + 1];
    assert_eq!(
        parse_event(&oversized).unwrap_err().0,
        "event_limit_exceeded"
    );
}

#[test]
fn retained_budget_stops_suite_path_amplification_before_test_allocation() {
    let mut reporter = Reporter::new("dart");
    reporter.consume(&start()).unwrap();
    let mut long_suite = suite(0);
    long_suite["suite"]["path"] = json!("x".repeat(16 * 1024));
    reporter.consume(&long_suite).unwrap();
    reporter.consume(&group(2, 0)).unwrap();
    reporter.memory.maximum = reporter.memory.used + 24 * 1024;
    reporter.consume(&test(3, 0, &[2], "first")).unwrap();
    rejected(
        &mut reporter,
        test(4, 0, &[2], "second"),
        "reporter_memory_limit_exceeded",
    );
    assert_eq!(reporter.counts.started, 1);
    assert_eq!(reporter.tests.len(), 1);
    assert!(reporter.test(4).is_none());
    assert!(reporter.memory.used <= reporter.memory.maximum);
    assert_eq!(
        reporter
            .tests
            .iter()
            .map(|(_, test)| test.suite_path.len())
            .sum::<usize>(),
        16 * 1024
    );
    // The uncapped control demonstrates the actual amplification: another
    // tiny testStart retains another full copy of the one suite declaration.
    reporter.memory.maximum = usize::MAX;
    reporter
        .consume(&test(5, 0, &[2], "uncapped control"))
        .unwrap();
    assert_eq!(
        reporter
            .tests
            .iter()
            .map(|(_, test)| test.suite_path.len())
            .sum::<usize>(),
        32 * 1024
    );
}

#[test]
fn retained_budget_rejects_diagnostics_before_copying_or_counting_them() {
    let mut reporter = active();
    reporter.memory.maximum = reporter.memory.used;
    rejected(
        &mut reporter,
        diagnostic(3, "a large failure"),
        "reporter_memory_limit_exceeded",
    );
    let test = reporter.test(3).unwrap();
    assert!(test.errors.is_empty());
    assert!(!test.timeout_diagnostic_observed);
    assert_eq!(reporter.counts.failed, 0);
}

#[test]
fn retained_budget_covers_load_failure_diagnostic_and_identity_duplicates() {
    let mut reporter = ready();
    reporter
        .consume(&test(1, 0, &[], "loading test/a.dart"))
        .unwrap();
    reporter
        .consume(&diagnostic(1, "Failed to load test/a.dart: StateError"))
        .unwrap();
    reporter.memory.maximum = reporter.memory.used + "error".len();
    rejected(
        &mut reporter,
        done(1, "error", false, true),
        "reporter_memory_limit_exceeded",
    );
    assert!(reporter.test(1).unwrap().done);
    assert_eq!(reporter.counts.loading_failed, 0);
    assert_eq!(reporter.compile_load_failures, 0);
    assert_eq!(reporter.other_load_failures, 0);
    assert!(reporter.load_failures.is_empty());
}

#[test]
fn retained_budget_bounds_ids_suites_groups_and_counter_overflow() {
    let mut reporter = Reporter::new("dart");
    reporter.consume(&start()).unwrap();
    reporter.memory.maximum = 0;
    rejected(&mut reporter, suite(0), "reporter_memory_limit_exceeded");
    assert!(reporter.ids.is_empty());
    reporter.memory.maximum = std::mem::size_of::<u64>() * 4;
    rejected(&mut reporter, suite(0), "reporter_memory_limit_exceeded");
    assert!(reporter.suites.is_empty());
    rejected(&mut reporter, suite(0), "duplicate_event_id");
    let mut budget = RetainedBudget {
        used: usize::MAX,
        maximum: usize::MAX,
    };
    assert_eq!(
        budget.reserve(1).unwrap_err().0,
        "reporter_memory_limit_exceeded"
    );
    assert_eq!(budget.used, usize::MAX);
}
