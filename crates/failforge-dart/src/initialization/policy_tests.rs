use super::*;
use serde_json::{Value, json};

fn evidence(outcome: &str, signature: Option<&str>) -> InitializationEvidence {
    serde_json::from_value(json!({
        "outcome":outcome,
        "reason":if outcome == "LOAD_FAILURE" {"suite_load_failure_unattributed"} else {"all_tests_passed"},
        "testIdentities":if outcome == "SURVIVED" {
            json!([["test/author_test.dart","author"],["test/other_test.dart","unaffected"]])
        } else {json!([["test/other_test.dart","unaffected"]])},
        "observation": {
            "suites":["test/author_test.dart","test/other_test.dart"],
            "signature":signature.map(|kind| json!({"kind":kind,"signature":[{
                "suitePath":"<workspace>/test/author_test.dart",
                "testName":"loading <workspace>/test/author_test.dart",
                "errors":[{"error":"Bad state: registration failed", "isFailure":false,
                    "stackTrace":"package:subject/subject.dart 1:1 call\n<workspace>/test/author_test.dart 3:1 main\n"}]
            }]})),
            "failedSuite":signature.filter(|kind| *kind == "author_null_check").map(|_| "test/author_test.dart")
        }
    })).unwrap()
}

fn context(kind: &str) -> InitializationContext {
    let baseline = evidence("SURVIVED", None);
    InitializationContext {
        candidate_valid: true,
        expected: baseline.test_identities.clone().unwrap(),
        baseline,
        initial: evidence("LOAD_FAILURE", Some(kind)),
        attempts: vec![],
        cancelled: false,
    }
}

fn append_control(context: &mut InitializationContext) {
    let role = ROLES[context.attempts.len()];
    let evidence = if role == InitializationRole::MutantReplay {
        context.initial.clone()
    } else {
        context.baseline.clone()
    };
    context
        .attempts
        .push(InitializationAttempt { role, evidence });
}

fn complete(kind: &str) -> InitializationContext {
    let mut context = context(kind);
    for _ in ROLES {
        append_control(&mut context);
    }
    context
}

fn assert_terminal(context: &InitializationContext, outcome: &str, reason: &str) {
    let decision = evaluate_initialization(context).unwrap();
    assert_eq!(decision.outcome, outcome);
    assert_eq!(decision.reason, reason);
    assert_eq!(decision.confirmed, outcome == "KILLED");
    assert_eq!(decision.next_role, None);
}

fn alter_signature(evidence: &mut InitializationEvidence, key: &str, value: Value) {
    let mut encoded = serde_json::to_value(evidence.observation.as_ref().unwrap()).unwrap();
    encoded["signature"]["signature"][0]["errors"][0][key] = value;
    evidence.observation = Some(serde_json::from_value(encoded).unwrap());
}

#[test]
fn each_prefix_grants_only_the_next_control_and_all_three_prove_a_kill() {
    for kind in ["subject_stack", "author_null_check"] {
        let mut context = context(kind);
        for role in ROLES {
            let decision = evaluate_initialization(&context).unwrap();
            assert_eq!(decision.next_role, Some(role));
            assert_eq!(decision.outcome, "UNCLASSIFIED");
            assert!(!decision.confirmed);
            assert!(decision.signature_kind.is_some());
            append_control(&mut context);
        }
        assert_terminal(&context, "KILLED", "mutation_caused_initialization_failure");
    }
}

#[test]
fn invalid_candidate_missing_signature_or_wrong_initial_outcome_never_starts_controls() {
    for mode in 0..4 {
        let mut context = context("subject_stack");
        match mode {
            0 => context.candidate_valid = false,
            1 => context.initial.observation = None,
            2 => context.initial.observation.as_mut().unwrap().signature = None,
            3 => {
                context.initial.outcome = "KILLED".into();
                context.initial.observation.as_mut().unwrap().signature = None;
            }
            _ => unreachable!(),
        }
        assert_terminal(
            &context,
            "UNCLASSIFIED",
            "initialization_runtime_evidence_missing",
        );
    }
}

#[test]
fn baseline_must_be_observed_passing_and_match_the_full_expected_multiset() {
    for mode in 0..6 {
        let mut context = context("subject_stack");
        match mode {
            0 => context.baseline.outcome = "LOAD_FAILURE".into(),
            1 => context.baseline.test_identities = None,
            2 => context
                .baseline
                .test_identities
                .as_mut()
                .unwrap()
                .pop()
                .map(|_| ())
                .unwrap(),
            3 => context.baseline.observation = None,
            4 => context
                .expected
                .push(["test/author_test.dart".into(), "author".into()]),
            5 => {
                context.expected[0][0] = "test/unselected.dart".into();
                context.baseline.test_identities = Some(context.expected.clone());
            }
            _ => unreachable!(),
        }
        assert!(evaluate_initialization(&context).is_err(), "mode {mode}");
    }
}

#[test]
fn initial_and_every_control_must_select_exactly_the_baseline_suites() {
    for index in 0..4 {
        let mut context = context("subject_stack");
        for _ in 0..index {
            append_control(&mut context);
        }
        let evidence = if index == 0 {
            &mut context.initial
        } else {
            &mut context.attempts[index - 1].evidence
        };
        evidence.observation.as_mut().unwrap().suites.pop();
        assert_terminal(
            &context,
            "INFRA_ERROR",
            "initialization_suite_selection_changed",
        );
    }
}

#[test]
fn missing_control_observation_is_not_an_empty_matching_catalogue() {
    let mut context = context("subject_stack");
    append_control(&mut context);
    context.attempts[0].evidence.observation = None;
    assert_terminal(
        &context,
        "INFRA_ERROR",
        "initialization_suite_selection_changed",
    );
}

#[test]
fn replay_requires_the_same_complete_signature_and_a_load_failure() {
    for mode in 0..6 {
        let mut context = context("subject_stack");
        append_control(&mut context);
        append_control(&mut context);
        let replay = &mut context.attempts[1].evidence;
        match mode {
            0 | 1 => {
                replay.outcome = if mode == 0 { "TIMEOUT" } else { "SURVIVED" }.into();
                replay.observation.as_mut().unwrap().signature = None;
            }
            2 => replay.observation.as_mut().unwrap().signature = None,
            3 => {
                let observation = replay.observation.as_mut().unwrap();
                observation.signature.as_mut().unwrap().kind = SignatureKind::AuthorNullCheck;
                observation.failed_suite = Some("test/author_test.dart".into());
            }
            4 => alter_signature(replay, "error", json!("Bad state: different")),
            5 => alter_signature(
                replay,
                "stackTrace",
                json!("package:subject/subject.dart 1:2 call"),
            ),
            _ => unreachable!(),
        }
        assert_terminal(&context, "UNCLASSIFIED", "initialization_not_reproduced");
    }
}

#[test]
fn either_original_must_pass_with_exact_identity_order_and_occurrences() {
    for index in [0, 2] {
        for mode in 0..5 {
            let mut context = complete("subject_stack");
            context.attempts.truncate(index + 1);
            let original = &mut context.attempts[index].evidence;
            match mode {
                0 => original.outcome = "KILLED".into(),
                1 => original.outcome = "TIMEOUT".into(),
                2 => original.test_identities = None,
                3 => original.test_identities.as_mut().unwrap().reverse(),
                4 => original
                    .test_identities
                    .as_mut()
                    .unwrap()
                    .push(context.expected[0].clone()),
                _ => unreachable!(),
            }
            let (outcome, reason) = if mode < 2 {
                ("UNCLASSIFIED", "initialization_original_control_failed")
            } else {
                (
                    "INFRA_ERROR",
                    "initialization_author_test_selection_changed",
                )
            };
            assert_terminal(&context, outcome, reason);
        }
    }
}

#[test]
fn null_fallback_allows_only_deficits_in_the_failed_suite_and_preserves_multiplicity() {
    for is_replay in [false, true] {
        for mode in 0..5 {
            let mut context = context("author_null_check");
            if is_replay {
                append_control(&mut context);
                append_control(&mut context);
            }
            let evidence = if is_replay {
                &mut context.attempts[1].evidence
            } else {
                &mut context.initial
            };
            match mode {
                0 => evidence.test_identities = None,
                1 => evidence.test_identities = Some(vec![]),
                2 => {
                    evidence.test_identities = Some(vec![
                        context.expected[1].clone(),
                        context.expected[1].clone(),
                    ])
                }
                3 => {
                    evidence.test_identities =
                        Some(vec![["test/other_test.dart".into(), "renamed".into()]])
                }
                4 => {
                    evidence.test_identities = Some(vec![
                        ["test/author_test.dart".into(), "invented".into()],
                        context.expected[1].clone(),
                    ])
                }
                _ => unreachable!(),
            }
            assert_terminal(
                &context,
                "INFRA_ERROR",
                "initialization_author_test_selection_changed",
            );
        }
    }
}

#[test]
fn null_fallback_can_preserve_duplicate_names_and_partial_failed_suite_registration() {
    let mut context = complete("author_null_check");
    context.expected.insert(0, context.expected[0].clone());
    context.baseline.test_identities = Some(context.expected.clone());
    for index in [0, 2] {
        context.attempts[index].evidence.test_identities = Some(context.expected.clone());
    }
    context
        .initial
        .test_identities
        .as_mut()
        .unwrap()
        .push(context.expected[0].clone());
    context.attempts[1].evidence.test_identities = context.initial.test_identities.clone();
    assert_terminal(&context, "KILLED", "mutation_caused_initialization_failure");
    context.attempts[1]
        .evidence
        .test_identities
        .as_mut()
        .unwrap()
        .extend([context.expected[0].clone(), context.expected[0].clone()]);
    assert!(
        evaluate_initialization(&context).is_err(),
        "a terminal replay cannot be followed by another control"
    );
    context.attempts.truncate(2);
    assert_terminal(
        &context,
        "INFRA_ERROR",
        "initialization_author_test_selection_changed",
    );
}

#[test]
fn legacy_subject_stack_does_not_require_null_fallback_observable_catalogue() {
    let mut context = complete("subject_stack");
    context.initial.test_identities = None;
    context.attempts[1].evidence.test_identities = None;
    assert_terminal(&context, "KILLED", "mutation_caused_initialization_failure");
}

#[test]
fn cancellation_at_any_prefix_never_confirms_and_infrastructure_takes_priority() {
    for count in 0..=3 {
        let mut context = complete("subject_stack");
        context.attempts.truncate(count);
        context.cancelled = true;
        assert_terminal(&context, "CANCELLED", "campaign_cancelled");
        if let Some(last) = context.attempts.last_mut() {
            last.evidence.outcome = "INFRA_ERROR".into();
            last.evidence.reason = "incomplete_runner_cleanup".into();
            last.evidence.observation = None;
            assert_terminal(
                &context,
                "INFRA_ERROR",
                "initialization_incomplete_runner_cleanup",
            );
        }
    }
}

#[test]
fn skipped_reordered_duplicated_and_excess_controls_are_rejected() {
    for roles in [
        vec![InitializationRole::MutantReplay],
        vec![InitializationRole::OriginalAfter],
        vec![
            InitializationRole::OriginalBefore,
            InitializationRole::OriginalBefore,
        ],
        vec![
            InitializationRole::OriginalBefore,
            InitializationRole::OriginalAfter,
        ],
        vec![
            InitializationRole::OriginalBefore,
            InitializationRole::MutantReplay,
            InitializationRole::OriginalAfter,
            InitializationRole::OriginalAfter,
        ],
    ] {
        let mut context = context("subject_stack");
        context.attempts = roles
            .into_iter()
            .map(|role| InitializationAttempt {
                role,
                evidence: context.baseline.clone(),
            })
            .collect();
        assert!(evaluate_initialization(&context).is_err());
    }
}

#[test]
fn no_control_is_accepted_after_an_ineligible_initial_or_a_terminal_control() {
    for mode in 0..3 {
        let mut context = complete("subject_stack");
        match mode {
            0 => context.candidate_valid = false,
            1 => context.attempts[0].evidence.outcome = "KILLED".into(),
            2 => context.attempts[1].evidence.outcome = "TIMEOUT".into(),
            _ => unreachable!(),
        }
        assert!(evaluate_initialization(&context).is_err());
    }
}

#[test]
fn malformed_suite_lists_or_empty_signatures_are_rejected() {
    for mode in 0..5 {
        let mut context = context("subject_stack");
        let observation = context.initial.observation.as_mut().unwrap();
        match mode {
            0 => observation.suites.push(observation.suites[0].clone()),
            1 => observation.suites.reverse(),
            2 => observation.suites = vec!["test/../lib/source.dart".into()],
            3 => observation.suites = vec!["test/\\outside.dart".into()],
            4 => observation.signature.as_mut().unwrap().signature.clear(),
            _ => unreachable!(),
        }
        assert!(evaluate_initialization(&context).is_err());
    }
}

#[test]
fn contradictory_opaque_signature_shapes_cannot_be_used_as_confirmation_evidence() {
    for mode in 0..10 {
        let mut context = context("author_null_check");
        let observation = context.initial.observation.as_mut().unwrap();
        match mode {
            0 => observation.failed_suite = None,
            1 => observation.failed_suite = Some("test/unselected.dart".into()),
            2 => observation.signature = None,
            3 => observation.signature.as_mut().unwrap().signature[0]
                .errors
                .clear(),
            4 => observation.signature.as_mut().unwrap().signature[0].errors[0].is_failure = true,
            5 => {
                let signature = observation.signature.as_mut().unwrap();
                signature.signature.push(signature.signature[0].clone());
            }
            6 => {
                let errors = &mut observation.signature.as_mut().unwrap().signature[0].errors;
                errors.push(errors[0].clone());
            }
            7 => observation.signature.as_mut().unwrap().kind = SignatureKind::SubjectStack,
            8 => {
                observation.suites = vec!["test/directory".into()];
                observation.failed_suite = Some("test/directory".into());
            }
            9 => context.initial.outcome = "SURVIVED".into(),
            _ => unreachable!(),
        }
        assert!(evaluate_initialization(&context).is_err(), "mode {mode}");
    }
}

#[test]
fn identity_budget_is_checked_before_constructing_multisets() {
    let mut context = context("author_null_check");
    context.initial.test_identities = Some(vec![
        ["test/author_test.dart".into(), "author".into()];
        MAX_IDENTITIES + 1
    ]);
    assert!(evaluate_initialization(&context).is_err());
    context.initial.test_identities = None;
    context.expected.resize(
        MAX_IDENTITIES + 1,
        ["test/author_test.dart".into(), "author".into()],
    );
    assert!(evaluate_initialization(&context).is_err());
}

#[test]
fn serialized_policy_context_denies_unknown_fields_and_wrong_shapes() {
    let valid = serde_json::to_value(context("subject_stack")).unwrap();
    for (pointer, value) in [
        ("/extra", json!(true)),
        ("/candidateValid", json!("true")),
        ("/expected", json!([["test/author_test.dart"]])),
        ("/initial/extra", json!(1)),
        ("/initial/observation/extra", json!(1)),
    ] {
        let mut altered = valid.clone();
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        altered.pointer_mut(parent).unwrap()[key] = value;
        assert!(
            serde_json::from_value::<InitializationContext>(altered).is_err(),
            "{pointer}"
        );
    }
}

#[test]
#[ignore = "requires a frozen Python policy oracle supplied by the migration audit"]
fn compare_frozen_python_policy_oracle() {
    let path = std::env::var("FAILFORGE_INITIALIZATION_POLICY_ORACLE").unwrap();
    let cases = std::fs::read_to_string(path).unwrap();
    let mut count = 0;
    for line in cases.lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        let context: InitializationContext =
            serde_json::from_value(case["context"].clone()).unwrap();
        let decision = evaluate_initialization(&context)
            .unwrap_or_else(|error| panic!("case {}: {error}", case["name"]));
        assert_eq!(
            serde_json::to_value(decision).unwrap(),
            case["expected"],
            "case {}",
            case["name"]
        );
        count += 1;
    }
    assert!(count >= 100, "oracle must cover the full migration corpus");
    eprintln!("matched {count} frozen Python policy cases");
}
