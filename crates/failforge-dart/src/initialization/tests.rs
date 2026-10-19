use super::*;
use serde_json::json;

fn baseline() -> InitializationEvidence {
    InitializationEvidence {
        outcome: "SURVIVED".into(),
        reason: "user_tests_passed".into(),
        test_identities: Some(vec![["test/a.dart".into(), "author".into()]]),
        observation: Some(InitializationObservation {
            suites: vec!["test/a.dart".into()],
            signature: None,
            failed_suite: None,
        }),
    }
}

fn initial() -> InitializationEvidence {
    InitializationEvidence {
        outcome: "LOAD_FAILURE".into(),
        reason: "suite_load_failure_unattributed".into(),
        test_identities: Some(vec![]),
        observation: Some(InitializationObservation {
            suites: vec!["test/a.dart".into()],
            failed_suite: None,
            signature: Some(ConfirmationSignature {
                kind: SignatureKind::SubjectStack,
                signature: vec![NormalizedLoadFailure {
                    suite_path: "test/a.dart".into(),
                    test_name: "loading test/a.dart".into(),
                    errors: vec![NormalizedDiagnostic {
                        error: "Bad state: failed".into(),
                        is_failure: false,
                        stack_trace: "package:subject/a.dart 1:1\ntest/a.dart 1:1".into(),
                    }],
                }],
            }),
        }),
    }
}

fn selection() -> SelectionContext {
    SelectionContext {
        expected: baseline().test_identities,
        mutant: Some(json!({"id":"candidate", "file":"lib/a.dart"})),
        alias: "subject".into(),
        ..SelectionContext::default()
    }
}

fn state() -> InitializationState {
    InitializationState {
        mutant: selection().mutant.unwrap(),
        alias: "subject".into(),
        context: InitializationContext {
            candidate_valid: true,
            expected: baseline().test_identities.unwrap(),
            baseline: baseline(),
            initial: initial(),
            attempts: vec![],
            cancelled: false,
        },
    }
}

#[test]
fn private_requests_are_bound_to_candidate_namespace_selection_and_control_order() {
    let mut context = selection();
    context.initialization = Some(InitializationRequest::OriginalBefore { state: state() });
    validate_request(&context).unwrap();
    let encoded = serde_json::to_vec(&context).unwrap();
    crate::parse_selection_context(&encoded).unwrap();
    for field in [
        "candidate",
        "alias",
        "expected",
        "role",
        "terminal",
        "missing",
    ] {
        let mut changed = context.clone();
        match field {
            "candidate" => changed.mutant.as_mut().unwrap()["id"] = "different".into(),
            "alias" => changed.alias = "different".into(),
            "expected" => changed.expected.as_mut().unwrap()[0][1] = "different".into(),
            "role" => {
                changed.initialization =
                    Some(InitializationRequest::OriginalAfter { state: state() })
            }
            "terminal" => {
                let mut state = state();
                state.context.cancelled = true;
                changed.initialization = Some(InitializationRequest::OriginalBefore { state });
            }
            _ => changed.mutant = None,
        }
        assert!(
            crate::parse_selection_context(&serde_json::to_vec(&changed).unwrap()).is_err(),
            "{field}"
        );
    }
}

#[test]
fn initial_request_requires_a_passing_baseline_with_the_expected_catalogue() {
    let mut context = selection();
    context.initialization = Some(InitializationRequest::Initial {
        candidate_valid: true,
        baseline: baseline(),
    });
    validate_request(&context).unwrap();
    for change in ["outcome", "identities", "suites", "observation"] {
        let mut baseline = baseline();
        match change {
            "outcome" => baseline.outcome = "KILLED".into(),
            "identities" => baseline.test_identities = None,
            "suites" => baseline.observation.as_mut().unwrap().suites.clear(),
            _ => baseline.observation = None,
        }
        context.initialization = Some(InitializationRequest::Initial {
            candidate_valid: true,
            baseline,
        });
        assert!(validate_request(&context).is_err(), "{change}");
    }
}

#[test]
fn malformed_native_capture_never_grants_an_initialization_control() {
    let mut classification = json!({"outcome":"SURVIVED", "reason":"user_tests_passed"});
    let handoff = initialization_handoff(
        b"broken",
        Path::new("/work"),
        ReporterMode::Dart,
        &mut classification,
        &SelectionContext::default(),
    )
    .unwrap();
    assert_eq!(classification["outcome"], "INFRA_ERROR");
    assert!(handoff.observation.observation.is_none());
    assert!(handoff.decision.is_none());
    assert!(handoff.state.is_none());
}

#[test]
fn failed_native_control_terminates_the_opaque_continuation() {
    let mut context = selection();
    context.initialization = Some(InitializationRequest::OriginalBefore { state: state() });
    let mut classification = json!({"outcome":"INFRA_ERROR", "reason":"capture_changed"});
    let handoff = initialization_handoff(
        b"",
        Path::new("/work"),
        ReporterMode::Dart,
        &mut classification,
        &context,
    )
    .unwrap();
    let decision = handoff.decision.unwrap();
    assert_eq!(decision.outcome, "INFRA_ERROR");
    assert_eq!(decision.reason, "initialization_capture_changed");
    assert!(!decision.confirmed);
    assert!(decision.next_role.is_none());
    assert!(handoff.state.is_none());
}

#[test]
fn initialization_request_does_not_exclude_valid_non_library_candidates() {
    let mut context = selection();
    context.mutant.as_mut().unwrap()["file"] = "bin/main.dart".into();
    context.initialization = Some(InitializationRequest::Initial {
        candidate_valid: true,
        baseline: baseline(),
    });
    crate::parse_selection_context(&serde_json::to_vec(&context).unwrap()).unwrap();
    for target in [
        "../lib/a.dart",
        "/lib/a.dart",
        "lib/../a.dart",
        "lib\\a.dart",
    ] {
        context.mutant.as_mut().unwrap()["file"] = target.into();
        assert!(
            crate::parse_selection_context(&serde_json::to_vec(&context).unwrap()).is_err(),
            "{target}"
        );
    }
}

fn completed_reporter() -> Vec<u8> {
    [
        json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":100}),
        json!({"type":"suite","time":1,"suite":{"id":0,"platform":"vm","path":"test/a.dart"}}),
        json!({"type":"group","time":2,"group":{"id":1,"suiteID":0,"parentID":null,"name":"","testCount":1,"metadata":{"skip":false}}}),
        json!({"type":"testStart","time":3,"test":{"id":2,"suiteID":0,"groupIDs":[1],"name":"author","metadata":{"skip":false}}}),
        json!({"type":"testDone","time":5,"testID":2,"result":"success","hidden":false,"skipped":false}),
        json!({"type":"allSuites","time":6,"count":1}),
        json!({"type":"done","time":7,"success":true}),
    ].into_iter().flat_map(|event| {
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n'); bytes
    }).collect()
}

#[test]
fn completed_timeout_control_is_a_failed_original_and_never_a_kill() {
    let mut context = selection();
    context.initialization = Some(InitializationRequest::OriginalBefore { state: state() });
    for outcome in ["TIMEOUT", "INVALID", "NO_TESTS"] {
        let mut classification = json!({"outcome":outcome, "reason":"primary_failure"});
        let handoff = initialization_handoff(
            &completed_reporter(),
            Path::new("/work"),
            ReporterMode::Dart,
            &mut classification,
            &context,
        )
        .unwrap();
        assert_eq!(classification["outcome"], outcome);
        let decision = handoff.decision.unwrap();
        assert_eq!(decision.outcome, "UNCLASSIFIED");
        assert_eq!(decision.reason, "initialization_original_control_failed");
        assert!(!decision.confirmed);
        assert!(handoff.state.is_none());
    }
}

#[test]
fn incomplete_timeout_preserves_primary_outcome_without_proving_suite_selection() {
    let mut context = selection();
    context.initialization = Some(InitializationRequest::OriginalBefore { state: state() });
    let mut bytes = completed_reporter();
    let end = bytes.iter().rposition(|byte| *byte == b'\n').unwrap();
    let start = bytes[..end]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .unwrap();
    bytes.truncate(start + 1);
    let mut classification = json!({"outcome":"TIMEOUT", "reason":"runner_timeout"});
    let handoff = initialization_handoff(
        &bytes,
        Path::new("/work"),
        ReporterMode::Dart,
        &mut classification,
        &context,
    )
    .unwrap();
    assert_eq!(classification["outcome"], "TIMEOUT");
    assert_eq!(classification["reason"], "runner_timeout");
    assert!(handoff.observation.observation.is_none());
    let decision = handoff.decision.unwrap();
    assert_eq!(decision.reason, "initialization_suite_selection_changed");
    assert!(!decision.confirmed);
    assert!(handoff.state.is_none());
}
