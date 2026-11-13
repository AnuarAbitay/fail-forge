use super::*;
use failforge_dart::{
    InitializationAttempt, InitializationContext, InitializationEvidence, SelectionContext,
    evaluate_initialization, validate_selection_context,
};

fn evidence(outcome: &str, signature: bool) -> InitializationEvidence {
    serde_json::from_value(json!({
        "outcome":outcome,
        "reason":if outcome == "LOAD_FAILURE" {"suite_load_failure_unattributed"} else {"user_tests_passed"},
        "testIdentities":if outcome == "SURVIVED" {json!([["test/a_test.dart","author"]])} else {json!([])},
        "observation":{
            "suites":["test/a_test.dart"],"failedSuite":null,
            "signature":if signature {json!({"kind":"subject_stack","signature":[{
                "suitePath":"test/a_test.dart","testName":"loading test/a_test.dart",
                "errors":[{"error":"Bad state: failed","isFailure":false,
                    "stackTrace":"package:subject/a.dart 1:1\ntest/a_test.dart 1:1"}]
            }]})} else {Value::Null}
        }
    }))
    .unwrap()
}

fn context() -> InitializationContext {
    let baseline = evidence("SURVIVED", false);
    InitializationContext {
        candidate_valid: true,
        expected: baseline.test_identities.clone().unwrap(),
        baseline,
        initial: evidence("LOAD_FAILURE", true),
        attempts: Vec::new(),
        cancelled: false,
    }
}

fn mutant() -> Value {
    json!({"id":"candidate","file":"lib/a.dart"})
}

fn public(evidence: &InitializationEvidence) -> Value {
    json!({"outcome":evidence.outcome,"reason":evidence.reason,
        "testIdentities":evidence.test_identities,"temporaryCopyCleaned":true})
}

fn handoff(context: &InitializationContext) -> InitializationHandoff {
    let decision = evaluate_initialization(context).unwrap();
    InitializationHandoff {
        observation: context
            .attempts
            .last()
            .map(|attempt| &attempt.evidence)
            .unwrap_or(&context.initial)
            .clone(),
        state: decision.next_role.map(|_| {
            serde_json::from_value(json!({"mutant":mutant(),"alias":"subject","context":context}))
                .unwrap()
        }),
        decision: Some(decision),
    }
}

fn append(
    context: &mut InitializationContext,
    evidence: InitializationEvidence,
) -> InitializationRole {
    let role = ROLES[context.attempts.len()];
    context
        .attempts
        .push(InitializationAttempt { role, evidence });
    role
}

#[test]
fn only_the_complete_native_original_mutant_original_sequence_confirms_a_kill() {
    let mut context = context();
    let initial = public(&context.initial);
    let mut confirmation = Confirmation::new(initial.clone(), Some(handoff(&context)));
    for role in ROLES {
        let (requested, request) = confirmation.next_request().unwrap().unwrap();
        assert_eq!(requested, role);
        validate_selection_context(&SelectionContext {
            expected: Some(context.expected.clone()),
            mutant: Some(mutant()),
            alias: "subject".into(),
            initialization: Some(request),
            ..SelectionContext::default()
        })
        .unwrap();
        let observed = if role == InitializationRole::MutantReplay {
            context.initial.clone()
        } else {
            context.baseline.clone()
        };
        append(&mut context, observed.clone());
        confirmation
            .observe(role, public(&observed), Some(handoff(&context)))
            .unwrap();
    }
    assert!(confirmation.next_request().unwrap().is_none());
    let final_value = confirmation.finish().unwrap();
    assert_eq!(final_value["outcome"], "KILLED");
    assert_eq!(
        final_value["reason"],
        "mutation_caused_initialization_failure"
    );
    assert_eq!(final_value["observedOutcome"], initial["outcome"]);
    assert_eq!(final_value["observedReason"], initial["reason"]);
    assert_eq!(final_value["temporaryCopyCleaned"], true);
    let proof = &final_value["initializationConfirmation"];
    assert_eq!(proof["confirmed"], true);
    assert_eq!(proof["signatureKind"], "subject_stack");
    assert_eq!(proof["attempts"].as_array().unwrap().len(), 3);
    for (index, role) in ROLES.iter().enumerate() {
        assert_eq!(proof["attempts"][index]["role"], json!(role));
    }
}

#[test]
fn native_terminal_initial_decision_never_schedules_a_control() {
    let mut context = context();
    context.initial.observation.as_mut().unwrap().signature = None;
    let confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
    assert!(confirmation.next_request().unwrap().is_none());
    let answer = confirmation.finish().unwrap();
    assert_eq!(answer["outcome"], "UNCLASSIFIED");
    assert_eq!(answer["reason"], "initialization_runtime_evidence_missing");
    assert_eq!(answer["initializationConfirmation"]["attempts"], json!([]));
}

#[test]
fn other_initial_outcomes_are_preserved_without_controls() {
    for outcome in [
        "SURVIVED",
        "KILLED",
        "TIMEOUT",
        "INVALID",
        "NO_TESTS",
        "INFRA_ERROR",
        "CANCELLED",
    ] {
        let initial = json!({"outcome":outcome,"reason":"initial_reason"});
        let confirmation = Confirmation::new(initial.clone(), None);
        assert!(confirmation.next_request().unwrap().is_none(), "{outcome}");
        assert_eq!(confirmation.finish().unwrap(), initial);
    }
}

#[test]
fn missing_initial_native_evidence_is_infrastructure_failure() {
    let context = context();
    let mut no_decision = handoff(&context);
    no_decision.decision = None;
    for evidence in [None, Some(no_decision)] {
        let mut confirmation = Confirmation::new(public(&context.initial), evidence);
        confirmation.cancel();
        assert!(confirmation.next_request().unwrap().is_none());
        let answer = confirmation.finish().unwrap();
        assert_eq!(answer["outcome"], "INFRA_ERROR");
        assert_eq!(answer["reason"], "missing_native_initialization_evidence");
        assert_eq!(answer["observedOutcome"], "LOAD_FAILURE");
    }
}

#[test]
fn failed_original_or_nonreproducing_mutant_stops_at_the_native_terminal_decision() {
    for failure_index in [0, 1] {
        let mut context = context();
        let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
        for index in 0..=failure_index {
            let observed = if index == failure_index && index == 0 {
                evidence("KILLED", false)
            } else {
                context.baseline.clone()
            };
            let role = append(&mut context, observed.clone());
            confirmation
                .observe(role, public(&observed), Some(handoff(&context)))
                .unwrap();
        }
        assert!(confirmation.next_request().unwrap().is_none());
        let answer = confirmation.finish().unwrap();
        assert_eq!(answer["outcome"], "UNCLASSIFIED");
        assert_eq!(answer["initializationConfirmation"]["confirmed"], false);
        assert_eq!(
            answer["initializationConfirmation"]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            failure_index + 1
        );
    }
}

#[test]
fn integrity_override_supersedes_pending_native_policy_and_cancellation() {
    let context = context();
    for reason in [
        "capture_changed",
        "source_input_changed",
        "workspace_cleanup_failed",
    ] {
        let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
        confirmation
            .observe(
                InitializationRole::OriginalBefore,
                json!({"outcome":"INFRA_ERROR","reason":reason}),
                Some(handoff(&context)),
            )
            .unwrap();
        confirmation.cancel();
        let answer = confirmation.finish().unwrap();
        assert_eq!(answer["outcome"], "INFRA_ERROR");
        assert_eq!(answer["reason"], format!("initialization_{reason}"));
        assert_eq!(
            answer["initializationConfirmation"]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn cancelled_control_does_not_consume_stale_continuation_evidence() {
    let context = context();
    let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
    confirmation
        .observe(
            InitializationRole::OriginalBefore,
            json!({"outcome":"CANCELLED","reason":"campaign_cancelled"}),
            Some(handoff(&context)),
        )
        .unwrap();
    assert!(confirmation.next_request().unwrap().is_none());
    let answer = confirmation.finish().unwrap();
    assert_eq!(answer["outcome"], "CANCELLED");
    assert_eq!(answer["initializationConfirmation"]["confirmed"], false);
}

#[test]
fn cancellation_between_controls_or_before_final_publication_revokes_confirmation() {
    for completed in 0..=3 {
        let mut context = context();
        let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
        for role in ROLES.iter().copied().take(completed) {
            let observed = if role == InitializationRole::MutantReplay {
                context.initial.clone()
            } else {
                context.baseline.clone()
            };
            append(&mut context, observed.clone());
            confirmation
                .observe(role, public(&observed), Some(handoff(&context)))
                .unwrap();
        }
        confirmation.cancel();
        assert!(confirmation.next_request().unwrap().is_none());
        let answer = confirmation.finish().unwrap();
        assert_eq!(answer["outcome"], "CANCELLED");
        assert_eq!(answer["initializationConfirmation"]["confirmed"], false);
        assert_eq!(
            answer["initializationConfirmation"]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            completed
        );
    }
}

#[test]
fn missing_control_evidence_stops_without_requesting_another_control() {
    let context = context();
    let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
    confirmation
        .observe(
            InitializationRole::OriginalBefore,
            public(&context.baseline),
            None,
        )
        .unwrap();
    assert!(confirmation.next_request().unwrap().is_none());
    let answer = confirmation.finish().unwrap();
    assert_eq!(answer["outcome"], "INFRA_ERROR");
    assert_eq!(answer["reason"], "missing_native_initialization_evidence");
}

#[test]
fn control_order_continuation_and_completion_are_mandatory() {
    let context = context();
    let mut wrong_role = handoff(&context);
    wrong_role.decision.as_mut().unwrap().next_role = Some(InitializationRole::OriginalAfter);
    assert!(
        Confirmation::new(public(&context.initial), Some(wrong_role))
            .next_request()
            .is_err()
    );
    let mut missing_state = handoff(&context);
    missing_state.state = None;
    assert!(
        Confirmation::new(public(&context.initial), Some(missing_state))
            .next_request()
            .is_err()
    );
    let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
    assert!(
        confirmation
            .observe(
                InitializationRole::MutantReplay,
                public(&context.initial),
                None
            )
            .is_err()
    );
    assert_eq!(
        confirmation.next_request().unwrap().unwrap().0,
        InitializationRole::OriginalBefore
    );
    assert!(confirmation.finish().is_err());
}

#[test]
fn a_fourth_control_and_results_after_terminal_decisions_are_rejected() {
    let mut context = context();
    let mut confirmation = Confirmation::new(public(&context.initial), Some(handoff(&context)));
    for role in ROLES {
        let observed = if role == InitializationRole::MutantReplay {
            context.initial.clone()
        } else {
            context.baseline.clone()
        };
        append(&mut context, observed.clone());
        let mut next = handoff(&context);
        if role == InitializationRole::OriginalAfter {
            next.decision.as_mut().unwrap().next_role = Some(InitializationRole::OriginalBefore);
        }
        confirmation
            .observe(role, public(&observed), Some(next))
            .unwrap();
    }
    assert!(confirmation.next_request().is_err());
    assert!(confirmation.finish().is_err());

    let mut terminal = Confirmation::new(
        json!({"outcome":"INFRA_ERROR","reason":"capture_changed"}),
        None,
    );
    assert!(
        terminal
            .observe(
                InitializationRole::OriginalBefore,
                public(&context.baseline),
                None
            )
            .is_err()
    );
    terminal.cancel();
    assert_eq!(terminal.finish().unwrap()["reason"], "capture_changed");
}
