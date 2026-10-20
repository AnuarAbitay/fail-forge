use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::{CandidateEvidence, DiscoveryError, MAX_SOURCE_BYTES, io_error};
use crate::{Mutant, MutationFamily, apply_mutant, sha256_hex, validate_relative_target};

fn invalid(message: &'static str) -> DiscoveryError {
    DiscoveryError::Invalid(message)
}

pub(super) fn package_root(payload: &Value) -> Result<PathBuf, DiscoveryError> {
    let workspace = payload
        .get("workspaceRoot")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("payload workspaceRoot must be an absolute string"))?;
    if !Path::new(workspace).is_absolute() {
        return Err(invalid("payload workspaceRoot must be absolute"));
    }
    let workspace = Path::new(workspace).canonicalize().map_err(io_error)?;
    let relative = payload
        .get("packageRoot")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("payload packageRoot must be a canonical relative string"))?;
    if relative != "." {
        validate_relative_target(relative).map_err(|_| invalid("invalid packageRoot"))?;
    }
    let package = workspace.join(relative).canonicalize().map_err(io_error)?;
    if !package.is_dir() || !package.starts_with(&workspace) {
        return Err(invalid(
            "payload packageRoot must remain inside workspaceRoot",
        ));
    }
    Ok(package)
}

pub(super) fn candidate_limit(payload: &Value) -> Result<u64, DiscoveryError> {
    let Some(limits) = payload.get("limits") else {
        return Ok(1000);
    };
    let limits = limits
        .as_object()
        .ok_or_else(|| invalid("payload limits must be an object"))?;
    if limits.keys().any(|key| key != "maxMutants") {
        return Err(invalid("unknown campaign limits field"));
    }
    let Some(limit) = limits.get("maxMutants") else {
        return Ok(1000);
    };
    limit
        .as_u64()
        .filter(|count| *count <= 10_000)
        .ok_or_else(|| invalid("maxMutants must be an integer from zero through 10000"))
}

pub(super) fn operator_catalogue(
    handshake: &Value,
) -> Result<BTreeMap<String, (MutationFamily, u64)>, DiscoveryError> {
    if handshake.get("protocolVersion").and_then(Value::as_u64) != Some(1) {
        return Err(invalid("handshake protocol version is incompatible"));
    }
    let operators = handshake
        .get("operators")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("handshake has no operator catalogue"))?;
    let mut result = BTreeMap::new();
    for operator in operators {
        let id = operator
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 128 && !id.contains('\0'))
            .ok_or_else(|| invalid("operator catalogue contains an invalid identifier"))?;
        let family: MutationFamily =
            serde_json::from_value(operator.get("family").cloned().unwrap_or(Value::Null))
                .map_err(|_| invalid("operator catalogue contains an invalid family"))?;
        let version = operator
            .get("version")
            .and_then(Value::as_u64)
            .filter(|version| *version == 1)
            .ok_or_else(|| invalid("operator catalogue version is incompatible"))?;
        if result.insert(id.into(), (family, version)).is_some() {
            return Err(invalid("operator catalogue contains duplicate identifiers"));
        }
    }
    Ok(result)
}

pub(super) fn discovered<'a>(
    discovery: &'a Value,
    operators: &BTreeMap<String, (MutationFamily, u64)>,
    family: MutationFamily,
    limit: u64,
) -> Result<&'a [Value], DiscoveryError> {
    match discovery.get("truncated").and_then(Value::as_bool) {
        Some(false) => {}
        Some(true) => {
            return Err(invalid(
                "discovery was truncated; narrow the scope or raise maxMutants within its bound",
            ));
        }
        None => return Err(invalid("discovery has no truncation flag")),
    }
    let mutants = discovery
        .get("mutants")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("discovery has no mutants array"))?;
    let count = u64::try_from(mutants.len()).map_err(|_| invalid("candidate count overflow"))?;
    if count > limit
        || discovery.get("returnedCandidates").and_then(Value::as_u64) != Some(count)
        || discovery.get("totalCandidates").and_then(Value::as_u64) != Some(count)
    {
        return Err(invalid(
            "discovery candidate counters do not match the requested limit and candidates",
        ));
    }
    let mut ids = BTreeSet::new();
    for original in mutants {
        let mutant: Mutant = serde_json::from_value(original.clone())
            .map_err(|_| invalid("invalid discovered mutant metadata"))?;
        mutant
            .validate_model()
            .map_err(|_| invalid("invalid discovered mutant identity"))?;
        if mutant.family != family
            || operators.get(&mutant.operator_id)
                != Some(&(family, u64::from(mutant.operator_version)))
        {
            return Err(invalid(
                "candidate is not from the selected family and operator catalogue",
            ));
        }
        if !mutant.file.ends_with(".dart") || validate_relative_target(&mutant.file).is_err() {
            return Err(invalid(
                "mutation target must be a canonical Dart source path",
            ));
        }
        if !ids.insert(mutant.id) {
            return Err(invalid("duplicate discovered mutant identity"));
        }
    }
    Ok(mutants)
}

pub(super) fn candidate(
    index: usize,
    original: &Value,
    validation: Value,
    package: &Path,
) -> Result<CandidateEvidence, DiscoveryError> {
    let mutant: Mutant = serde_json::from_value(original.clone())
        .map_err(|_| invalid("invalid candidate metadata"))?;
    if validation.get("mutantId").and_then(Value::as_str) != Some(mutant.id.as_str()) {
        return Err(invalid("validation identifies a different mutant"));
    }
    let valid = match (
        validation.get("valid").and_then(Value::as_bool),
        validation.get("validation").and_then(Value::as_str),
    ) {
        (Some(true), Some("valid")) => true,
        (Some(false), Some("invalid")) => false,
        _ => return Err(invalid("adapter returned inconsistent validation metadata")),
    };
    let source = crate::patch::read_mutant_source(package, &mutant, MAX_SOURCE_BYTES)
        .map_err(|_| DiscoveryError::Patch)?;
    let safe_patch = if valid {
        let bytes = apply_mutant(&source, &mutant).map_err(|_| DiscoveryError::Patch)?;
        let hash = sha256_hex(&bytes);
        let length = bytes.len();
        let text = String::from_utf8(bytes).map_err(|_| DiscoveryError::Patch)?;
        json!({"applied":true,"mutantId":mutant.id,"file":mutant.file,
            "patchedHash":hash,"patchedBytes":length,"patchedUtf8":text,"writesPerformed":false})
    } else {
        json!({"applied":false,"mutantId":mutant.id,"file":mutant.file,"reason":"static_invalid","writesPerformed":false})
    };
    Ok(CandidateEvidence {
        index,
        mutant: original.clone(),
        validation,
        safe_patch,
    })
}
