//! Pure projections for retained classic replay reports, including partial runs.
use std::collections::{BTreeMap, BTreeSet};
use std::io;

use serde_json::{Value, json};

use crate::campaign_publish_fs::invalid;

fn text(value: &Value) -> io::Result<&str> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid("invalid retained replay report metadata"))
}

pub(super) fn discovered_metadata(discovery: &Value) -> Option<Value> {
    let handshake = discovery.pointer("/steps/handshake")?;
    if !handshake.is_object() {
        return None;
    }
    let candidates = discovery["candidates"].as_array()?;
    Some(json!({
        "operators":handshake["operators"],"operatorTree":handshake["operatorTree"],
        "candidates":candidates.len(),
        "retainedCandidates":candidates.iter().map(|candidate|json!({
            "mutantId":candidate["mutant"]["id"],"operatorId":candidate["mutant"]["operatorId"]
        })).collect::<Vec<_>>()
    }))
}

pub(super) fn annotate_discovered(summary: &mut Value, metadata: &Value) -> io::Result<()> {
    annotate(summary, metadata)?;
    let fields = summary.as_object_mut().unwrap();
    fields.remove("staticValidationReused");
    let missing = fields.remove("withoutRetainedCandidates").unwrap();
    fields.insert("notApplicableOperators".into(), missing);
    Ok(())
}

pub(super) fn annotate(summary: &mut Value, binding: &Value) -> io::Result<()> {
    let catalogue = binding["operators"]
        .as_array()
        .ok_or_else(|| invalid("retained replay lacks operator metadata"))?;
    let mut categories = BTreeMap::new();
    for operator in catalogue
        .iter()
        .filter(|operator| operator["family"] == "classic")
    {
        let id = text(&operator["id"])?;
        let category = match operator
            .get("category")
            .filter(|category| !category.is_null())
        {
            Some(category) => text(category)?,
            None => "unclassified",
        };
        if categories.insert(id, category).is_some() {
            return Err(invalid("duplicate retained replay operator metadata"));
        }
    }
    let retained = binding["retainedCandidates"]
        .as_array()
        .ok_or_else(|| invalid("retained replay lacks exact candidate identities"))?;
    if binding["candidates"].as_u64() != Some(retained.len() as u64) {
        return Err(invalid("retained replay candidate total changed"));
    }
    let mut candidates = BTreeMap::new();
    let mut retained_operators = BTreeSet::new();
    for row in retained {
        let id = text(&row["mutantId"])?;
        let operator = text(&row["operatorId"])?;
        if !categories.contains_key(operator) || candidates.insert(id, operator).is_some() {
            return Err(invalid(
                "unknown operator or duplicate retained replay candidate",
            ));
        }
        retained_operators.insert(operator);
    }
    let mut advertised = BTreeSet::new();
    match binding.pointer("/operatorTree/classic") {
        Some(Value::Object(groups)) => {
            for operators in groups.values() {
                for operator in operators
                    .as_array()
                    .ok_or_else(|| invalid("invalid retained classic operator tree"))?
                {
                    let id = text(operator)?;
                    if !categories.contains_key(id) || !advertised.insert(id) {
                        return Err(invalid("unknown or duplicate retained tree operator"));
                    }
                }
            }
        }
        None | Some(Value::Null) => advertised.extend(categories.keys().copied()),
        _ => return Err(invalid("invalid retained classic operator tree")),
    }
    // Candidates can use a declared operator omitted from an optional tree.
    // The catalogue remains authoritative for execution and result attribution.
    advertised.extend(categories.keys().copied());
    let missing: Vec<_> = advertised
        .difference(&retained_operators)
        .copied()
        .collect();
    let mut groups: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut operators: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut reasons: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut timeout_kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut processed = BTreeSet::new();
    for record in summary["records"]
        .as_array_mut()
        .ok_or_else(|| invalid("replay summary lacks result records"))?
    {
        let id = text(&record["mutantId"])?;
        let operator = text(&record["operatorId"])?;
        if candidates.get(id).copied() != Some(operator) || !processed.insert(id.to_owned()) {
            return Err(invalid(
                "replay result differs from retained candidate identity or operator",
            ));
        }
        let category = categories
            .get(operator)
            .copied()
            .ok_or_else(|| invalid("unknown replay result operator"))?;
        let outcome = text(&record["outcome"])?;
        let reason = text(&record["reason"])?;
        *groups
            .entry(category.into())
            .or_default()
            .entry(outcome.into())
            .or_default() += 1;
        *operators
            .entry(operator.into())
            .or_default()
            .entry(outcome.into())
            .or_default() += 1;
        *reasons
            .entry(outcome.into())
            .or_default()
            .entry(reason.into())
            .or_default() += 1;
        if matches!(outcome, "KILLED" | "TIMEOUT") {
            if let Some(diagnostic) = record
                .get("timeoutDiagnostics")
                .filter(|value| !value.is_null())
            {
                *timeout_kinds
                    .entry(text(&diagnostic["kind"])?.into())
                    .or_default() += 1;
            }
        }
        record["category"] = category.into();
    }
    summary["resultsByClassicalCategory"] = json!(groups);
    summary["resultsByOperator"] = json!(operators);
    summary["withoutRetainedCandidates"] = json!(missing);
    summary["outcomeReasonCounts"] = json!(reasons);
    summary["timeoutKindCounts"] = json!(timeout_kinds);
    summary["allDiscoveredMutantsProcessed"] = (processed.len() == candidates.len()).into();
    summary["staticValidationReused"] = true.into();
    Ok(())
}

#[cfg(test)]
#[path = "campaign_replay_report_tests.rs"]
mod tests;
