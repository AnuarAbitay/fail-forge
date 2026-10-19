//! Native loader observations and ordered original/mutant/original decisions.

mod evidence;
mod policy;

pub use evidence::{
    ConfirmationSignature, InitializationObservation, NormalizedDiagnostic, NormalizedLoadFailure,
    SignatureKind, analyze_initialization_evidence,
};
pub use policy::{
    InitializationAttempt, InitializationContext, InitializationDecision, InitializationEvidence,
    InitializationRole, evaluate_initialization,
};

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ReporterMode, SelectionContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InitializationError(pub(crate) &'static str);

/// An opaque continuation bound to the exact candidate and package namespace.
/// Only the parent controller transports it; test children never inherit it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationState {
    mutant: Value,
    alias: String,
    context: InitializationContext,
}

/// The controller requests the next control; the native policy validates its
/// order before child startup and produces the only permitted continuation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "kebab-case", deny_unknown_fields)]
pub enum InitializationRequest {
    Initial {
        #[serde(rename = "candidateValid")]
        candidate_valid: bool,
        baseline: InitializationEvidence,
    },
    OriginalBefore {
        state: InitializationState,
    },
    MutantReplay {
        state: InitializationState,
    },
    OriginalAfter {
        state: InitializationState,
    },
}

impl InitializationRequest {
    fn continuation(&self) -> Option<(InitializationRole, &InitializationState)> {
        match self {
            Self::Initial { .. } => None,
            Self::OriginalBefore { state } => Some((InitializationRole::OriginalBefore, state)),
            Self::MutantReplay { state } => Some((InitializationRole::MutantReplay, state)),
            Self::OriginalAfter { state } => Some((InitializationRole::OriginalAfter, state)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationHandoff {
    pub observation: InitializationEvidence,
    pub decision: Option<InitializationDecision>,
    pub state: Option<InitializationState>,
}

fn bindings(selection: &SelectionContext) -> Result<(&Value, &[[String; 2]]), InitializationError> {
    let mutant = selection
        .mutant
        .as_ref()
        .filter(|value| value.is_object())
        .ok_or(InitializationError("missing initialization candidate"))?;
    let target = mutant
        .get("file")
        .and_then(Value::as_str)
        .ok_or(InitializationError("missing initialization target"))?;
    crate::validate_relative_target(target)
        .map_err(|_| InitializationError("invalid initialization target"))?;
    if target.len() > 32 * 1024
        || selection.alias.len() > 1024
        || selection.alias.is_empty()
        || selection.alias.contains('\0')
    {
        return Err(InitializationError("invalid initialization binding"));
    }
    let expected = selection
        .expected
        .as_deref()
        .ok_or(InitializationError("missing initialization selection"))?;
    if selection
        .baseline
        .as_ref()
        .is_some_and(|baseline| baseline.identities != expected)
    {
        return Err(InitializationError(
            "initialization baseline selection mismatch",
        ));
    }
    Ok((mutant, expected))
}

/// Validate the private request before any test process can start. A completed
/// or reordered transcript cannot be reused to request another control.
pub(crate) fn validate_request(selection: &SelectionContext) -> Result<(), InitializationError> {
    let Some(request) = &selection.initialization else {
        return Ok(());
    };
    let (mutant, expected) = bindings(selection)?;
    match request {
        InitializationRequest::Initial {
            candidate_valid,
            baseline,
        } => {
            // A baseline-only prefix checks the baseline and identity contract
            // without manufacturing a loader failure or permitting a control.
            evaluate_initialization(&InitializationContext {
                candidate_valid: *candidate_valid,
                expected: expected.to_vec(),
                baseline: baseline.clone(),
                initial: baseline.clone(),
                attempts: Vec::new(),
                cancelled: false,
            })?;
        }
        _ => {
            let (role, state) = request.continuation().expect("control request");
            if state.mutant != *mutant
                || state.alias != selection.alias
                || state.context.expected != expected
                || evaluate_initialization(&state.context)?.next_role != Some(role)
            {
                return Err(InitializationError("invalid initialization continuation"));
            }
        }
    }
    Ok(())
}

/// Build observations from authenticated bytes, then evaluate only the requested
/// control. The caller still verifies source integrity and campaign cancellation
/// before using this private handoff to publish a final mutation verdict.
pub fn initialization_handoff(
    raw: &[u8],
    cwd: &Path,
    mode: ReporterMode,
    classification: &mut Value,
    selection: &SelectionContext,
) -> Result<InitializationHandoff, InitializationError> {
    validate_request(selection)?;
    let outcome = classification["outcome"].as_str();
    let required_observation = matches!(outcome, Some("SURVIVED" | "KILLED" | "LOAD_FAILURE"));
    let observation =
        if required_observation || matches!(outcome, Some("TIMEOUT" | "INVALID" | "NO_TESTS")) {
            match analyze_initialization_evidence(
                raw,
                cwd,
                mode,
                classification,
                selection.mutant.as_ref(),
                &selection.alias,
            ) {
                Ok(observation) => Some(observation),
                Err(_) => {
                    // A timeout or compiler error may have only a reporter prefix.
                    // Preserve that primary outcome, but do not treat the prefix as
                    // a proven full suite selection for initialization controls.
                    if required_observation {
                        classification["outcome"] = "INFRA_ERROR".into();
                        classification["reason"] = "invalid_native_initialization_evidence".into();
                    }
                    None
                }
            }
        } else {
            None
        };
    let evidence = InitializationEvidence {
        outcome: classification["outcome"]
            .as_str()
            .ok_or(InitializationError("missing native outcome"))?
            .into(),
        reason: classification["reason"]
            .as_str()
            .ok_or(InitializationError("missing native reason"))?
            .into(),
        test_identities: classification
            .get("testIdentities")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .map_err(|_| InitializationError("invalid native identities"))?,
        observation,
    };
    let Some(request) = &selection.initialization else {
        return Ok(InitializationHandoff {
            observation: evidence,
            decision: None,
            state: None,
        });
    };
    let (mutant, expected) = bindings(selection)?;
    let context = match request {
        InitializationRequest::Initial {
            candidate_valid,
            baseline,
        } => InitializationContext {
            candidate_valid: *candidate_valid,
            expected: expected.to_vec(),
            baseline: baseline.clone(),
            initial: evidence.clone(),
            attempts: Vec::new(),
            cancelled: false,
        },
        _ => {
            let (role, state) = request.continuation().expect("control request");
            let mut context = state.context.clone();
            context.attempts.push(InitializationAttempt {
                role,
                evidence: evidence.clone(),
            });
            context
        }
    };
    let decision = evaluate_initialization(&context)?;
    let state = decision.next_role.map(|_| InitializationState {
        mutant: mutant.clone(),
        alias: selection.alias.clone(),
        context,
    });
    Ok(InitializationHandoff {
        observation: evidence,
        decision: Some(decision),
        state,
    })
}

#[cfg(test)]
mod tests;
