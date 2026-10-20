//! Drive native causal decisions without interpreting reporter diagnostics.
//!
//! The owner performs each requested control on a fresh copy, verifies bindings
//! before and after it, and passes only the sealed attempt receipt back here.
use failforge_dart::{
    InitializationHandoff, InitializationRequest, InitializationRole, namespace::NamespaceError,
};
use serde_json::{Value, json};

use crate::dart_namespace::invalid;

const ROLES: [InitializationRole; 3] = [
    InitializationRole::OriginalBefore,
    InitializationRole::MutantReplay,
    InitializationRole::OriginalAfter,
];

pub(crate) struct Confirmation {
    answer: Value,
    handoff: Option<InitializationHandoff>,
    completed: usize,
    terminal: bool,
}

impl Confirmation {
    pub fn new(initial: Value, handoff: Option<InitializationHandoff>) -> Self {
        if initial["outcome"] != "LOAD_FAILURE" {
            return Self {
                answer: initial,
                handoff: None,
                completed: 0,
                terminal: true,
            };
        }
        let mut answer = initial;
        answer["observedOutcome"] = answer["outcome"].clone();
        answer["observedReason"] = answer["reason"].clone();
        answer["outcome"] = "INFRA_ERROR".into();
        answer["reason"] = "missing_native_initialization_evidence".into();
        answer["initializationConfirmation"] = json!({"confirmed":false,"attempts":[]});
        let mut value = Self {
            answer,
            handoff: None,
            completed: 0,
            terminal: false,
        };
        value.accept_handoff(handoff);
        value
    }

    fn accept_handoff(&mut self, handoff: Option<InitializationHandoff>) {
        let Some(handoff) = handoff.filter(|handoff| handoff.decision.is_some()) else {
            self.finish_as("INFRA_ERROR", "missing_native_initialization_evidence");
            return;
        };
        let decision = handoff.decision.as_ref().expect("checked decision");
        if let Some(signature) = decision.signature_kind {
            self.answer["initializationConfirmation"]["signatureKind"] = json!(signature);
        }
        if decision.next_role.is_none() {
            self.answer["initializationConfirmation"]["confirmed"] = decision.confirmed.into();
            self.answer["outcome"] = decision.outcome.clone().into();
            self.answer["reason"] = decision.reason.clone().into();
            self.terminal = true;
        }
        self.handoff = Some(handoff);
    }

    fn finish_as(&mut self, outcome: &str, reason: &str) {
        self.answer["outcome"] = outcome.into();
        self.answer["reason"] = reason.into();
        if let Some(proof) = self.answer.get_mut("initializationConfirmation") {
            proof["confirmed"] = false.into();
        }
        self.handoff = None;
        self.terminal = true;
    }

    pub fn next_request(
        &self,
    ) -> Result<Option<(InitializationRole, InitializationRequest)>, NamespaceError> {
        if self.terminal {
            return Ok(None);
        }
        let handoff = self
            .handoff
            .as_ref()
            .ok_or_else(|| invalid("missing native initialization evidence"))?;
        let role = handoff
            .decision
            .as_ref()
            .and_then(|decision| decision.next_role)
            .filter(|role| ROLES.get(self.completed) == Some(role))
            .ok_or_else(|| invalid("invalid native initialization control order"))?;
        let state = handoff
            .state
            .clone()
            .ok_or_else(|| invalid("missing native initialization continuation"))?;
        let request = match role {
            InitializationRole::OriginalBefore => InitializationRequest::OriginalBefore { state },
            InitializationRole::MutantReplay => InitializationRequest::MutantReplay { state },
            InitializationRole::OriginalAfter => InitializationRequest::OriginalAfter { state },
        };
        Ok(Some((role, request)))
    }

    pub fn observe(
        &mut self,
        role: InitializationRole,
        attempt: Value,
        handoff: Option<InitializationHandoff>,
    ) -> Result<(), NamespaceError> {
        if self.next_request()?.map(|(expected, _)| expected) != Some(role) {
            return Err(invalid("unexpected native initialization control"));
        }
        let outcome = attempt["outcome"]
            .as_str()
            .ok_or_else(|| invalid("missing initialization control outcome"))?
            .to_owned();
        let reason = attempt["reason"]
            .as_str()
            .ok_or_else(|| invalid("missing initialization control reason"))?
            .to_owned();
        let mut recorded = attempt;
        recorded["role"] = json!(role);
        self.answer["initializationConfirmation"]["attempts"]
            .as_array_mut()
            .ok_or_else(|| invalid("invalid initialization confirmation proof"))?
            .push(recorded);
        self.completed += 1;
        match outcome.as_str() {
            "INFRA_ERROR" => {
                self.finish_as("INFRA_ERROR", &format!("initialization_{reason}"));
            }
            "CANCELLED" => self.finish_as("CANCELLED", "campaign_cancelled"),
            _ => self.accept_handoff(handoff),
        }
        Ok(())
    }

    /// Cancellation cannot replace an already established infrastructure error.
    pub fn cancel(&mut self) {
        if !self.terminal || self.answer["outcome"] != "INFRA_ERROR" {
            self.finish_as("CANCELLED", "campaign_cancelled");
        }
    }

    pub fn finish(self) -> Result<Value, NamespaceError> {
        if !self.terminal {
            return Err(invalid("incomplete native initialization confirmation"));
        }
        Ok(self.answer)
    }
}

#[cfg(test)]
#[path = "mutant_confirmation_tests.rs"]
mod tests;
