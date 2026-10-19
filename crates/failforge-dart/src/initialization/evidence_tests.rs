use super::*;
use serde_json::json;

type Suite<'a> = (&'a str, Option<(&'a str, &'a str, bool)>);

fn capture(suites: &[Suite<'_>], flutter: bool) -> Vec<u8> {
    let mut events = vec![
        json!({"type":"start","protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":1,"time":0}),
    ];
    if flutter {
        events.push(json!([{"event":"test.startedProcess","params":{"vmServiceUri":null}}]));
    }
    for (index, (suite, error)) in suites.iter().enumerate() {
        let id = index * 2;
        events
            .push(json!({"type":"suite","suite":{"id":id,"platform":"vm","path":suite},"time":0}));
        events.push(json!({"type":"testStart","test":{"id":id+1,"suiteID":id,"groupIDs":[],"name":format!("loading {suite}"),"metadata":{"skip":false}},"time":0}));
        if let Some((error, stack, failure)) = error {
            events.push(json!({"type":"error","testID":id+1,"error":format!("Failed to load \"{suite}\": {error}"),"stackTrace":stack,"isFailure":failure,"time":0}));
        }
        events.push(json!({"type":"testDone","testID":id+1,"result":if error.is_some(){"error"}else{"success"},"hidden":error.is_none(),"skipped":false,"time":0}));
    }
    events.push(json!({"type":"allSuites","count":suites.len(),"time":0}));
    events.push(
        json!({"type":"done","success":suites.iter().all(|(_, error)|error.is_none()),"time":0}),
    );
    events
        .into_iter()
        .flat_map(|event| format!("{event}\n").into_bytes())
        .collect()
}

fn classified(raw: &[u8], mode: ReporterMode) -> Value {
    let summary = format!(
        "failforge: cause=ProcessCompleted status=exit:1 cleanup=complete stdout_bytes={} stderr_bytes=0 stdout_issue=none stderr_issue=none\n",
        raw.len()
    );
    crate::test_outcome::classify_dart_capture(raw, 1, summary.as_bytes(), mode)
}

fn observed(body: &str, stack: &str) -> InitializationObservation {
    let raw = capture(
        &[("test/author_test.dart", Some((body, stack, false)))],
        false,
    );
    analyze_initialization_evidence(
        &raw,
        Path::new("/missing/initialization"),
        ReporterMode::Dart,
        &classified(&raw, ReporterMode::Dart),
        Some(&json!({"file":"lib/subject.dart"})),
        "subject",
    )
    .unwrap()
}

const STACK: &str =
    "package:subject/subject.dart 2:1 initialize\ntest/author_test.dart 3:1 main.<fn>\n";
const NULL_STACK: &str = "test/author_test.dart 3:1 main.<fn>\npackage:test_api Declarer.group\npackage:test_core/src/scaffolding.dart 233:13 group\ntest/author_test.dart 1:1 main\n";

#[test]
fn runtime_subject_and_later_author_frame_produce_observation_not_verdict() {
    for error in [
        "RangeError (length): wrong",
        "StateError: failed",
        "Bad state: nope",
        "ArgumentError(foo)",
        "Invalid arguments: bad",
        "type 'A' is not a subtype of type B",
        "Stack Overflow",
        "Exception: failed",
    ] {
        let observation = observed(error, STACK);
        assert_eq!(observation.suites, ["test/author_test.dart"]);
        assert_eq!(
            observation.signature.unwrap().kind,
            SignatureKind::SubjectStack,
            "{error}"
        );
        assert_eq!(observation.failed_suite, None);
    }
}

#[test]
fn lookalikes_unknown_runtime_and_exact_stack_overflow_reject() {
    for error in [
        "RangeErrorFactory",
        "StateError_extra",
        "StateErroré",
        "Could not resolve package missing",
        "FileSystemException: failed",
        "StackOverflowError",
        "Stack Overflowed",
        "Stack Overflow\n",
        "Stack Overflow: reason",
        "Not a Stack Overflow",
        "type '' is not a subtype of type B",
        "Invalid arguments",
    ] {
        assert!(observed(error, STACK).signature.is_none(), "{error:?}");
    }
    // Python's boundary considers combining marks non-word; keep this behavior.
    assert!(
        observed("StateError\u{301}: failed", STACK)
            .signature
            .is_some()
    );
}

#[test]
fn strict_frame_identity_order_coordinates_and_boundaries() {
    for stack in [
        "test/author_test.dart 3:1 main\npackage:subject/subject.dart 2:1 initialize\n",
        "package:subject/other.dart 2:1 initialize\ntest/author_test.dart 3:1 main\n",
        "xpackage:subject/subject.dart 2:1 initialize\ntest/author_test.dart 3:1 main\n",
        "package:subject/subject.dart  2:1 initialize\ntest/author_test.dart 3:1 main\n",
        "package:subject/subject.dart 2:1x\ntest/author_test.dart 3:1 main\n",
        "package:subject/subject.dart 2:1 initialize\ntest/other.dart 3:1 main\n",
    ] {
        assert!(
            observed("StateError: failed", stack).signature.is_none(),
            "{stack:?}"
        );
    }
    for stack in [
        STACK,
        "package:subject/subject.dart:2:1\ntest/author_test.dart:3:1\n",
        "package:subject/subject.dart ٠:١ init\ntest/author_test.dart ٢:٣ main\n",
        "package:subject/subject.dart\u{1c}2:1 init\ntest/author_test.dart 3:1 main\n",
    ] {
        assert!(
            observed("StateError: failed", stack).signature.is_some(),
            "{stack:?}"
        );
    }
}

#[test]
fn null_fallback_requires_narrow_author_and_framework_stack() {
    let actual = observed("Null check operator used on a null value", NULL_STACK);
    assert_eq!(
        actual.signature.unwrap().kind,
        SignatureKind::AuthorNullCheck
    );
    assert_eq!(
        actual.failed_suite.as_deref(),
        Some("test/author_test.dart")
    );
    for stack in [
        "",
        "test/author_test.dart 0:1 main\n",
        "test/author_test.dart 3:1 main\ndart:io Socket.connect\n",
        "package:test_api Declarer.group\ntest/author_test.dart 3:1 main\n",
        "test/author_test.dart 3:1 other\n",
        "test/author_test.dart 3:1 main\n\n",
        "test/author_test.dart ٣:١ main\n",
    ] {
        assert!(
            observed("Null check operator used on a null value", stack)
                .signature
                .is_none(),
            "{stack:?}"
        );
    }
    for message in [
        "Null check operator used on a null value ",
        "Null check operator used on a null value\nextra",
        "Exception: Null check operator used on a null value",
    ] {
        assert!(observed(message, NULL_STACK).signature.is_none());
    }
}

#[test]
fn normalization_preserves_error_line_padding_and_replay_identity() {
    let evidence = |cwd: &str, column: u32| {
        let suite = format!("{cwd}/test/author_test.dart");
        let stack =
            format!("package:subject/subject.dart 2:1 initialize\n{suite} 3:{column} main\n");
        let raw = capture(&[(&suite, Some(("StateError: 123", &stack, false)))], false);
        analyze_initialization_evidence(
            &raw,
            Path::new(cwd),
            ReporterMode::Dart,
            &classified(&raw, ReporterMode::Dart),
            Some(&json!({"file":"lib/subject.dart"})),
            "subject",
        )
        .unwrap()
        .signature
    };
    assert_eq!(evidence("/missing/a 12", 1), evidence("/missing/b 34", 1));
    assert_ne!(evidence("/missing/a 12", 1), evidence("/missing/b 34", 2));
    assert!(
        serde_json::to_string(&evidence("/missing/a 12", 1))
            .unwrap()
            .contains("StateError: 123")
    );
}

#[test]
fn unsafe_targets_never_supply_subject_or_null_signatures() {
    for target in [
        "test/author.dart",
        "lib/../evil.dart",
        "lib/./subject.dart",
        "lib//subject.dart",
        "lib/subject.txt",
        "lib/sub\\subject.dart",
        "lib/subject\0.dart",
    ] {
        let stack = format!(
            "package:subject/{} 2:1 initialize\ntest/author_test.dart 3:1 main\n",
            target.strip_prefix("lib/").unwrap_or(target)
        );
        for stack in [&stack, NULL_STACK] {
            let raw = capture(
                &[(
                    "test/author_test.dart",
                    Some(("Null check operator used on a null value", stack, false)),
                )],
                false,
            );
            let actual = analyze_initialization_evidence(
                &raw,
                Path::new("/missing/init"),
                ReporterMode::Dart,
                &classified(&raw, ReporterMode::Dart),
                Some(&json!({"file":target})),
                "subject",
            )
            .unwrap();
            assert!(actual.signature.is_none(), "{target:?}");
        }
    }
}

#[test]
fn suites_are_sorted_complete_and_reject_aliases_escaping_root() {
    let raw = capture(&[("test/z.dart", None), ("test/a.dart", None)], true);
    let actual = analyze_initialization_evidence(
        &raw,
        Path::new("/missing/init"),
        ReporterMode::Flutter,
        &json!({}),
        None,
        "",
    )
    .unwrap();
    assert_eq!(actual.suites, ["test/a.dart", "test/z.dart"]);
    for suites in [
        vec![("test/a.dart", None), ("test/./a.dart", None)],
        vec![("../test/a.dart", None)],
        vec![("lib/a.dart", None)],
    ] {
        assert!(
            analyze_initialization_evidence(
                &capture(&suites, false),
                Path::new("/missing/init"),
                ReporterMode::Dart,
                &json!({}),
                None,
                ""
            )
            .is_err()
        );
    }
}

#[test]
fn redundant_dot_in_suite_is_normalized_before_frame_matching() {
    let stack = "package:subject/subject.dart 2:1 init\ntest/./author_test.dart 3:1 main\n";
    let raw = capture(
        &[(
            "test/./author_test.dart",
            Some(("StateError: failed", stack, false)),
        )],
        false,
    );
    let actual = analyze_initialization_evidence(
        &raw,
        Path::new("/missing/init"),
        ReporterMode::Dart,
        &classified(&raw, ReporterMode::Dart),
        Some(&json!({"file":"lib/subject.dart"})),
        "subject",
    )
    .unwrap();
    assert_eq!(actual.suites, ["test/author_test.dart"]);
    assert!(actual.signature.is_none());
}

#[test]
fn symlink_cwd_preserves_logical_frames_but_checks_physical_containment() {
    use std::os::unix::fs::symlink;
    let temporary = std::env::temp_dir().join(format!("ff-init-symlink-{}", std::process::id()));
    std::fs::create_dir_all(temporary.join("project/test")).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(temporary.clone());
    let logical = temporary.join("logical");
    symlink(temporary.join("project"), &logical).unwrap();
    let suite = format!("{}/test/author_test.dart", logical.display());
    let stack = format!("package:subject/subject.dart 2:1 init\n{suite} 3:1 main\n");
    let raw = capture(
        &[(&suite, Some(("StateError: failed", &stack, false)))],
        false,
    );
    let actual = analyze_initialization_evidence(
        &raw,
        &logical,
        ReporterMode::Dart,
        &classified(&raw, ReporterMode::Dart),
        Some(&json!({"file":"lib/subject.dart"})),
        "subject",
    )
    .unwrap();
    assert_eq!(actual.suites, ["test/author_test.dart"]);
    assert_eq!(actual.signature.unwrap().kind, SignatureKind::SubjectStack);
    std::fs::create_dir(temporary.join("outside")).unwrap();
    symlink(
        temporary.join("outside"),
        temporary.join("project/test/escape"),
    )
    .unwrap();
    let escaping = capture(&[("test/escape/author_test.dart", None)], false);
    assert!(
        analyze_initialization_evidence(
            &escaping,
            &logical,
            ReporterMode::Dart,
            &json!({}),
            None,
            ""
        )
        .is_err()
    );
}

#[test]
fn invalid_incomplete_or_duplicate_json_never_has_suites() {
    let raw = capture(&[("test/author_test.dart", None)], false);
    let rows = raw.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    let incomplete = rows[..rows.len() - 2].join(&b'\n');
    for raw in [
        incomplete,
        b"{\"type\":\"start\",\"type\":\"done\"}\n".to_vec(),
        b"[]\n".to_vec(),
    ] {
        assert!(
            analyze_initialization_evidence(
                &raw,
                Path::new("/missing/init"),
                ReporterMode::Dart,
                &json!({}),
                None,
                ""
            )
            .is_err()
        );
    }
}

#[test]
fn modified_or_partial_primary_evidence_cannot_supply_signature() {
    let raw = capture(
        &[(
            "test/author_test.dart",
            Some(("StateError: fail", STACK, false)),
        )],
        false,
    );
    for change in [
        json!({"outcome":"INFRA_ERROR"}),
        json!({"reporterComplete":false}),
        json!({"loadFailures":[]}),
        json!({"tests":{}}),
    ] {
        let mut classified = classified(&raw, ReporterMode::Dart);
        for (key, value) in change.as_object().unwrap() {
            classified[key] = value.clone();
        }
        assert!(
            analyze_initialization_evidence(
                &raw,
                Path::new("/missing/init"),
                ReporterMode::Dart,
                &classified,
                Some(&json!({"file":"lib/subject.dart"})),
                "subject"
            )
            .unwrap()
            .signature
            .is_none()
        );
    }
}

#[test]
fn null_fallback_rejects_timeout_partial_control_and_noncanonical_runner() {
    let raw = capture(
        &[(
            "test/author_test.dart",
            Some((
                "Null check operator used on a null value",
                NULL_STACK,
                false,
            )),
        )],
        false,
    );
    let base = classified(&raw, ReporterMode::Dart);
    for (key, value) in [
        ("testsPartial", json!(true)),
        ("timeoutDiagnostics", json!({"errorObserved":true})),
        ("phase", json!("author_execution")),
        ("reason", json!("unknown")),
        (
            "runner",
            json!({"cause":"TimedOut","cleanup":"complete","status":"exit:1"}),
        ),
    ] {
        let mut changed = base.clone();
        changed[key] = value;
        assert!(
            analyze_initialization_evidence(
                &raw,
                Path::new("/missing/init"),
                ReporterMode::Dart,
                &changed,
                Some(&json!({"file":"lib/subject.dart"})),
                "subject"
            )
            .unwrap()
            .signature
            .is_none(),
            "{key}"
        );
    }
}

#[test]
fn signature_order_matches_python_ascii_escaped_json() {
    let make = |name: &str| NormalizedLoadFailure {
        suite_path: "test/a.dart".into(),
        test_name: "loading test/a.dart".into(),
        errors: vec![NormalizedDiagnostic {
            error: name.into(),
            stack_trace: STACK.into(),
            is_failure: false,
        }],
    };
    let mut values = [make("é"), make("z"), make("\u{7f}"), make("\u{10000}")];
    values.sort_by_cached_key(signature_sort_key);
    assert_eq!(
        values
            .iter()
            .map(|value| value.errors[0].error.as_str())
            .collect::<Vec<_>>(),
        ["\u{7f}", "é", "\u{10000}", "z"]
    );
    assert!(signature_sort_key(&make("\u{7f}")).contains("\\u007f"));
}

#[test]
fn normalization_budget_stops_short_root_amplification() {
    let directory = Directory::new(Path::new("/")).unwrap();
    assert!(directory.normalize("/////", &mut 16).is_none());
    let mut budget = 32;
    assert_eq!(
        directory.normalize("a/b", &mut budget).as_deref(),
        Some("a<workspace>b")
    );
    assert_eq!(budget, 19);
}

#[test]
#[ignore = "reads frozen Python differential cases from an explicit local path"]
fn frozen_python_initialization_signatures_match() {
    let path = std::env::var("FAILFORGE_INITIALIZATION_ORACLE").expect("oracle case JSONL path");
    let cases = std::fs::read_to_string(path).unwrap();
    let mut checked = 0;
    for line in cases.lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        let raw = case["raw"].as_str().unwrap().as_bytes();
        let mode = if case["mode"] == "flutter" {
            ReporterMode::Flutter
        } else {
            ReporterMode::Dart
        };
        let actual = analyze_initialization_evidence(
            raw,
            Path::new(case["cwd"].as_str().unwrap()),
            mode,
            &case["classification"],
            case.get("mutant"),
            case["alias"].as_str().unwrap(),
        );
        if case["suiteError"] == true {
            assert!(actual.is_err(), "{}", case["name"]);
        } else {
            let actual = actual.unwrap();
            assert_eq!(
                serde_json::to_value(actual.suites).unwrap(),
                case["suites"],
                "{} suites",
                case["name"]
            );
            assert_eq!(
                serde_json::to_value(actual.signature).unwrap(),
                case["signature"],
                "{} signature",
                case["name"]
            );
        }
        checked += 1;
    }
    assert!(checked >= 50, "insufficient frozen cases");
}
