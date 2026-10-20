//! Reverify a retained discovery candidate without writing any source files.
use std::path::Path;

use serde_json::Value;

use super::{DiscoveryError, LimitedBuffer, validate_value, validation};
use crate::{MAX_LINE_BYTES, Mutant, validate_relative_target};

const MAX_SAFE_INTEGER: u64 = (1_u64 << 53) - 1;

/// Source-bound metadata accepted for one fresh attempt. The original JSON is
/// retained because optional nulls and adapter diagnostics belong to the report.
#[derive(Debug, Clone)]
pub struct VerifiedCandidate {
    pub mutant: Mutant,
    pub raw_mutant: Value,
    pub validation: Value,
    pub patch_sha256: Option<String>,
}

fn invalid(message: &'static str) -> DiscoveryError {
    DiscoveryError::Invalid(message)
}

/// Check a retained candidate against the current source and independently
/// reconstruct its complete safe-patch proof. This verifies byte applicability;
/// the caller still owns the original static-validation session and its binding.
/// A statically invalid candidate binds its source without requiring its edit
/// to be applicable. Neither path modifies the package.
pub fn verify_candidate(package: &Path, raw: &Value) -> Result<VerifiedCandidate, DiscoveryError> {
    if !package.is_absolute() {
        return Err(invalid("candidate package root must be absolute"));
    }
    let mut budget = MAX_LINE_BYTES;
    validate_value(raw, 0, &mut budget)?;
    let mut encoded = LimitedBuffer::new(MAX_LINE_BYTES);
    serde_json::to_writer(&mut encoded, raw)
        .map_err(|_| invalid("candidate evidence exceeds its byte limit"))?;
    drop(encoded);
    let object = raw
        .as_object()
        .filter(|object| {
            object.len() == 4
                && ["index", "mutant", "validation", "safePatch"]
                    .into_iter()
                    .all(|key| object.contains_key(key))
        })
        .ok_or_else(|| invalid("invalid retained candidate fields"))?;
    let index = object["index"]
        .as_u64()
        .filter(|index| *index <= MAX_SAFE_INTEGER)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| invalid("invalid retained candidate index"))?;
    let raw_mutant = &object["mutant"];
    // Derived struct deserializers also accept positional JSON arrays. The
    // adapter protocol requires named objects, including an empty Symbol.
    if !raw_mutant.is_object()
        || !raw_mutant["edit"].is_object()
        || !raw_mutant["symbol"].is_object()
        || !object["validation"].is_object()
        || !object["safePatch"].is_object()
    {
        return Err(invalid("retained candidate metadata must use JSON objects"));
    }
    let mutant: Mutant = serde_json::from_value(raw_mutant.clone())
        .map_err(|_| invalid("invalid retained mutant metadata"))?;
    mutant
        .validate_model()
        .map_err(|_| invalid("invalid retained mutant identity"))?;
    if !mutant.file.ends_with(".dart") || validate_relative_target(&mutant.file).is_err() {
        return Err(invalid(
            "mutation target must be a canonical Dart source path",
        ));
    }
    let regenerated =
        validation::candidate(index, raw_mutant, object["validation"].clone(), package)?;
    if regenerated.safe_patch != object["safePatch"] {
        return Err(invalid(
            "retained safe-patch proof differs from current source",
        ));
    }
    let patch_sha256 = regenerated.safe_patch["patchedHash"]
        .as_str()
        .map(str::to_owned);
    Ok(VerifiedCandidate {
        mutant,
        raw_mutant: raw_mutant.clone(),
        validation: regenerated.validation,
        patch_sha256,
    })
}

#[cfg(test)]
#[path = "candidate_proof_tests.rs"]
mod tests;
