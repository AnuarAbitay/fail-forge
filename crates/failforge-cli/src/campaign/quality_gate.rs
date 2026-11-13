//! Reproducible quality decisions over retained mutation evidence.
use std::collections::BTreeMap;
use std::io;

use failforge_config::quality_gate::{QualityGate, ScoreThreshold};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StopProof {
    min_score: ScoreThreshold,
    max_possible_percent: f64,
    killed: u64,
    survived: u64,
    remaining: u64,
}

pub(crate) fn configured(value: Option<&Value>) -> io::Result<QualityGate> {
    let Some(value) = value else {
        return Ok(QualityGate::default());
    };
    if !value.is_object() {
        return Err(invalid("quality gate configuration must be an object"));
    }
    let gate: QualityGate = serde_json::from_value(json!({
        "minScore":value["min_score"],
        "stopWhenUnreachable":value["stop_when_unreachable"]
    }))
    .map_err(|_| invalid("invalid quality gate configuration"))?;
    gate.validate()
        .map_err(|_| invalid("invalid quality gate configuration"))?;
    Ok(gate)
}

pub(crate) fn proof(value: &Value) -> io::Result<StopProof> {
    if !value.is_object() {
        return Err(invalid("quality stop proof must be an object"));
    }
    serde_json::from_value(value.clone()).map_err(|_| invalid("invalid quality stop proof"))
}

fn completed(status: &str) -> bool {
    matches!(status, "completed" | "no_mutants")
}

fn can_evaluate(status: &str) -> bool {
    completed(status) || status == "quality_stopped"
}

pub(crate) fn family(
    gate: &QualityGate,
    proof: Option<&StopProof>,
    family_status: &str,
    campaign_status: &str,
    expected: Option<u64>,
    counts: &BTreeMap<String, u64>,
    processed: u64,
) -> io::Result<Option<Value>> {
    if campaign_status == "quality_stopped"
        && !matches!(
            family_status,
            "completed" | "no_mutants" | "quality_stopped" | "not_started"
        )
    {
        return Err(invalid(
            "quality stop cannot mask a failed or active family",
        ));
    }
    gate.validate()
        .map_err(|_| invalid("invalid quality gate configuration"))?;
    let Some(threshold) = gate.min_score else {
        if proof.is_some() || family_status == "quality_stopped" {
            return Err(invalid("quality stop requires a configured threshold"));
        }
        return Ok(None);
    };
    let killed = counts.get("KILLED").copied().unwrap_or(0);
    let survived = counts.get("SURVIVED").copied().unwrap_or(0);
    let scored = killed
        .checked_add(survived)
        .ok_or_else(|| invalid("quality gate count overflow"))?;
    if scored > processed {
        return Err(invalid(
            "quality gate scored count exceeds processed candidates",
        ));
    }
    let not_run = match expected {
        Some(total) => Some(
            total
                .checked_sub(processed)
                .ok_or_else(|| invalid("quality gate count exceeds plan"))?,
        ),
        None => None,
    };
    if let Some(proof) = proof {
        let total = expected.ok_or_else(|| invalid("quality stop requires a known plan"))?;
        let before = total
            .checked_sub(proof.remaining)
            .ok_or_else(|| invalid("quality stop count exceeds plan"))?;
        let proof_scored = proof
            .killed
            .checked_add(proof.survived)
            .ok_or_else(|| invalid("quality stop count overflow"))?;
        let excluded_before = before
            .checked_sub(proof_scored)
            .ok_or_else(|| invalid("quality stop counts exceed processed candidates"))?;
        let maximum = 100.0 * (proof.killed as f64 + proof.remaining as f64)
            / (proof.killed as f64 + proof.survived as f64 + proof.remaining as f64);
        if !gate.stop_when_unreachable
            || proof.min_score != threshold
            || proof.killed > killed
            || proof.survived > survived
            || before > processed
            || excluded_before > processed.saturating_sub(scored)
            || !proof.max_possible_percent.is_finite()
            || !maximum.is_finite()
            || (proof.max_possible_percent - maximum).abs() > 0.000_000_001
            || !threshold.unreachable(proof.killed, proof.survived, proof.remaining)
        {
            return Err(invalid(
                "quality stop proof disagrees with retained evidence",
            ));
        }
    }
    if family_status == "quality_stopped" && (proof.is_none() || not_run == Some(0)) {
        return Err(invalid(
            "quality-stopped family lacks skipped candidates or proof",
        ));
    }
    let actual = (completed(family_status) && can_evaluate(campaign_status) && scored > 0)
        .then(|| 100.0 * killed as f64 / scored as f64);
    let (status, reason) = if !can_evaluate(campaign_status) {
        ("not_evaluated", Some("campaign_incomplete"))
    } else if proof.is_some() {
        ("failed", Some("threshold_unreachable"))
    } else if !completed(family_status) {
        (
            "not_evaluated",
            Some(
                if campaign_status == "quality_stopped" && family_status == "not_started" {
                    "skipped_due_to_gate"
                } else {
                    "campaign_incomplete"
                },
            ),
        )
    } else if scored == 0 {
        ("not_evaluated", Some("no_scored_mutants"))
    } else if threshold.passes(killed, survived) {
        ("passed", None)
    } else {
        ("failed", Some("below_threshold"))
    };
    Ok(Some(json!({"status":status,"minScore":threshold,
        "stopWhenUnreachable":gate.stop_when_unreachable,
        "actualPercent":actual,"reason":reason,"campaignStatus":campaign_status,
        "maxPossiblePercent":proof.map(|proof| proof.max_possible_percent),
        "processedMutants":processed,"notRunMutants":not_run,"proof":proof})))
}

pub(crate) fn profile(families: BTreeMap<String, Value>, campaign_status: &str) -> Option<Value> {
    if families.is_empty() {
        return None;
    }
    let insufficient = families
        .values()
        .any(|gate| gate["status"] == "not_evaluated" && gate["reason"] != "skipped_due_to_gate");
    let status = if !can_evaluate(campaign_status) || insufficient {
        "not_evaluated"
    } else if families.values().any(|gate| gate["status"] == "failed") {
        "failed"
    } else if families
        .values()
        .any(|gate| gate["status"] == "not_evaluated")
    {
        "not_evaluated"
    } else {
        "passed"
    };
    Some(json!({"status":status,"families":families}))
}

/// Validate a stored gate by rebuilding its decision from the same retained facts.
pub(crate) fn validate_stored(
    value: Option<&Value>,
    family_status: &str,
    campaign_status: &str,
    expected: Option<u64>,
    counts: &BTreeMap<String, u64>,
    processed: u64,
) -> io::Result<Value> {
    let Some(value) = value else {
        if family_status == "quality_stopped" {
            return Err(invalid("quality-stopped report lacks a gate"));
        }
        return Ok(Value::Null);
    };
    if !value.is_object() || value["campaignStatus"] != campaign_status {
        return Err(invalid(
            "quality gate campaign status disagrees with report",
        ));
    }
    let gate: QualityGate = serde_json::from_value(json!({
        "minScore":value["minScore"],"stopWhenUnreachable":value["stopWhenUnreachable"]
    }))
    .map_err(|_| invalid("invalid stored quality threshold"))?;
    let proof = value
        .get("proof")
        .filter(|value| !value.is_null())
        .map(proof)
        .transpose()?;
    let rebuilt = family(
        &gate,
        proof.as_ref(),
        family_status,
        campaign_status,
        expected,
        counts,
        processed,
    )?
    .ok_or_else(|| invalid("stored quality gate has no threshold"))?;
    if &rebuilt != value {
        return Err(invalid("quality gate disagrees with retained results"));
    }
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(threshold: &str, stop: bool) -> QualityGate {
        QualityGate {
            min_score: Some(ScoreThreshold::parse(threshold).unwrap()),
            stop_when_unreachable: stop,
        }
    }

    fn counts(killed: u64, survived: u64) -> BTreeMap<String, u64> {
        [("KILLED".into(), killed), ("SURVIVED".into(), survived)].into()
    }

    #[test]
    fn exact_thresholds_and_missing_evidence_have_distinct_verdicts() {
        let evaluate = |threshold: &str, k, s| {
            family(
                &gate(threshold, false),
                None,
                "completed",
                "completed",
                Some(k + s),
                &counts(k, s),
                k + s,
            )
            .unwrap()
            .unwrap()
        };
        assert_eq!(evaluate("80", 4, 1)["status"], "passed");
        assert_eq!(evaluate("80.01", 4, 1)["status"], "failed");
        assert_eq!(evaluate("0", 0, 0)["reason"], "no_scored_mutants");
        assert!(evaluate("0", 0, 0)["actualPercent"].is_null());
    }

    #[test]
    fn early_proof_survives_draining_without_publishing_a_partial_score() {
        let proof = proof(&json!({"minScore":80,"maxPossiblePercent":75.0,
            "killed":0,"survived":1,"remaining":3}))
        .unwrap();
        let value = family(
            &gate("80", true),
            Some(&proof),
            "quality_stopped",
            "quality_stopped",
            Some(4),
            &counts(1, 1),
            2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(value["status"], "failed");
        assert_eq!(value["reason"], "threshold_unreachable");
        assert_eq!(value["notRunMutants"], 2);
        assert!(value["actualPercent"].is_null());
        let failed = family(
            &gate("80", true),
            Some(&proof),
            "failed",
            "failed",
            Some(4),
            &counts(1, 1),
            2,
        )
        .unwrap()
        .unwrap();
        assert_eq!(failed["status"], "not_evaluated");
        assert_eq!(failed["reason"], "campaign_incomplete");
        for status in ["failed", "cancelled", "running"] {
            assert!(
                family(
                    &gate("80", true),
                    Some(&proof),
                    status,
                    "quality_stopped",
                    Some(4),
                    &counts(1, 1),
                    2
                )
                .is_err()
            );
        }
    }

    #[test]
    fn forged_or_nonterminal_stop_proof_is_rejected() {
        for value in [
            json!({"minScore":80,"maxPossiblePercent":74.0,"killed":0,"survived":1,"remaining":3}),
            json!({"minScore":80,"maxPossiblePercent":100.0,"killed":1,"survived":0,"remaining":3}),
            json!({"minScore":80,"maxPossiblePercent":75.0,"killed":0,"survived":2,"remaining":6}),
            json!({"minScore":90,"maxPossiblePercent":75.0,"killed":0,"survived":1,"remaining":3}),
        ] {
            let proof = proof(&value).unwrap();
            assert!(
                family(
                    &gate("80", true),
                    Some(&proof),
                    "quality_stopped",
                    "quality_stopped",
                    Some(4),
                    &counts(0, 1),
                    1
                )
                .is_err(),
                "{value}"
            );
        }
        let valid = family(
            &gate("80", false),
            None,
            "completed",
            "completed",
            Some(5),
            &counts(4, 1),
            5,
        )
        .unwrap()
        .unwrap();
        let mut forged = valid.clone();
        forged["status"] = json!("failed");
        assert!(
            validate_stored(
                Some(&forged),
                "completed",
                "completed",
                Some(5),
                &counts(4, 1),
                5
            )
            .is_err()
        );
        assert_eq!(
            validate_stored(
                Some(&valid),
                "completed",
                "completed",
                Some(5),
                &counts(4, 1),
                5
            )
            .unwrap(),
            valid
        );
    }

    #[test]
    fn insufficient_evidence_overrides_failures_but_skipped_families_do_not() {
        let failed = family(
            &gate("80", false),
            None,
            "completed",
            "completed",
            Some(1),
            &counts(0, 1),
            1,
        )
        .unwrap()
        .unwrap();
        let no_score = family(
            &gate("80", false),
            None,
            "no_mutants",
            "completed",
            Some(0),
            &counts(0, 0),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            profile(
                [
                    ("classic".into(), failed.clone()),
                    ("failure_mode".into(), no_score)
                ]
                .into(),
                "completed"
            )
            .unwrap()["status"],
            "not_evaluated"
        );
        let skipped = family(
            &gate("80", true),
            None,
            "not_started",
            "quality_stopped",
            Some(3),
            &counts(0, 0),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            profile(
                [("classic".into(), failed), ("failure_mode".into(), skipped)].into(),
                "quality_stopped"
            )
            .unwrap()["status"],
            "failed"
        );
    }
}
