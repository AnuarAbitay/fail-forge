//! Native author-test catalogues and conservative selection-change attribution.

mod comparison;
mod view;

pub(crate) use comparison::python_string as python_json_string;
pub(crate) use view::{path_uri, resolve_existing};

pub use view::{CatalogSnapshot, analyze_catalogue};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_CONTEXT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CatalogError(pub(crate) &'static str);

/// A parent-owned baseline descriptor and the intended author-test selection.
/// Suppressed, never-started tests are never inserted into observed identities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectionContext {
    #[serde(default)]
    pub expected: Option<Vec<[String; 2]>>,
    #[serde(default)]
    pub baseline: Option<CatalogSnapshot>,
    #[serde(default)]
    pub mutant: Option<Value>,
    pub alias: String,
    #[serde(default)]
    pub initialization: Option<crate::InitializationRequest>,
}

/// Decode a bounded private context without accepting duplicate JSON keys,
/// unknown fields or internally contradictory baseline descriptors.
pub fn parse_selection_context(bytes: &[u8]) -> Result<SelectionContext, CatalogError> {
    let value = crate::test_reporter::parse_json(bytes, MAX_CONTEXT_BYTES)
        .map_err(|_| CatalogError("invalid test selection context"))?;
    let context: SelectionContext = serde_json::from_value(value)
        .map_err(|_| CatalogError("invalid test selection context"))?;
    validate_selection_context(&context)?;
    Ok(context)
}

/// Validate an already-decoded context before starting a child. Native owners
/// keep this value in memory; they do not need to encode a private FD request.
pub fn validate_selection_context(context: &SelectionContext) -> Result<(), CatalogError> {
    if let Some(baseline) = &context.baseline {
        comparison::validate_snapshot(baseline)?;
    }
    crate::initialization::validate_request(context)
        .map_err(|_| CatalogError("invalid initialization context"))?;
    Ok(())
}

/// Apply catalogue policy only after native capture authentication and primary
/// classification. The caller still verifies unchanged execution inputs.
pub fn apply_selection(
    result: &mut Value,
    current: &CatalogSnapshot,
    context: &SelectionContext,
) -> Result<(), CatalogError> {
    let outcome = result
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or(CatalogError("invalid primary test verdict"))?
        .to_owned();
    if !matches!(outcome.as_str(), "KILLED" | "SURVIVED" | "LOAD_FAILURE") {
        return Ok(());
    }
    comparison::validate_snapshot(current)?;
    result["testIdentities"] = serde_json::to_value(&current.identities)
        .map_err(|_| CatalogError("invalid author-test identities"))?;
    if outcome == "LOAD_FAILURE"
        || context
            .expected
            .as_ref()
            .is_none_or(|expected| expected == &current.identities)
    {
        return Ok(());
    }
    let reason = result
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let evidence = if outcome == "KILLED" && reason == "user_test_failure" {
        comparison::selection_evidence(current, context)
    } else {
        None
    };
    let Some((suppression, witness)) = evidence else {
        result["outcome"] = Value::from("INFRA_ERROR");
        result["reason"] = Value::from("author_test_selection_changed");
        return Ok(());
    };
    result["observedOutcome"] = Value::from(outcome);
    result["observedReason"] = Value::from(reason);
    result["testCatalogueSuppression"] = suppression;
    if let Some(witness) = witness {
        result["outcome"] = Value::from("KILLED");
        result["reason"] = Value::from("ordinary_author_failure_with_setup_suppression");
        result["ordinaryFailureWitness"] = witness;
    } else {
        result["outcome"] = Value::from("UNCLASSIFIED");
        result["reason"] = Value::from("author_test_catalogue_incomplete_after_setup_failure");
    }
    Ok(())
}

#[cfg(test)]
mod context_validation_tests {
    use super::*;

    #[test]
    fn typed_context_validation_matches_wire_validation() {
        let valid = SelectionContext {
            alias: "subject".into(),
            ..Default::default()
        };
        assert!(validate_selection_context(&valid).is_ok());
        assert!(parse_selection_context(&serde_json::to_vec(&valid).unwrap()).is_ok());
        let bad_baseline = SelectionContext {
            baseline: Some(CatalogSnapshot {
                raw_sha256: "bad".into(),
                cwd: "/".into(),
                identities: vec![],
                view: None,
            }),
            ..valid.clone()
        };
        let bad_initialization = SelectionContext {
            initialization: Some(crate::InitializationRequest::Initial {
                candidate_valid: true,
                baseline: crate::InitializationEvidence {
                    outcome: "SURVIVED".into(),
                    reason: "user_tests_passed".into(),
                    test_identities: None,
                    observation: None,
                },
            }),
            ..valid
        };
        for context in [bad_baseline, bad_initialization] {
            assert!(validate_selection_context(&context).is_err());
            assert!(parse_selection_context(&serde_json::to_vec(&context).unwrap()).is_err());
        }
    }
}
