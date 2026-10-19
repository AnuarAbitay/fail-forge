//! Pure, replayable A/B/A confirmation policy. Filesystem and process ownership
//! stay with the caller; no single loader diagnostic is itself proof of a kill.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{InitializationError, InitializationObservation, SignatureKind};

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;

const MAX_IDENTITIES: usize = 100_000;

/// Order matters: the same mutation must fail between two fresh passing originals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InitializationRole {
    OriginalBefore,
    MutantReplay,
    OriginalAfter,
}

const ROLES: [InitializationRole; 3] = [
    InitializationRole::OriginalBefore,
    InitializationRole::MutantReplay,
    InitializationRole::OriginalAfter,
];

/// Native evidence after capture authentication, classification and catalogue
/// selection. Missing observations remain missing; policy never invents them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationEvidence {
    pub outcome: String,
    pub reason: String,
    pub test_identities: Option<Vec<[String; 2]>>,
    pub observation: Option<InitializationObservation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationAttempt {
    pub role: InitializationRole,
    pub evidence: InitializationEvidence,
}

/// A bounded transcript, recomputed from the beginning on every control result.
/// The execution owner binds it to one candidate and validates fresh inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationContext {
    pub candidate_valid: bool,
    pub expected: Vec<[String; 2]>,
    pub baseline: InitializationEvidence,
    pub initial: InitializationEvidence,
    pub attempts: Vec<InitializationAttempt>,
    #[serde(default)]
    pub cancelled: bool,
}

/// `next_role` grants only permission for the next control, never a final kill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationDecision {
    pub next_role: Option<InitializationRole>,
    pub outcome: String,
    pub reason: String,
    pub confirmed: bool,
    pub signature_kind: Option<SignatureKind>,
}

impl InitializationDecision {
    fn pending() -> Self {
        Self {
            next_role: None,
            outcome: "UNCLASSIFIED".into(),
            reason: "initialization_runtime_evidence_missing".into(),
            confirmed: false,
            signature_kind: None,
        }
    }

    fn finish(mut self, outcome: &str, reason: &str) -> Self {
        self.next_role = None;
        self.outcome = outcome.into();
        self.reason = reason.into();
        self
    }
}

/// Evaluate eligibility and every completed control. A malformed transcript is
/// rejected rather than silently dropping a duplicated, reordered or late result.
pub fn evaluate_initialization(
    context: &InitializationContext,
) -> Result<InitializationDecision, InitializationError> {
    validate_transcript(context)?;
    let mut decision = InitializationDecision::pending();
    let baseline = baseline_observation(context)?;
    let initial = &context.initial;
    let signature = initial
        .observation
        .as_ref()
        .and_then(|observation| observation.signature.as_ref());
    let Some(signature) =
        signature.filter(|_| context.candidate_valid && initial.outcome == "LOAD_FAILURE")
    else {
        return terminal(decision, 0, context);
    };
    decision.signature_kind = Some(signature.kind);
    if !same_suites(initial, baseline) {
        return terminal(
            decision.finish("INFRA_ERROR", "initialization_suite_selection_changed"),
            0,
            context,
        );
    }
    if !observable_catalogue_matches(initial, &context.expected, signature.kind) {
        return terminal(
            decision.finish(
                "INFRA_ERROR",
                "initialization_author_test_selection_changed",
            ),
            0,
            context,
        );
    }

    for (index, attempt) in context.attempts.iter().enumerate() {
        let evidence = &attempt.evidence;
        // Preserve infrastructure evidence even if cancellation arrived during
        // this same attempt. The controller checks cancellation before starting.
        if evidence.outcome == "INFRA_ERROR" {
            return terminal(
                decision.finish(
                    "INFRA_ERROR",
                    &format!("initialization_{}", evidence.reason),
                ),
                index + 1,
                context,
            );
        }
        if context.cancelled && index + 1 == context.attempts.len() {
            return terminal(
                decision.finish("CANCELLED", "campaign_cancelled"),
                index + 1,
                context,
            );
        }
        if !same_suites(evidence, baseline) {
            return terminal(
                decision.finish("INFRA_ERROR", "initialization_suite_selection_changed"),
                index + 1,
                context,
            );
        }
        match attempt.role {
            InitializationRole::OriginalBefore | InitializationRole::OriginalAfter => {
                if evidence.outcome != "SURVIVED" {
                    return terminal(
                        decision.finish("UNCLASSIFIED", "initialization_original_control_failed"),
                        index + 1,
                        context,
                    );
                }
                if evidence.test_identities.as_ref() != Some(&context.expected) {
                    return terminal(
                        decision.finish(
                            "INFRA_ERROR",
                            "initialization_author_test_selection_changed",
                        ),
                        index + 1,
                        context,
                    );
                }
            }
            InitializationRole::MutantReplay => {
                if evidence.outcome != "LOAD_FAILURE"
                    || evidence
                        .observation
                        .as_ref()
                        .and_then(|observation| observation.signature.as_ref())
                        != Some(signature)
                {
                    return terminal(
                        decision.finish("UNCLASSIFIED", "initialization_not_reproduced"),
                        index + 1,
                        context,
                    );
                }
                if !observable_catalogue_matches(evidence, &context.expected, signature.kind) {
                    return terminal(
                        decision.finish(
                            "INFRA_ERROR",
                            "initialization_author_test_selection_changed",
                        ),
                        index + 1,
                        context,
                    );
                }
            }
        }
    }
    if context.cancelled {
        return Ok(decision.finish("CANCELLED", "campaign_cancelled"));
    }
    if let Some(role) = ROLES.get(context.attempts.len()) {
        decision.next_role = Some(*role);
        return Ok(decision);
    }
    decision.confirmed = true;
    Ok(decision.finish("KILLED", "mutation_caused_initialization_failure"))
}

fn terminal(
    decision: InitializationDecision,
    consumed: usize,
    context: &InitializationContext,
) -> Result<InitializationDecision, InitializationError> {
    if consumed != context.attempts.len() {
        return Err(InitializationError(
            "control after terminal initialization decision",
        ));
    }
    Ok(decision)
}

fn validate_transcript(context: &InitializationContext) -> Result<(), InitializationError> {
    if context.attempts.len() > ROLES.len()
        || context
            .attempts
            .iter()
            .zip(ROLES)
            .any(|(attempt, role)| attempt.role != role)
        || context.expected.len() > MAX_IDENTITIES
    {
        return Err(InitializationError(
            "invalid initialization control transcript",
        ));
    }
    for evidence in std::iter::once(&context.baseline)
        .chain(std::iter::once(&context.initial))
        .chain(context.attempts.iter().map(|attempt| &attempt.evidence))
    {
        if evidence
            .test_identities
            .as_ref()
            .is_some_and(|identities| identities.len() > MAX_IDENTITIES)
        {
            return Err(InitializationError(
                "initialization identity limit exceeded",
            ));
        }
        if let Some(observation) = &evidence.observation {
            if observation.suites.len() > MAX_IDENTITIES
                || !observation.suites.windows(2).all(|pair| pair[0] < pair[1])
                || observation.suites.iter().any(|suite| !author_suite(suite))
            {
                return Err(InitializationError(
                    "invalid initialization suite catalogue",
                ));
            }
            if let Some(signature) = &observation.signature {
                if evidence.outcome != "LOAD_FAILURE"
                    || signature.signature.is_empty()
                    || signature.signature.iter().any(|failure| {
                        failure.errors.is_empty()
                            || failure.errors.iter().any(|error| error.is_failure)
                    })
                {
                    return Err(InitializationError("invalid initialization signature"));
                }
                let valid_kind = match signature.kind {
                    SignatureKind::SubjectStack => observation.failed_suite.is_none(),
                    SignatureKind::AuthorNullCheck => {
                        signature.signature.len() == 1
                            && signature.signature[0].errors.len() == 1
                            && observation.failed_suite.as_ref().is_some_and(|suite| {
                                suite.ends_with(".dart")
                                    && observation.suites.binary_search(suite).is_ok()
                            })
                    }
                };
                if !valid_kind {
                    return Err(InitializationError(
                        "inconsistent initialization signature kind",
                    ));
                }
            } else if observation.failed_suite.is_some() {
                return Err(InitializationError(
                    "failed suite without initialization signature",
                ));
            }
        }
    }
    Ok(())
}

fn baseline_observation(
    context: &InitializationContext,
) -> Result<&InitializationObservation, InitializationError> {
    let baseline = &context.baseline;
    let observation = baseline.observation.as_ref().ok_or(InitializationError(
        "missing initialization baseline observation",
    ))?;
    if baseline.outcome != "SURVIVED"
        || baseline.test_identities.as_ref() != Some(&context.expected)
        || context
            .expected
            .iter()
            .any(|row| observation.suites.binary_search(&row[0]).is_err())
    {
        return Err(InitializationError("invalid initialization baseline"));
    }
    Ok(observation)
}

fn author_suite(path: &str) -> bool {
    path.starts_with("test/")
        && !path.contains(['\\', '\0'])
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn same_suites(evidence: &InitializationEvidence, baseline: &InitializationObservation) -> bool {
    evidence
        .observation
        .as_ref()
        .is_some_and(|observation| observation.suites == baseline.suites)
}

fn observable_catalogue_matches(
    evidence: &InitializationEvidence,
    expected: &[[String; 2]],
    kind: SignatureKind,
) -> bool {
    if kind != SignatureKind::AuthorNullCheck {
        return true;
    }
    let Some(observation) = &evidence.observation else {
        return false;
    };
    let Some(failed_suite) = &observation.failed_suite else {
        return false;
    };
    if observation.suites.binary_search(failed_suite).is_err() {
        return false;
    }
    let Some(observed) = &evidence.test_identities else {
        return false;
    };
    // Names can repeat within one suite. Subtract occurrences, not a set of
    // names: invented duplicates must never turn a partial catalogue into proof.
    let mut remaining = HashMap::<&[String; 2], usize>::new();
    for row in expected {
        *remaining.entry(row).or_default() += 1;
    }
    for row in observed {
        let Some(count) = remaining.get_mut(row) else {
            return false;
        };
        let Some(next) = count.checked_sub(1) else {
            return false;
        };
        *count = next;
    }
    remaining
        .iter()
        .all(|(row, count)| *count == 0 || row[0] == *failed_suite)
}
