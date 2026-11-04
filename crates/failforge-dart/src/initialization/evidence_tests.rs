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
        &crate::test_fs::absolute("/missing/initialization"),
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

#[cfg(windows)]
#[test]
fn native_windows_registration_null_check_retains_strict_author_attribution() {
    // Captured from the native Windows registration-confirmation regression.
    let suite = r"test\value_test.dart";
    let observe = |stack: &str, suite: &str| {
        let raw = capture(
            &[(
                suite,
                Some(("Null check operator used on a null value", stack, false)),
            )],
            false,
        );
        let classification = classified(&raw, ReporterMode::Dart);
        assert_eq!(classification["outcome"], "LOAD_FAILURE");
        analyze_initialization_evidence(
            &raw,
            &crate::test_fs::absolute("/missing/windows-registration"),
            ReporterMode::Dart,
            &classification,
            Some(&json!({"file":"lib/value.dart"})),
            "subject",
        )
        .unwrap()
    };
    let actual = observe("test\\value_test.dart 3:35  main\n", suite);
    assert_eq!(actual.suites, ["test/value_test.dart"]);
    assert_eq!(actual.failed_suite.as_deref(), Some("test/value_test.dart"));
    let signature = actual.signature.unwrap();
    assert_eq!(signature.kind, SignatureKind::AuthorNullCheck);
    assert_eq!(
        serde_json::to_value(signature.signature).unwrap(),
        json!([{
            "suitePath":r"test\value_test.dart",
            "testName":r"loading test\value_test.dart",
            "errors":[{
                "error":"Failed to load \"test\\value_test.dart\": Null check operator used on a null value",
                "stackTrace":"test\\value_test.dart 3:35  main\n",
                "isFailure":false
            }]
        }])
    );
    let native_root: PathBuf = crate::test_fs::absolute("/missing/windows-registration")
        .components()
        .collect();
    let native_absolute = native_root.join("test").join("value_test.dart");
    assert!(
        observe(
            &format!("{} 3:35  main\n", native_absolute.display()),
            suite
        )
        .signature
        .is_some()
    );
    for stack in [
        "test\\other_test.dart 3:35  main\n",
        "other\\test\\value_test.dart 3:35  main\n",
        "test\\value_test.dart 3:35  main\ndart:io Socket.connect\n",
    ] {
        assert!(observe(stack, suite).signature.is_none(), "{stack:?}");
    }
    assert!(
        observe(
            "test\\value_test.dart 3:35  main\n",
            r"test\..\test\value_test.dart",
        )
        .signature
        .is_none()
    );
}

#[test]
fn normalization_preserves_error_line_padding_and_replay_identity() {
    let evidence = |cwd: &str, column: u32| {
        let native = crate::test_fs::absolute(cwd);
        let cwd = native.to_str().unwrap();
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
                &crate::test_fs::absolute("/missing/init"),
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
        &crate::test_fs::absolute("/missing/init"),
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
                &crate::test_fs::absolute("/missing/init"),
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
        &crate::test_fs::absolute("/missing/init"),
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
    use crate::test_fs::symlink;
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
    let suite = logical.join("test").join("author_test.dart");
    let suite = suite.to_str().unwrap();
    let stack = format!("package:subject/subject.dart 2:1 init\n{suite} 3:1 main\n");
    let raw = capture(
        &[(suite, Some(("StateError: failed", &stack, false)))],
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
                &crate::test_fs::absolute("/missing/init"),
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
                &crate::test_fs::absolute("/missing/init"),
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
                &crate::test_fs::absolute("/missing/init"),
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
    let directory = Directory::new(&crate::test_fs::absolute("/")).unwrap();
    let root = text(&directory.logical).unwrap();
    assert!(directory.normalize(&root.repeat(5), &mut 16).is_none());
    let mut budget = 32;
    assert_eq!(
        directory
            .normalize(&format!("a{root}b"), &mut budget)
            .as_deref(),
        Some("a<workspace>b")
    );
    assert_eq!(budget, 19);
}

#[cfg(unix)]
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
            &crate::test_fs::absolute(case["cwd"].as_str().unwrap()),
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

// Captured from the same Windows logging mutant and its fresh replay. The VM
// exhausted the stack at different points in the same recursive call cycle.
const LOGGING_OVERFLOW_INITIAL: &str = r###"dart:core                                        _StringBase.startsWith
package:failforge_subject/src/logger.dart 81:14  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
.                                                ...
.                                                ...
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
test\logging_test.dart 294:15                    main.<fn>
package:test_api                                 Declarer.group
package:test_core/src/scaffolding.dart 233:13    group
test\logging_test.dart 292:3                     main
"###;
const LOGGING_OVERFLOW_REPLAY: &str = r###"dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
.                                                ...
.                                                ...
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
package:failforge_subject/src/logger.dart 93:32  new Logger._named
package:failforge_subject/src/logger.dart 67:47  new Logger.<fn>
dart:_compact_hash                               _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16  new Logger
test\logging_test.dart 294:15                    main.<fn>
package:test_api                                 Declarer.group
package:test_core/src/scaffolding.dart 233:13    group
test\logging_test.dart 292:3                     main
"###;
const LOGGING_HEALTHY_IDENTITIES: &str = r###"[
  [
    "test/logging_test.dart",
    "cannot directly manipulate Logger.children"
  ],
  [
    "test/logging_test.dart",
    "default levels are in order"
  ],
  [
    "test/logging_test.dart",
    "detached loggers children is empty"
  ],
  [
    "test/logging_test.dart",
    "detached loggers create new instances of Logger"
  ],
  [
    "test/logging_test.dart",
    "detached loggers have levels independent of the root level"
  ],
  [
    "test/logging_test.dart",
    "detached loggers log messages regardless of hierarchy"
  ],
  [
    "test/logging_test.dart",
    "detached loggers parent is null"
  ],
  [
    "test/logging_test.dart",
    "level comparison is a valid comparator"
  ],
  [
    "test/logging_test.dart",
    "levels are comparable"
  ],
  [
    "test/logging_test.dart",
    "levels are hashable"
  ],
  [
    "test/logging_test.dart",
    "logger full name"
  ],
  [
    "test/logging_test.dart",
    "logger name cannot end with a \".\""
  ],
  [
    "test/logging_test.dart",
    "logger name cannot start with a \".\" "
  ],
  [
    "test/logging_test.dart",
    "logger naming is hierarchical"
  ],
  [
    "test/logging_test.dart",
    "logger parent-child links are correct"
  ],
  [
    "test/logging_test.dart",
    "loggers are singletons"
  ],
  [
    "test/logging_test.dart",
    "mutating levels add/remove handlers - no hierarchy"
  ],
  [
    "test/logging_test.dart",
    "mutating levels add/remove handlers - with hierarchy"
  ],
  [
    "test/logging_test.dart",
    "mutating levels cannot set level if hierarchy is disabled"
  ],
  [
    "test/logging_test.dart",
    "mutating levels cannot set the level to null on a detached logger"
  ],
  [
    "test/logging_test.dart",
    "mutating levels cannot set the level to null on the root logger"
  ],
  [
    "test/logging_test.dart",
    "mutating levels isLoggable is appropriate"
  ],
  [
    "test/logging_test.dart",
    "mutating levels loggers effective level - no hierarchy"
  ],
  [
    "test/logging_test.dart",
    "mutating levels loggers effective level - with changing hierarchy"
  ],
  [
    "test/logging_test.dart",
    "mutating levels loggers effective level - with hierarchy"
  ],
  [
    "test/logging_test.dart",
    "mutating levels logging methods store appropriate level"
  ],
  [
    "test/logging_test.dart",
    "mutating levels logging methods store exception"
  ],
  [
    "test/logging_test.dart",
    "mutating levels message logging - calls toString"
  ],
  [
    "test/logging_test.dart",
    "mutating levels message logging - lazy functions"
  ],
  [
    "test/logging_test.dart",
    "mutating levels message logging - no hierarchy"
  ],
  [
    "test/logging_test.dart",
    "mutating levels message logging - with hierarchy"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel defaults a missing trace"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel defaults an empty trace"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel error also generated when generating a trace"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel listen for level changed"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel no stack trace by default"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel onLevelChanged is not emited if set the level to the same value"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel provided trace is used if given"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel setting level in a loop throws state error"
  ],
  [
    "test/logging_test.dart",
    "recordStackTraceAtLevel trace recorded only on requested levels"
  ],
  [
    "test/logging_test.dart",
    "root level has proper defaults"
  ],
  [
    "test/logging_test.dart",
    "stackTrace gets throw to LogRecord"
  ],
  [
    "test/logging_test.dart",
    "zone gets recorded to LogRecord child zone"
  ],
  [
    "test/logging_test.dart",
    "zone gets recorded to LogRecord custom zone"
  ],
  [
    "test/logging_test.dart",
    "zone gets recorded to LogRecord root zone"
  ]
]"###;

fn logging_overflow_evidence(
    body: &str,
    stack: &str,
) -> crate::initialization::InitializationEvidence {
    #[cfg(windows)]
    let suite = r"test\logging_test.dart";
    #[cfg(unix)]
    let suite = "test/logging_test.dart";
    #[cfg(unix)]
    let native_stack = stack.replace(r"test\logging_test.dart", suite);
    #[cfg(unix)]
    let stack = native_stack.as_str();
    let raw = capture(&[(suite, Some((body, stack, false)))], false);
    let classification = classified(&raw, ReporterMode::Dart);
    let observation = analyze_initialization_evidence(
        &raw,
        &crate::test_fs::absolute("/missing/logging-overflow"),
        ReporterMode::Dart,
        &classification,
        Some(&json!({"file":"lib/src/logger.dart"})),
        "failforge_subject",
    )
    .unwrap();
    crate::initialization::InitializationEvidence {
        outcome: classification["outcome"].as_str().unwrap().into(),
        reason: classification["reason"].as_str().unwrap().into(),
        test_identities: Some(vec![]),
        observation: Some(observation),
    }
}

fn logging_overflow_context(
    body: &str,
    replay_stack: &str,
) -> crate::initialization::InitializationContext {
    use crate::initialization::*;
    let expected: Vec<[String; 2]> = serde_json::from_str(LOGGING_HEALTHY_IDENTITIES).unwrap();
    let baseline = InitializationEvidence {
        outcome: "SURVIVED".into(),
        reason: "user_tests_passed".into(),
        test_identities: Some(expected.clone()),
        observation: Some(InitializationObservation {
            suites: vec!["test/logging_test.dart".into()],
            signature: None,
            failed_suite: None,
        }),
    };
    let initial = logging_overflow_evidence(body, LOGGING_OVERFLOW_INITIAL);
    let replay = logging_overflow_evidence(body, replay_stack);
    InitializationContext {
        candidate_valid: true,
        expected,
        baseline: baseline.clone(),
        initial,
        attempts: vec![
            InitializationAttempt {
                role: InitializationRole::OriginalBefore,
                evidence: baseline,
            },
            InitializationAttempt {
                role: InitializationRole::MutantReplay,
                evidence: replay,
            },
        ],
        cancelled: false,
    }
}

#[test]
fn captured_recursive_overflow_replay_still_requires_both_healthy_originals() {
    use crate::initialization::*;
    assert_ne!(LOGGING_OVERFLOW_INITIAL, LOGGING_OVERFLOW_REPLAY);
    let full = logging_overflow_context("Stack Overflow", LOGGING_OVERFLOW_REPLAY);
    let mut context = full.clone();
    context.attempts.clear();
    for role in [
        InitializationRole::OriginalBefore,
        InitializationRole::MutantReplay,
        InitializationRole::OriginalAfter,
    ] {
        let decision = evaluate_initialization(&context).unwrap();
        assert_eq!(decision.next_role, Some(role));
        assert!(!decision.confirmed);
        let evidence = if role == InitializationRole::MutantReplay {
            full.attempts[1].evidence.clone()
        } else {
            full.baseline.clone()
        };
        context
            .attempts
            .push(InitializationAttempt { role, evidence });
    }
    let decision = evaluate_initialization(&context).unwrap();
    assert!(decision.confirmed);
    assert_eq!(decision.outcome, "KILLED");
    assert_eq!(decision.reason, "mutation_caused_initialization_failure");
}

#[test]
fn recursive_overflow_matching_keeps_cycle_target_author_and_error_boundaries() {
    use crate::initialization::evaluate_initialization;
    let marker = ".                                                ...";
    let (head, tail) = LOGGING_OVERFLOW_REPLAY.split_once(marker).unwrap();
    let changes = [
        LOGGING_OVERFLOW_REPLAY.replace("src/logger.dart", "src/other.dart"),
        LOGGING_OVERFLOW_REPLAY.replace("93:32", "94:32"),
        LOGGING_OVERFLOW_REPLAY.replace("new Logger.<fn>", "new Logger.other"),
        LOGGING_OVERFLOW_REPLAY.replace(
            "_LinkedHashMapMixin.putIfAbsent",
            "_LinkedHashMapMixin.other",
        ),
        LOGGING_OVERFLOW_REPLAY.replace("294:15", "295:15"),
        LOGGING_OVERFLOW_REPLAY.replace(r"test\logging_test.dart", r"test\other_test.dart"),
        format!(
            "{}{}{}",
            head.replace("new Logger.<fn>", "new Logger.other"),
            marker,
            tail
        ),
        format!(
            "{}\n{}{}",
            head.lines().take(4).collect::<Vec<_>>().join("\n"),
            marker,
            tail
        ),
        LOGGING_OVERFLOW_REPLAY.replace(marker, "not an elision marker"),
        format!(
            "{}{}",
            "dart:core temporary\n".repeat(17),
            LOGGING_OVERFLOW_REPLAY
        ),
        format!(
            "{}{}",
            "dart:core temporary
"
            .repeat(257),
            LOGGING_OVERFLOW_REPLAY
        ),
    ];
    for stack in changes {
        let context = logging_overflow_context("Stack Overflow", &stack);
        let decision = evaluate_initialization(&context).unwrap();
        assert!(!decision.confirmed);
        assert_eq!(decision.outcome, "UNCLASSIFIED");
        assert_eq!(decision.reason, "initialization_not_reproduced");
    }
    // A bound target only in the ignored head must not authenticate a foreign cycle.
    let foreign = |stack: &str| {
        format!(
            "package:failforge_subject/src/logger.dart 10:1 entry\n{}",
            stack.replace(
                "package:failforge_subject/src/logger.dart",
                "package:external/loop.dart"
            )
        )
    };
    let mut context = logging_overflow_context("Stack Overflow", &foreign(LOGGING_OVERFLOW_REPLAY));
    context.initial =
        logging_overflow_evidence("Stack Overflow", &foreign(LOGGING_OVERFLOW_INITIAL));
    assert_eq!(
        evaluate_initialization(&context).unwrap().reason,
        "initialization_not_reproduced"
    );
    let context =
        logging_overflow_context("StateError: initialization failed", LOGGING_OVERFLOW_REPLAY);
    let decision = evaluate_initialization(&context).unwrap();
    assert!(!decision.confirmed);
    assert_eq!(decision.reason, "initialization_not_reproduced");
    let mut context = logging_overflow_context("Stack Overflow", LOGGING_OVERFLOW_REPLAY);
    context.attempts[1].evidence =
        logging_overflow_evidence("StateError: initialization failed", LOGGING_OVERFLOW_REPLAY);
    assert_eq!(
        evaluate_initialization(&context).unwrap().reason,
        "initialization_not_reproduced"
    );
}

// This captured overflow enters its recursive cycle through a stable bridge.
// The bridge includes a getter frame without coordinates and is exact evidence.
const LOGGING_BRIDGE_OVERFLOW_INITIAL: &str = r###"dart:core                                         _StringBase.startsWith
package:failforge_subject/src/logger.dart 81:14   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
.                                                 ...
.                                                 ...
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 317:30  Logger.root
package:failforge_subject/src/logger.dart         Logger.root
test\logging_test.dart 293:25                     main.<fn>
package:test_api                                  Declarer.group
package:test_core/src/scaffolding.dart 233:13     group
test\logging_test.dart 292:3                      main
"###;
const LOGGING_BRIDGE_OVERFLOW_REPLAY: &str = r###"dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 93:39   new Logger._named
.                                                 ...
.                                                 ...
package:failforge_subject/src/logger.dart 93:39   new Logger._named
package:failforge_subject/src/logger.dart 67:47   new Logger.<fn>
dart:_compact_hash                                _LinkedHashMapMixin.putIfAbsent
package:failforge_subject/src/logger.dart 67:16   new Logger
package:failforge_subject/src/logger.dart 317:30  Logger.root
package:failforge_subject/src/logger.dart         Logger.root
test\logging_test.dart 293:25                     main.<fn>
package:test_api                                  Declarer.group
package:test_core/src/scaffolding.dart 233:13     group
test\logging_test.dart 292:3                      main
"###;

fn logging_bridge_overflow_context(replay: &str) -> crate::initialization::InitializationContext {
    let mut context = logging_overflow_context("Stack Overflow", replay);
    context.initial = logging_overflow_evidence("Stack Overflow", LOGGING_BRIDGE_OVERFLOW_INITIAL);
    context
}

#[test]
fn captured_overflow_entry_bridge_preserves_ordered_confirmation() {
    use crate::initialization::*;
    assert_ne!(
        LOGGING_BRIDGE_OVERFLOW_INITIAL,
        LOGGING_BRIDGE_OVERFLOW_REPLAY
    );
    let mut context = logging_bridge_overflow_context(LOGGING_BRIDGE_OVERFLOW_REPLAY);
    let decision = evaluate_initialization(&context).unwrap();
    assert_eq!(decision.next_role, Some(InitializationRole::OriginalAfter));
    assert!(!decision.confirmed);
    context.attempts.push(InitializationAttempt {
        role: InitializationRole::OriginalAfter,
        evidence: context.baseline.clone(),
    });
    let decision = evaluate_initialization(&context).unwrap();
    assert!(decision.confirmed);
    assert_eq!(decision.outcome, "KILLED");
    assert_eq!(decision.reason, "mutation_caused_initialization_failure");
}

#[test]
fn overflow_entry_bridge_and_first_tail_cycle_remain_exact_evidence() {
    use crate::initialization::evaluate_initialization;
    let stack = LOGGING_BRIDGE_OVERFLOW_REPLAY;
    let lines: Vec<_> = stack.lines().collect();
    let first_tail = lines
        .iter()
        .position(|line| line.split_ascii_whitespace().eq([".", "..."]))
        .unwrap()
        + 2;
    let changed_tail = |replacement: Vec<&str>| {
        let mut changed = lines[..first_tail].to_vec();
        changed.extend(replacement);
        format!("{}\n", changed.join("\n"))
    };
    let mut mismatch = lines[first_tail..].to_vec();
    mismatch.swap(0, 1);
    let mut short_cycle = lines[first_tail..].to_vec();
    short_cycle.remove(0);
    let mut later_cycle = lines[first_tail..].to_vec();
    drop(later_cycle.splice(0..0, lines[first_tail..first_tail + 4].iter().copied()));
    later_cycle.swap(0, 1);
    let changes = [
        stack.replace("317:30", "318:30"),
        stack.replace("Logger.root", "Logger.other"),
        stack.replace("293:25", "294:25"),
        changed_tail(mismatch),
        changed_tail(short_cycle),
        changed_tail(later_cycle),
    ];
    for changed in changes {
        let context = logging_bridge_overflow_context(&changed);
        let decision = evaluate_initialization(&context).unwrap();
        assert!(!decision.confirmed);
        assert_eq!(decision.outcome, "UNCLASSIFIED");
        assert_eq!(decision.reason, "initialization_not_reproduced");
    }
}
