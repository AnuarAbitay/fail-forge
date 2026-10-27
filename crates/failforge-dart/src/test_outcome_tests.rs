use super::*;
#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;
fn native_status(code: i32) -> std::process::ExitStatus {
    #[cfg(unix)]
    {
        std::process::ExitStatus::from_raw(code << 8)
    }
    #[cfg(windows)]
    {
        std::process::ExitStatus::from_raw(code as u32)
    }
}
use failforge_runner::{CancelReason, CaptureIssue, CaptureSummary, CleanupReport, GroupState};

fn events(result: &str) -> Vec<Value> {
    let mut rows = vec![
        json!({"type":"start","protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":1,"time":0}),
        json!({"type":"suite","suite":{"id":0,"platform":"vm","path":"/tmp/test.dart"},"time":0}),
        json!({"type":"testStart","test":{"id":1,"name":"loading /tmp/test.dart","suiteID":0,"groupIDs":[],"metadata":{"skip":false}},"time":0}),
        json!({"type":"allSuites","count":1,"time":0}),
        json!({"type":"testDone","testID":1,"result":"success","skipped":false,"hidden":true,"time":0}),
        json!({"type":"group","group":{"id":2,"suiteID":0,"parentID":null,"name":"","testCount":1},"time":0}),
        json!({"type":"testStart","test":{"id":3,"name":"assertion","suiteID":0,"groupIDs":[2],"metadata":{"skip":false}},"time":0}),
    ];
    if result != "success" {
        rows.push(json!({"type":"error","testID":3,"error":"expected another value","stackTrace":"stack","isFailure":result=="failure","time":0}));
    }
    rows.push(json!({"type":"testDone","testID":3,"result":result,"skipped":false,"hidden":false,"time":0}));
    rows.push(json!({"type":"done","success":result=="success","time":0}));
    rows
}

fn stream(rows: &[Value]) -> Vec<u8> {
    let mut raw = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut raw, row).unwrap();
        raw.push(b'\n');
    }
    raw
}

fn receipt(raw: &[u8], exit: i32) -> String {
    format!(
        "failforge: cause=ProcessCompleted status=exit:{exit} execution_ms=1 stdout_bytes={} stderr_bytes=0 cleanup=complete display_dropped_bytes=0 display_unconfirmed_bytes=0 stdout_issue=none stderr_issue=none stdout_tail_truncated=false stderr_tail_truncated=false\n",
        raw.len()
    )
}

fn classify_rows(rows: &[Value], exit: i32) -> Value {
    let raw = stream(rows);
    classify_dart_capture(
        &raw,
        exit,
        receipt(&raw, exit).as_bytes(),
        ReporterMode::Dart,
    )
}

fn timeout(raw: &[u8]) -> Value {
    classify_dart_capture(
        raw,
        124,
        receipt(raw, 124)
            .replace("cause=ProcessCompleted", "cause=TimedOut")
            .as_bytes(),
        ReporterMode::Dart,
    )
}

fn assert_verdict(result: &Value, outcome: &str, reason: &str) {
    assert_eq!(result["outcome"], outcome, "{result}");
    assert_eq!(result["reason"], reason, "{result}");
}

#[test]
fn passing_failing_runtime_and_skipped_author_tests_are_distinct() {
    for (result, exit, outcome, reason) in [
        ("success", 0, "SURVIVED", "user_tests_passed"),
        ("failure", 1, "KILLED", "user_test_failure"),
        ("error", 255, "KILLED", "user_test_failure"),
        (
            "failure",
            0,
            "INFRA_ERROR",
            "failed_test_exit_code_mismatch",
        ),
        (
            "success",
            1,
            "INFRA_ERROR",
            "successful_tests_exit_code_mismatch",
        ),
    ] {
        assert_verdict(&classify_rows(&events(result), exit), outcome, reason);
    }
    for hidden in [true, false] {
        let mut rows = events("success");
        rows[7][if hidden { "hidden" } else { "skipped" }] = json!(true);
        for exit in [0, 79] {
            assert_verdict(
                &classify_rows(&rows, exit),
                "NO_TESTS",
                "no_executed_user_tests",
            );
        }
        assert_verdict(
            &classify_rows(&rows, 1),
            "INFRA_ERROR",
            "no_tests_exit_code_mismatch",
        );
    }
}

fn loading_failure(compiler: bool) -> Vec<Value> {
    let mut rows = events("success");
    rows.truncate(4);
    rows.push(json!({"type":"error","testID":1,"error": if compiler { "Failed to load /tmp/test.dart:\n/tmp/test.dart:1:2: Error: invalid type" } else { "Bad state: registration failed" },"stackTrace":"stack","isFailure":false,"time":0}));
    rows.push(json!({"type":"testDone","testID":1,"result":"error","skipped":false,"hidden":true,"time":0}));
    rows.push(json!({"type":"done","success":false,"time":0}));
    rows
}

#[test]
fn synthetic_loading_diagnostics_never_prove_author_failure() {
    for compiler in [true, false] {
        let rows = loading_failure(compiler);
        let verdict = classify_rows(&rows, 1);
        assert_verdict(
            &verdict,
            if compiler { "INVALID" } else { "LOAD_FAILURE" },
            if compiler {
                "compiler_load_failure"
            } else {
                "suite_load_failure_unattributed"
            },
        );
        assert_eq!(verdict["phase"], "suite_loading");
        assert_eq!(verdict["tests"]["failed"], 0);
        assert_eq!(verdict["tests"]["loadingFailed"], 1);
        assert_eq!(
            verdict["loadFailures"][0]["kind"],
            if compiler { "compiler" } else { "unknown" }
        );
        assert_verdict(
            &classify_rows(&rows, 0),
            "INFRA_ERROR",
            "failed_load_exit_code_mismatch",
        );
    }
}

#[test]
fn cleanup_capture_and_termination_precede_reporter_verdicts() {
    let raw = stream(&events("success"));
    for (from, to, reason) in [
        (
            "cleanup=complete",
            "cleanup=incomplete",
            "incomplete_runner_cleanup",
        ),
        (
            "stdout_issue=none",
            "stdout_issue=drain-expired",
            "runner_capture_issue",
        ),
        (
            "stderr_issue=none",
            "stderr_issue=read-error",
            "runner_capture_issue",
        ),
        (
            "cause=ProcessCompleted",
            "cause=RunnerFailure",
            "runner_not_completed",
        ),
        (
            "cause=ProcessCompleted",
            "cause=Cancelled(CtrlC)",
            "runner_not_completed",
        ),
        ("status=exit:0", "status=signal:9", "no_normal_child_exit"),
        (
            "status=exit:0",
            "status=unavailable",
            "no_normal_child_exit",
        ),
        ("status=exit:0", "status=exit:1", "child_exit_code_mismatch"),
        (
            "cause=ProcessCompleted",
            "cause=TimedOut",
            "timeout_exit_code_mismatch",
        ),
    ] {
        let verdict = classify_dart_capture(
            &raw,
            0,
            receipt(&raw, 0).replace(from, to).as_bytes(),
            ReporterMode::Dart,
        );
        assert_verdict(&verdict, "INFRA_ERROR", reason);
        assert_eq!(verdict["tests"]["started"], 0);
    }
    let result = classify_dart_capture(
        &raw,
        0,
        receipt(&raw[..raw.len() - 1], 0).as_bytes(),
        ReporterMode::Dart,
    );
    assert_verdict(&result, "INFRA_ERROR", "incomplete_stdout_capture");
}

#[test]
fn display_losses_do_not_invalidate_complete_raw_capture() {
    let raw = stream(&events("success"));
    let text = receipt(&raw, 0)
        .replace("display_dropped_bytes=0", "display_dropped_bytes=999")
        .replace("display_unconfirmed_bytes=0", "display_unconfirmed_bytes=1")
        .replace("stdout_tail_truncated=false", "stdout_tail_truncated=true");
    assert_verdict(
        &classify_dart_capture(&raw, 0, text.as_bytes(), ReporterMode::Dart),
        "SURVIVED",
        "user_tests_passed",
    );
}

#[test]
fn summary_requires_exactly_one_coherent_line() {
    let raw = stream(&events("success"));
    let base = receipt(&raw, 0);
    for bad in [
        String::new(),
        format!("{base}{base}"),
        base.replace(" status=exit:0", "\nfailforge: status=exit:0"),
        base.replace("status=exit:0", "status=exit:0 status=exit:0"),
        base.replace("status=exit:0", "status=exit:0 unknown=1"),
        base.replace("status=exit:0", "status=exit:0 incomplete"),
        base.replace("status=exit:0", "status=exit:0=x"),
    ] {
        assert_verdict(
            &classify_dart_capture(&raw, 0, bad.as_bytes(), ReporterMode::Dart),
            "INFRA_ERROR",
            "invalid_runner_summary",
        );
    }
    let incomplete = b"failforge: cause=ProcessCompleted status=exit:0\n";
    assert_verdict(
        &classify_dart_capture(&raw, 0, incomplete, ReporterMode::Dart),
        "INFRA_ERROR",
        "incomplete_runner_summary",
    );
    assert!(
        parse_cli_summary(
            format!("untrusted child diagnostic\nfailforge: spawn failed\n{base}").as_bytes()
        )
        .is_ok()
    );
}

#[test]
fn summary_rejects_noncanonical_numbers_overflows_and_invalid_enums() {
    for (key, value) in [
        ("stdout_bytes", "00"),
        ("stdout_bytes", "+1"),
        ("stdout_bytes", "-1"),
        ("stdout_bytes", "18446744073709551616"),
        ("stdout_bytes", "1.0"),
        ("execution_ms", "340282366920938463463374607431768211456"),
        #[cfg(unix)]
        ("status", "exit:256"),
        #[cfg(unix)]
        ("status", "exit:-1"),
        ("status", "exit:01"),
        ("status", "signal:0"),
        ("status", "signal:2147483648"),
        ("cause", "Cancelled(Unknown)"),
        ("cleanup", "okay"),
        ("stdout_issue", "unknown"),
        ("stderr_tail_truncated", "True"),
        ("stdout_bytes", "١"),
    ] {
        assert!(
            parse_cli_summary(format!("failforge: {key}={value}\n").as_bytes()).is_err(),
            "{key}={value}"
        );
    }
    assert!(parse_cli_summary(b"failforge: stdout_bytes=0 stdout_tail_truncated=true\n").is_err());
    assert!(parse_cli_summary(b"failforge: stderr_bytes=0 stderr_tail_truncated=true\n").is_err());
    assert!(parse_cli_summary(b"failforge: execution_ms=340282366920938463463374607431768211455 stdout_bytes=18446744073709551615 status=signal:2147483647\r\n").is_ok());
    assert!(parse_cli_summary(b"failforge: cause=\xff\n").is_err());
}

#[test]
fn missing_done_and_unicode_json_framing_are_exact() {
    let mut rows = events("success");
    rows[6]["test"]["name"] = json!("漢字🙂\u{2028}\u{2029}\u{85}");
    assert_verdict(&classify_rows(&rows, 0), "SURVIVED", "user_tests_passed");
    rows.pop();
    assert_verdict(
        &classify_rows(&rows, 0),
        "INFRA_ERROR",
        "missing_done_event",
    );
    let mut raw = stream(&events("success"));
    raw.extend_from_slice(b"\xff");
    assert_verdict(
        &classify_dart_capture(&raw, 0, receipt(&raw, 0).as_bytes(), ReporterMode::Dart),
        "INFRA_ERROR",
        "malformed_reporter_output",
    );
}

#[test]
fn flutter_tool_events_are_only_allowed_in_flutter_mode() {
    let mut rows = events("success");
    rows[0]["runnerVersion"] = Value::Null;
    rows.insert(
        1,
        json!([{"event":"test.startedProcess","params":{"vmServiceUri":null}}]),
    );
    let raw = stream(&rows);
    assert_verdict(
        &classify_dart_capture(&raw, 0, receipt(&raw, 0).as_bytes(), ReporterMode::Flutter),
        "SURVIVED",
        "user_tests_passed",
    );
    assert_verdict(
        &classify_dart_capture(&raw, 0, receipt(&raw, 0).as_bytes(), ReporterMode::Dart),
        "INFRA_ERROR",
        "invalid_runner_version",
    );
}

#[test]
fn timeout_retains_validated_pending_prefix_despite_interrupted_utf8() {
    let rows = events("success");
    let mut raw = stream(&rows[..7]);
    raw.extend_from_slice(b"{\"type\":\"print\",\"message\":\"\xf0\x9f");
    let verdict = timeout(&raw);
    assert_verdict(&verdict, "TIMEOUT", "runner_timeout");
    assert_eq!(verdict["testsPartial"], true);
    assert_eq!(verdict["tests"]["started"], 1);
    assert_eq!(verdict["reporterComplete"], false);
    assert_eq!(verdict["timeoutProgress"]["eventsParsed"], 7);
    assert_eq!(verdict["timeoutProgress"]["pendingTests"], 1);
    assert_eq!(
        verdict["timeoutProgress"]["stopReason"],
        "interrupted_final_event"
    );
    assert_eq!(
        verdict["timeoutDiagnostics"]["pendingAuthorTests"]["authorTests"][0]["testId"],
        3
    );
}

#[test]
fn timeout_prefix_never_publishes_mutations_of_a_rejected_event() {
    let mut rows = events("success");
    rows.truncate(4);
    let mut done = events("success")[4].clone();
    done["skipped"] = json!(true); // consume sets done before rejecting a synthetic skip.
    rows.push(done);
    let verdict = timeout(&stream(&rows));
    assert_eq!(
        verdict["timeoutProgress"]["diagnostic"],
        "skipped_loading_test"
    );
    assert_eq!(verdict["timeoutProgress"]["pendingLoadingTests"], 1);
    assert_eq!(verdict["timeoutProgress"]["eventsParsed"], 4);
    assert_eq!(verdict["tests"]["loadingPassed"], 0);
    assert_eq!(verdict["loadFailures"], json!([]));
}

#[test]
fn complete_reporter_cannot_override_native_timeout() {
    let verdict = timeout(&stream(&events("success")));
    assert_verdict(&verdict, "TIMEOUT", "runner_timeout");
    assert_eq!(verdict["reporterComplete"], true);
    assert_eq!(verdict["timeoutProgress"]["stopReason"], "reporter_done");
    assert_eq!(verdict["timeoutProgress"]["pendingTests"], 0);
    assert_eq!(verdict["tests"]["passed"], 1);
}

#[test]
fn timeout_stops_permanently_at_rejected_line_with_exact_byte_count() {
    let prefix = stream(&events("success")[..7]);
    for (suffix, stop, reason) in [
        (
            b"{bad".as_slice(),
            "interrupted_final_event",
            "malformed_reporter_output",
        ),
        (
            b"{bad\n",
            "invalid_reporter_event",
            "malformed_reporter_output",
        ),
        (
            b"{\"type\":\"done\",\"type\":\"done\"}",
            "invalid_reporter_event",
            "duplicate_json_key",
        ),
    ] {
        let mut raw = prefix.clone();
        raw.extend_from_slice(suffix);
        let verdict = timeout(&raw);
        assert_eq!(verdict["timeoutProgress"]["stopReason"], stop);
        assert_eq!(verdict["timeoutProgress"]["diagnostic"], reason);
        assert_eq!(verdict["timeoutProgress"]["unparsedBytes"], suffix.len());
        assert_eq!(verdict["timeoutProgress"]["eventsParsed"], 7);
    }
}

fn limited(raw: &[u8], timeout: bool, limits: Limits) -> Value {
    let receipt = RunnerReceipt {
        cause: if timeout {
            "TimedOut"
        } else {
            "ProcessCompleted"
        }
        .to_owned(),
        status: "exit:0".to_owned(),
        cleanup: true,
        capture_issue: false,
        stdout_bytes: raw.len() as u64,
        digest: CaptureDigest::LegacyReceipt,
    };
    classify(
        raw,
        if timeout { 124 } else { 0 },
        &receipt,
        Reporter::new("dart"),
        limits,
    )
}

#[test]
fn limits_include_blank_lines_and_preserve_timeout_prefix_semantics() {
    let raw = stream(&events("success"));
    for timeout in [true, false] {
        assert_verdict(
            &limited(
                &raw,
                timeout,
                Limits {
                    capture: raw.len() - 1,
                    ..Limits::default()
                },
            ),
            "INFRA_ERROR",
            "capture_limit_exceeded",
        );
    }
    let overlines = limited(
        &raw,
        false,
        Limits {
            events: 9,
            ..Limits::default()
        },
    );
    assert_verdict(&overlines, "INFRA_ERROR", "event_limit_exceeded");
    assert_eq!(overlines["tests"]["started"], 0);
    let prefix = limited(
        &raw,
        true,
        Limits {
            events: 7,
            ..Limits::default()
        },
    );
    assert_verdict(&prefix, "TIMEOUT", "runner_timeout");
    assert_eq!(prefix["timeoutProgress"]["eventsParsed"], 7);
    assert_eq!(
        prefix["timeoutProgress"]["stopReason"],
        "event_limit_exceeded"
    );
    let huge_blank = b"       \n";
    // Legacy completed streams ignore whitespace before the event-size check;
    // interrupted captures bound every physical line, including whitespace.
    assert_verdict(
        &limited(
            huge_blank,
            false,
            Limits {
                event: 3,
                ..Limits::default()
            },
        ),
        "INFRA_ERROR",
        "missing_done_event",
    );
    assert_eq!(
        limited(
            huge_blank,
            true,
            Limits {
                event: 3,
                ..Limits::default()
            }
        )["timeoutProgress"]["stopReason"],
        "event_limit_exceeded"
    );
}

#[test]
fn inner_timeout_shape_is_diagnostic_only_and_requires_failed_completion() {
    let mut rows = events("error");
    rows[7]["error"] =
        json!("TimeoutException after 0:00:01.000000: Test timed out after 1 seconds.");
    rows[7]["stackTrace"] = json!("dart:isolate  _RawReceivePort._handleMessage\n");
    for row in &mut rows[7..] {
        row["time"] = json!(1000);
    }
    let verdict = classify_rows(&rows, 1);
    assert_verdict(&verdict, "KILLED", "test_timeout");
    assert_eq!(verdict["timeoutDiagnostics"]["cause"], "not_established");
    assert_eq!(
        verdict["timeoutDiagnostics"]["observedInnerTimeouts"]["total"],
        1
    );
    let prefix = timeout(&stream(&rows[..8]));
    assert_eq!(
        prefix["timeoutDiagnostics"]["observedInnerTimeouts"]["total"],
        1
    );
    assert_eq!(
        prefix["timeoutDiagnostics"]["pendingAuthorTests"]["total"],
        1
    );
    rows[8]["result"] = json!("failure");
    assert_verdict(&classify_rows(&rows, 1), "KILLED", "user_test_failure");
}

#[test]
fn timeout_identity_limits_count_unicode_characters_and_bound_lists() {
    let name = "🙂".repeat(513);
    let suite = "a".repeat(512);
    let diagnostics = timeout_diagnostics(
        "runner_timeout",
        (0..10).map(|id| (id, suite.as_str(), name.as_str(), true, true)),
    );
    for field in ["observedInnerTimeouts", "pendingAuthorTests"] {
        assert_eq!(diagnostics[field]["total"], 10);
        assert_eq!(diagnostics[field]["truncated"], true);
        assert_eq!(
            diagnostics[field]["authorTests"].as_array().unwrap().len(),
            8
        );
        assert_eq!(
            diagnostics[field]["authorTests"][0]["testName"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            512
        );
        assert_eq!(
            diagnostics[field]["authorTests"][0]["identityTruncated"],
            true
        );
    }
    assert_eq!(
        timeout_diagnostics(
            "test_timeout",
            [(0, "suite", "test", false, false)].into_iter()
        )["errorObserved"],
        false
    );
}

fn native_result(raw: &[u8]) -> CommandResult {
    CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: Some(native_status(0)),
        timings: Default::default(),
        stdout: CaptureSummary {
            bytes_read: raw.len() as u64,
            sha256: Some(Sha256::digest(raw).into()),
            ..Default::default()
        },
        stderr: Default::default(),
        cleanup: CleanupReport {
            child_reaped: true,
            output_finished: true,
            group: GroupState::NoLiveMembers { zombies: 0 },
            ..Default::default()
        },
    }
}

#[test]
fn native_owner_and_persisted_receipt_classification_match() {
    for (name, exit) in [("success", 0), ("failure", 1), ("error", 255)] {
        let raw = stream(&events(name));
        let mut result = native_result(&raw);
        result.status = Some(native_status(exit));
        assert_eq!(
            classify_dart_result(&raw, &result, ReporterMode::Dart),
            classify_dart_capture(
                &raw,
                exit,
                receipt(&raw, exit).as_bytes(),
                ReporterMode::Dart
            )
        );
    }
    let raw = stream(&events("success"));
    let mut result = native_result(&raw);
    result.cause = CompletionCause::TimedOut;
    result.status = Some(native_status(124));
    assert_eq!(
        classify_dart_result(&raw, &result, ReporterMode::Dart),
        timeout(&raw)
    );
}

#[test]
fn native_result_faults_cannot_be_hidden_by_valid_reporter_output() {
    let raw = stream(&events("success"));
    let mut mutations: Vec<(CommandResult, &str)> = Vec::new();
    let mut result = native_result(&raw);
    result.cleanup.child_reaped = false;
    mutations.push((result, "incomplete_runner_cleanup"));
    let mut result = native_result(&raw);
    result.stdout.issue = Some(CaptureIssue::FileQuotaExceeded);
    mutations.push((result, "runner_capture_issue"));
    let mut result = native_result(&raw);
    result.cause = CompletionCause::Cancelled(CancelReason::SigTerm);
    mutations.push((result, "runner_not_completed"));
    let mut result = native_result(&raw);
    #[cfg(unix)]
    {
        result.status = Some(std::process::ExitStatus::from_raw(9));
    }
    #[cfg(windows)]
    {
        result.status = Some(native_status(0xc0000005_u32 as i32));
    }
    mutations.push((result, "no_normal_child_exit"));
    let mut result = native_result(&raw);
    result.status = None;
    mutations.push((result, "no_normal_child_exit"));
    let mut result = native_result(&raw);
    result.stdout.bytes_read += 1;
    mutations.push((result, "incomplete_stdout_capture"));
    for (result, reason) in mutations {
        assert_verdict(
            &classify_dart_result(&raw, &result, ReporterMode::Dart),
            "INFRA_ERROR",
            reason,
        );
    }
}

/// The frozen migration oracle is deliberately an external research artifact.
/// Regular unit tests above remain self-contained and do not require Python.
#[test]
#[ignore = "requires FAILFORGE_EVIDENCE_CORPUS pointing to the frozen migration corpus"]
fn frozen_python_evidence_corpus() {
    fn bytes(value: &Value) -> Option<Vec<u8>> {
        match value["kind"].as_str()? {
            "bytes" => Some(
                value["bytes"]
                    .as_array()
                    .expect("numeric byte corpus")
                    .iter()
                    .map(|byte| {
                        u8::try_from(byte.as_u64().expect("byte integer")).expect("byte range")
                    })
                    .collect(),
            ),
            "value" => value["value"]
                .as_str()
                .map(|value| value.as_bytes().to_vec()),
            other => panic!("unknown corpus tag {other}"),
        }
    }
    let path = std::env::var_os("FAILFORGE_EVIDENCE_CORPUS").expect("corpus path required");
    let contents = std::fs::read_to_string(path).expect("read corpus");
    let (mut compared, mut mismatches) = (0usize, Vec::new());
    let mut inapplicable = Vec::new();
    let mut intentional_differences = Vec::new();
    for line in contents.lines() {
        let case: Value = serde_json::from_str(line).expect("valid corpus JSON");
        let id = &case["caseId"];
        let mode = match case["reporterMode"].as_str() {
            Some("dart") => ReporterMode::Dart,
            Some("flutter") => ReporterMode::Flutter,
            _ => {
                assert_eq!(case["expected"]["reason"], "invalid_reporter_mode");
                inapplicable.push(json!({"caseId":id,"reason":"mode is a closed Rust enum"}));
                continue;
            }
        };
        let Some(exit) = case["returncode"]
            .as_i64()
            .and_then(|exit| i32::try_from(exit).ok())
        else {
            assert_eq!(case["expected"]["reason"], "invalid_cli_exit_code");
            inapplicable.push(json!({"caseId":id,"reason":"exit status is a Rust i32"}));
            continue;
        };
        let Some(summary) = bytes(&case["runnerSummary"]) else {
            assert_eq!(case["expected"]["reason"], "invalid_runner_summary");
            inapplicable.push(json!({"caseId":id,"reason":"receipt is a Rust byte slice"}));
            continue;
        };
        let Some(raw) = bytes(&case["stdout"]) else {
            assert_eq!(case["expected"]["reason"], "invalid_capture_type");
            inapplicable.push(json!({"caseId":id,"reason":"capture is a Rust byte slice"}));
            continue;
        };
        let limit =
            |name| usize::try_from(case["limits"][name].as_u64().expect("corpus limit")).unwrap();
        let observed = classify_capture(
            &raw,
            exit,
            &summary,
            mode,
            Limits {
                capture: limit("MAX_CAPTURE_BYTES"),
                event: limit("MAX_EVENT_BYTES"),
                events: limit("MAX_EVENTS"),
            },
        );
        compared += 1;
        if observed != case["expected"] {
            // The integer token guard now refuses lossy f64 conversion before
            // the event-time validator. Only this archived overflow fixture's
            // diagnostic changes; its timeout verdict and all other evidence
            // must remain byte-for-byte equivalent as JSON values.
            if id == 228
                && case["sourceTest"]
                    == "test_dart_timeout_progress.DartTimeoutProgressTests.test_uint64_max_timestamp_is_accepted_but_same_width_overflow_is_rejected"
                && case["expected"]["outcome"] == "TIMEOUT"
                && case["expected"]["timeoutProgress"]["diagnostic"] == "invalid_event_time"
                && observed["timeoutProgress"]["diagnostic"] == "integer_limit_exceeded"
                && raw
                    .windows(20)
                    .any(|token| token == b"18446744073709551616")
            {
                let mut compatible = observed.clone();
                compatible["timeoutProgress"]["diagnostic"] = json!("invalid_event_time");
                assert_eq!(
                    compatible, case["expected"],
                    "only the documented diagnostic may change"
                );
                intentional_differences.push(json!({"caseId":id,"sourceTest":case["sourceTest"],"reason":"reject integer token before lossy numeric conversion","expected":case["expected"],"observed":observed}));
                continue;
            }
            mismatches.push(json!({"caseId":id,"sourceTest":case["sourceTest"],"expected":case["expected"],"observed":observed}));
        }
    }
    let exact = compared - mismatches.len() - intentional_differences.len();
    let report = json!({"compared":compared,"exact":exact,"intentionalDifferences":intentional_differences,"intentionalDifferenceCount":intentional_differences.len(),"inapplicable":inapplicable,"mismatches":mismatches});
    if let Some(path) = std::env::var_os("FAILFORGE_EVIDENCE_PARITY_RESULT") {
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write parity report");
    }
    eprintln!(
        "frozen evidence parity: {compared} compared, {exact} exact, {} intentional differences; excluded typed-only inputs: {}",
        intentional_differences.len(),
        report["inapplicable"]
    );
    assert!(
        intentional_differences.len() <= 1,
        "duplicate intentional difference"
    );
    assert!(
        mismatches.is_empty(),
        "{}",
        serde_json::to_string_pretty(&report).unwrap()
    );
    assert!(compared > 0, "empty corpus");
}

#[test]
fn typed_verdict_rejects_same_length_capture_replacement_before_parsing() {
    let replacement = stream(&events("failure"));
    let mut original = stream(&loading_failure(false));
    assert!(replacement.len() > original.len());
    original.resize(replacement.len(), b' ');
    let mut owner = native_result(&original);
    owner.status = Some(native_status(1));
    assert_verdict(
        &classify_dart_result(&original, &owner, ReporterMode::Dart),
        "LOAD_FAILURE",
        "suite_load_failure_unattributed",
    );
    // A same-size file overwrite would otherwise change an unattributed loading
    // failure into an apparent author assertion failure with a matching exit.
    assert_verdict(
        &classify_dart_capture(
            &replacement,
            1,
            receipt(&replacement, 1).as_bytes(),
            ReporterMode::Dart,
        ),
        "KILLED",
        "user_test_failure",
    );
    let rejected = classify_dart_result(&replacement, &owner, ReporterMode::Dart);
    assert_verdict(&rejected, "INFRA_ERROR", "capture_digest_mismatch");
    assert_eq!(rejected["tests"]["started"], 0);
    owner.cause = CompletionCause::TimedOut;
    assert_verdict(
        &classify_dart_result(&replacement, &owner, ReporterMode::Dart),
        "INFRA_ERROR",
        "capture_digest_mismatch",
    );
}

#[test]
fn missing_native_digest_fails_closed_with_cleanup_and_capture_faults_first() {
    let raw = stream(&events("success"));
    let mut owner = native_result(&raw);
    owner.stdout.sha256 = None;
    assert_verdict(
        &classify_dart_result(&raw, &owner, ReporterMode::Dart),
        "INFRA_ERROR",
        "missing_capture_digest",
    );
    owner.stdout.bytes_read += 1;
    assert_verdict(
        &classify_dart_result(&raw, &owner, ReporterMode::Dart),
        "INFRA_ERROR",
        "incomplete_stdout_capture",
    );
    owner.stdout.issue = Some(CaptureIssue::ReadError("fixture".into()));
    assert_verdict(
        &classify_dart_result(&raw, &owner, ReporterMode::Dart),
        "INFRA_ERROR",
        "runner_capture_issue",
    );
    owner.cleanup.child_reaped = false;
    assert_verdict(
        &classify_dart_result(&raw, &owner, ReporterMode::Dart),
        "INFRA_ERROR",
        "incomplete_runner_cleanup",
    );
}

#[cfg(windows)]
#[test]
fn windows_full_width_exit_codes_are_canonical_and_exception_status_is_not_a_kill() {
    let raw = stream(&events("success"));
    for code in [256, i32::MAX, i32::MIN, -1] {
        assert_eq!(
            parse_cli_summary(receipt(&raw, code).as_bytes()).unwrap()["status"],
            format!("exit:{code}")
        );
    }
    for code in ["2147483648", "-2147483649", "+1", "-0", "-01"] {
        assert!(
            parse_cli_summary(
                receipt(&raw, 0)
                    .replace("exit:0", &format!("exit:{code}"))
                    .as_bytes()
            )
            .is_err()
        );
    }
    let result = classify_dart_capture(
        &raw,
        0xc0000005_u32 as i32,
        receipt(&raw, 0xc0000005_u32 as i32).as_bytes(),
        ReporterMode::Dart,
    );
    assert_verdict(&result, "INFRA_ERROR", "no_normal_child_exit");
}
