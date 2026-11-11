//! Dart diagnostics can contain individual UTF-16 code units. Their display text
//! must not invalidate an otherwise complete authored-test failure transcript.
use failforge_dart::{ReporterMode, classify_dart_capture};
use serde_json::{Value, json};

fn prefix() -> Vec<String> {
    [
        json!({"type":"start","protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":1,"time":0}),
        json!({"type":"suite","suite":{"id":0,"platform":"vm","path":"test/unicode_test.dart"},"time":0}),
        json!({"type":"testStart","test":{"id":1,"name":"loading test/unicode_test.dart","suiteID":0,"groupIDs":[],"metadata":{"skip":false}},"time":0}),
        json!({"type":"allSuites","count":1,"time":0}),
        json!({"type":"testDone","testID":1,"result":"success","skipped":false,"hidden":true,"time":0}),
        json!({"type":"group","group":{"id":2,"suiteID":0,"parentID":null,"name":"","testCount":1},"time":0}),
        json!({"type":"testStart","test":{"id":3,"name":"expectChar with the high surrogate","suiteID":0,"groupIDs":[2],"metadata":{"skip":false}},"time":0}),
    ]
    .into_iter()
    .map(|event| event.to_string())
    .collect()
}

fn diagnostic(error: &str, stack_trace: &str) -> String {
    format!(
        r#"{{"type":"error","testID":3,"error":{error},"stackTrace":{stack_trace},"isFailure":false,"time":0}}"#
    )
}

fn stream(events: &[String]) -> Vec<u8> {
    format!("{}\n", events.join("\n")).into_bytes()
}

fn failure(event: &str) -> Vec<u8> {
    let mut events = prefix();
    events.push(event.to_owned());
    events.push(json!({"type":"testDone","testID":3,"result":"error","skipped":false,"hidden":false,"time":0}).to_string());
    events.push(json!({"type":"done","success":false,"time":0}).to_string());
    stream(&events)
}

fn classify(raw: &[u8], timeout: bool) -> Value {
    let cause = if timeout {
        "TimedOut"
    } else {
        "ProcessCompleted"
    };
    let exit = if timeout { 124 } else { 1 };
    let receipt = format!(
        "failforge: cause={cause} status=exit:{exit} execution_ms=1 stdout_bytes={} stderr_bytes=0 cleanup=complete display_dropped_bytes=0 display_unconfirmed_bytes=0 stdout_issue=none stderr_issue=none stdout_tail_truncated=false stderr_tail_truncated=false\n",
        raw.len()
    );
    classify_dart_capture(raw, exit, receipt.as_bytes(), ReporterMode::Dart)
}

fn assert_killed(raw: &[u8]) {
    let result = classify(raw, false);
    assert_eq!(result["outcome"], "KILLED", "{result}");
    assert_eq!(result["reason"], "user_test_failure", "{result}");
    assert_eq!(result["reporterComplete"], true);
    assert_eq!(result["tests"]["started"], 1);
    assert_eq!(result["tests"]["failed"], 1);
    assert_eq!(result["tests"]["loadingPassed"], 1);
}

#[test]
fn complete_authored_failures_accept_dart_lone_surrogates_in_diagnostics() {
    for message in [
        r#""Error: expected \"\ud83d\".""#,
        r#""Error: expected \"\udc6d\".""#,
        r#""\ud800\ud800\udc00\udfff""#,
        r#""\uD83D and \uDC6D""#,
    ] {
        assert_killed(&failure(&diagnostic(
            message,
            r#""test/unicode_test.dart 1:1 main""#,
        )));
    }
    assert_killed(&failure(&diagnostic(
        r#""expected a code unit""#,
        r#""test/unicode_test.dart 1:1 \ud83d\n""#,
    )));
}

#[test]
fn valid_emoji_pairs_and_literal_escape_text_remain_supported() {
    for message in [
        r#""expected \"\ud83d\udc6d\"""#,
        r#""expected 👭""#,
        r#""literal \\ud83d is not an orphan escape""#,
        r#""escaped slash \\ followed by valid \ud83d\udc6d""#,
        r#""ordinary failure with \\ paths and \n line breaks""#,
    ] {
        assert_killed(&failure(&diagnostic(message, r#""stack""#)));
    }
}

#[test]
fn unicode_diagnostic_support_does_not_relax_identity_or_metadata_strings() {
    let event = diagnostic(r#""expected \ud83d""#, r#""stack""#);
    for changed in [
        event.replace(r#""testID":3"#, r#""testID":"\ud83d""#),
        event.replace(r#""type":"error""#, r#""type":"\ud83d""#),
        event.replace(r#""time":0"#, r#""time":0,"ignored":"\ud83d""#),
        event.replace(r#""time":0"#, r#""time":0,"\ud83d":1"#),
    ] {
        let result = classify(&failure(&changed), false);
        assert_eq!(result["outcome"], "INFRA_ERROR", "{changed}: {result}");
        assert_eq!(result["reporterComplete"], false);
    }
}

#[test]
fn unicode_diagnostics_keep_duplicate_keys_and_integer_bounds_strict() {
    let event = diagnostic(r#""expected \ud83d""#, r#""stack""#);
    for (changed, expected) in [
        (
            event.replace(r#""time":0"#, r#""time":0,"\u0065rror":"duplicate""#),
            "duplicate_json_key",
        ),
        (
            event.replace(r#""time":0"#, r#""time":18446744073709551616"#),
            "integer_limit_exceeded",
        ),
    ] {
        let result = classify(&failure(&changed), false);
        assert_eq!(result["outcome"], "INFRA_ERROR", "{changed}: {result}");
        assert_eq!(result["reason"], expected, "{changed}: {result}");
    }
}

#[test]
fn invalid_escape_syntax_is_not_repaired_as_a_dart_code_unit() {
    for message in [
        r#""bad \ud83x""#,
        r#""bad \u123""#,
        r#""bad \x41""#,
        r#""orphan \ud83d then malformed \uqqqq""#,
    ] {
        let result = classify(&failure(&diagnostic(message, r#""stack""#)), false);
        assert_eq!(result["outcome"], "INFRA_ERROR", "{message}: {result}");
        assert_eq!(result["reason"], "malformed_reporter_output");
        assert_eq!(result["reporterComplete"], false);
    }
}

#[test]
fn truncated_unicode_events_never_establish_a_kill() {
    let mut raw = stream(&prefix());
    raw.extend_from_slice(br#"{"type":"error","testID":3,"error":"expected \ud83"#);
    let completed = classify(&raw, false);
    assert_eq!(completed["outcome"], "INFRA_ERROR");
    assert_eq!(completed["reason"], "malformed_reporter_output");
    assert_eq!(completed["reporterComplete"], false);
    let timed_out = classify(&raw, true);
    assert_eq!(timed_out["outcome"], "TIMEOUT");
    assert_eq!(timed_out["reason"], "runner_timeout");
    assert_eq!(timed_out["reporterComplete"], false);
    assert_eq!(timed_out["timeoutProgress"]["eventsParsed"], 7);
    assert_eq!(
        timed_out["timeoutProgress"]["stopReason"],
        "interrupted_final_event"
    );

    let complete_failure = failure(&diagnostic(r#""expected \ud83d""#, r#""stack""#));
    let timed_out = classify(&complete_failure, true);
    assert_eq!(timed_out["outcome"], "TIMEOUT");
    assert_eq!(timed_out["reason"], "runner_timeout");
    assert_eq!(timed_out["reporterComplete"], true);
    assert_eq!(timed_out["tests"]["failed"], 1);
}
