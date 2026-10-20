//! Reuse a complete discovery transcript only after rechecking every source proof.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

use super::{DiscoveryError, MAX_TRANSCRIPT_BYTES, validation, verify_candidate};
use crate::{MAX_LINE_BYTES, MutationFamily};

#[derive(Debug, Clone)]
pub struct RetainedTranscript {
    pub steps: BTreeMap<String, Value>,
    pub candidates: Vec<Value>,
}

fn require(condition: bool, message: &'static str) -> Result<(), DiscoveryError> {
    if condition {
        Ok(())
    } else {
        Err(DiscoveryError::Invalid(message))
    }
}

/// Validate the original LF-delimited transcript without starting an adapter.
/// Static validity is retained evidence; byte applicability is independently
/// reconstructed against `project` for every valid and invalid candidate.
pub fn load_retained_transcript(
    bytes: &[u8],
    project: &Path,
    family: &str,
) -> Result<RetainedTranscript, DiscoveryError> {
    let family: MutationFamily = serde_json::from_value(Value::String(family.into()))
        .map_err(|_| DiscoveryError::Invalid("unsupported retained mutation family"))?;
    require(
        bytes.len() <= MAX_TRANSCRIPT_BYTES && bytes.ends_with(b"\n"),
        "invalid retained transcript framing or size",
    )?;
    let mut steps = BTreeMap::new();
    let mut candidates = Vec::new();
    let mut phase = "handshake";
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        require(
            line.len() <= MAX_LINE_BYTES,
            "oversized retained adapter record",
        )?;
        let event = failforge_workspace::parse_json(line)
            .map_err(|_| DiscoveryError::Invalid("invalid retained adapter JSON"))?;
        let step = event["step"]
            .as_str()
            .ok_or(DiscoveryError::Invalid("missing retained adapter step"))?;
        require(
            event["payload"].is_object(),
            "retained payload must be an object",
        )?;
        match phase {
            "handshake" | "discover" => {
                require(step == phase, "retained adapter steps are out of order")?;
                phase = if step == "handshake" {
                    "discover"
                } else {
                    "candidates"
                };
            }
            "candidates" => require(
                matches!(step, "candidate" | "cleanup"),
                "unexpected retained adapter step",
            )?,
            _ => {
                return Err(DiscoveryError::Invalid(
                    "retained adapter cleanup must be terminal",
                ));
            }
        }
        if step == "candidate" {
            require(candidates.len() < 10_000, "too many retained candidates")?;
            candidates.push(event["payload"].clone());
        } else {
            require(
                steps
                    .insert(step.to_owned(), event["payload"].clone())
                    .is_none(),
                "duplicate retained adapter step",
            )?;
        }
        if step == "cleanup" {
            phase = "complete";
        }
    }
    require(phase == "complete", "missing retained adapter cleanup")?;
    let cleanup = &steps["cleanup"];
    for key in [
        "complete",
        "campaignSucceeded",
        "adapterCloseSucceeded",
        "runnerShutdown",
    ] {
        require(
            cleanup[key].as_bool() == Some(true),
            "incomplete retained adapter cleanup",
        )?;
    }
    require(
        cleanup["activeRuns"].as_u64() == Some(0)
            && cleanup["exitCode"].as_u64() == Some(0)
            && cleanup["writesPerformed"].as_bool() == Some(false)
            && cleanup.get("cancelled").is_none_or(|value| value == false),
        "retained adapter did not complete successfully",
    )?;
    let operators = validation::operator_catalogue(&steps["handshake"])?;
    let mutants = validation::discovered(&steps["discover"], &operators, family, 10_000)?;
    require(
        candidates.len() == mutants.len()
            && cleanup["validatedCandidates"].as_u64() == Some(candidates.len() as u64),
        "incomplete retained candidate count",
    )?;
    let originals: BTreeMap<_, _> = mutants
        .iter()
        .map(|value| {
            (
                value["id"].as_str().expect("validated mutant identity"),
                value,
            )
        })
        .collect();
    let mut ids = BTreeSet::new();
    let mut valid = 0usize;
    for (index, candidate) in candidates.iter().enumerate() {
        let proof = verify_candidate(project, candidate)?;
        require(
            candidate["index"].as_u64() == Some(index as u64)
                && proof.mutant.family == family
                && originals.get(proof.mutant.id.as_str()).copied() == Some(&candidate["mutant"])
                && ids.insert(proof.mutant.id),
            "retained candidate differs from exact discovery set",
        )?;
        valid += usize::from(proof.validation["valid"] == true);
    }
    require(
        cleanup["staticallyValidCandidates"].as_u64() == Some(valid as u64),
        "retained static validation count differs",
    )?;
    Ok(RetainedTranscript { steps, candidates })
}

#[cfg(test)]
#[path = "retained_transcript_tests.rs"]
mod tests;
