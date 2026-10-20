//! Pure aggregation of already validated campaign facts.
//!
//! Execution owners establish evidence. This module validates the reporting
//! contract, counts records and builds summaries without filesystem access.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use failforge_dart::namespace::NamespaceError;
use failforge_workspace::parse_json;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::dart_namespace::{absolute, invalid};

const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const FAMILIES: [Family; 3] = [
    Family::Classic,
    Family::FailureMode,
    Family::LanguageSpecific,
];
const OUTCOMES: [&str; 8] = [
    "KILLED",
    "SURVIVED",
    "INVALID",
    "TIMEOUT",
    "INFRA_ERROR",
    "UNCLASSIFIED",
    "NO_TESTS",
    "CANCELLED",
];

/// Nullable fields are required on the wire: omission is not an implicit null.
fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Direct,
    Profile,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum Family {
    Classic,
    FailureMode,
    LanguageSpecific,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::FailureMode => "failure_mode",
            Self::LanguageSpecific => "language_specific",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Completed,
    NoMutants,
    Failed,
    Cancelled,
}

impl Status {
    fn complete(self) -> bool {
        matches!(self, Self::Completed | Self::NoMutants)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FamilyStatus {
    NotStarted,
    Running,
    Completed,
    NoMutants,
    Failed,
    Cancelled,
}

impl FamilyStatus {
    fn complete(self) -> bool {
        matches!(self, Self::Completed | Self::NoMutants)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Skipped {
    name: String,
    reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    recovered: Vec<String>,
    skipped: Vec<Skipped>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cleanup {
    original_project_files_unchanged: bool,
    temporary_snapshot_cleaned: bool,
    workspace_cleanup_deferred: bool,
    #[serde(deserialize_with = "nullable")]
    workspace_directory: Option<PathBuf>,
    workspace_recovery: Recovery,
}

impl Cleanup {
    fn clean(&self) -> bool {
        self.original_project_files_unchanged
            && self.temporary_snapshot_cleaned
            && !self.workspace_cleanup_deferred
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FamilyInput {
    family: Family,
    #[serde(deserialize_with = "nullable")]
    language: Option<String>,
    output: PathBuf,
    status: FamilyStatus,
    #[serde(deserialize_with = "nullable")]
    error: Option<String>,
    #[serde(deserialize_with = "nullable")]
    expected_mutants: Option<u64>,
    records: Vec<Value>,
    #[serde(deserialize_with = "nullable")]
    baseline: Option<Value>,
    original_controls: Vec<Value>,
    timings: BTreeMap<String, f64>,
    wall_seconds: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    operation: String,
    mode: Mode,
    run_id: String,
    output: PathBuf,
    status: Status,
    #[serde(deserialize_with = "nullable")]
    error: Option<String>,
    #[serde(deserialize_with = "nullable")]
    expected_mutants: Option<u64>,
    timings: BTreeMap<String, f64>,
    wall_seconds: f64,
    cleanup: Cleanup,
    families: Vec<FamilyInput>,
}

/// Build a final report from the bounded private JSON request.
pub(crate) fn build(input: &[u8]) -> Result<Value, NamespaceError> {
    if input.is_empty() || input.len() > MAX_INPUT_BYTES {
        return Err(invalid("invalid campaign report request size"));
    }
    let value = parse_json(input)?;
    // Serde permits positional sequences for structs. This protocol requires
    // named objects, including recovery entries, before typed decoding.
    for pointer in ["", "/cleanup", "/cleanup/workspaceRecovery"] {
        object(value.pointer(pointer))?;
    }
    for family in value["families"]
        .as_array()
        .ok_or_else(|| invalid("campaign report families must be an array"))?
    {
        object(Some(family))?;
    }
    for skipped in value["cleanup"]["workspaceRecovery"]["skipped"]
        .as_array()
        .ok_or_else(|| invalid("campaign report recovery skips must be an array"))?
    {
        object(Some(skipped))?;
    }
    let request =
        serde_json::from_value(value).map_err(|_| invalid("invalid campaign report request"))?;
    summarize(request)
}

fn object(value: Option<&Value>) -> Result<(), NamespaceError> {
    if value.is_some_and(Value::is_object) {
        Ok(())
    } else {
        Err(invalid("campaign report requires named objects"))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Counts {
    outcomes: BTreeMap<String, u64>,
    reasons: BTreeMap<String, u64>,
}

impl Counts {
    fn records(family: Family, records: &[Value]) -> Result<Self, NamespaceError> {
        let mut result = Self::default();
        let mut ids = BTreeSet::new();
        for record in records {
            let id = record["mutantId"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| invalid("invalid campaign mutation identity"))?;
            let outcome = record["outcome"]
                .as_str()
                .filter(|outcome| OUTCOMES.contains(outcome))
                .ok_or_else(|| invalid("invalid campaign mutation outcome"))?;
            let reason = record["reason"]
                .as_str()
                .ok_or_else(|| invalid("invalid campaign mutation reason"))?;
            if !record.is_object()
                || record["recordKind"] != "mutation"
                || record["family"] != family.name()
                || !ids.insert(id)
            {
                return Err(invalid("mixed-family or duplicate campaign mutation"));
            }
            *result.outcomes.entry(outcome.to_owned()).or_default() += 1;
            *result.reasons.entry(reason.to_owned()).or_default() += 1;
        }
        Ok(result)
    }

    fn merge(&mut self, other: &Self) {
        for (key, count) in &other.outcomes {
            *self.outcomes.entry(key.clone()).or_default() += count;
        }
        for (key, count) in &other.reasons {
            *self.reasons.entry(key.clone()).or_default() += count;
        }
    }

    fn score(&self, family: Family) -> Value {
        let killed = self.outcomes.get("KILLED").copied().unwrap_or(0);
        let survived = self.outcomes.get("SURVIVED").copied().unwrap_or(0);
        let denominator = killed + survived;
        let excluded: BTreeMap<_, _> = self
            .outcomes
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "KILLED" | "SURVIVED"))
            .collect();
        json!({
            "family": family, "killed": killed, "survived": survived,
            "denominator": denominator,
            "percent": (denominator != 0).then(|| 100.0 * killed as f64 / denominator as f64),
            "excluded": excluded
        })
    }
}

fn timing(timings: &BTreeMap<String, f64>, wall_seconds: f64) -> Result<(), NamespaceError> {
    if !wall_seconds.is_finite()
        || wall_seconds < 0.0
        || timings
            .values()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(invalid(
            "campaign report timings must be finite and nonnegative",
        ));
    }
    Ok(())
}

fn validate(request: &Request) -> Result<Vec<Counts>, NamespaceError> {
    if request.operation != "campaign-report"
        || request.run_id.len() != 32
        || !request
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || request.families.is_empty()
        || request.families.len() > 3
        || request.mode == Mode::Direct && request.families.len() != 1
    {
        return Err(invalid(
            "invalid campaign report identity or family selection",
        ));
    }
    absolute(&request.output)?;
    if let Some(directory) = &request.cleanup.workspace_directory {
        absolute(directory)?;
    }
    timing(&request.timings, request.wall_seconds)?;
    let mut families = BTreeSet::new();
    let mut expected = Some(0u64);
    let mut counts = Vec::with_capacity(request.families.len());
    for family in &request.families {
        if !families.insert(family.family)
            || match family.family {
                Family::LanguageSpecific => family.language.as_deref() != Some("dart"),
                _ => family.language.is_some(),
            }
        {
            return Err(invalid("invalid campaign report family or language"));
        }
        absolute(&family.output)?;
        timing(&family.timings, family.wall_seconds)?;
        if family
            .baseline
            .as_ref()
            .is_some_and(|value| !value.is_object())
            || family
                .original_controls
                .iter()
                .any(|value| !value.is_object())
        {
            return Err(invalid("campaign report attempts must be objects"));
        }
        let completed = family.records.len() as u64;
        if family
            .expected_mutants
            .is_some_and(|total| completed > total)
            || family.status.complete()
                && (family.expected_mutants != Some(completed) || family.error.is_some())
            || family.status == FamilyStatus::NoMutants && completed != 0
        {
            return Err(invalid("inconsistent campaign report family totals"));
        }
        expected = match (expected, family.expected_mutants) {
            (Some(total), Some(additional)) => Some(
                total
                    .checked_add(additional)
                    .ok_or_else(|| invalid("campaign report total overflow"))?,
            ),
            _ => None,
        };
        counts.push(Counts::records(family.family, &family.records)?);
    }
    if request.expected_mutants.is_some() && request.expected_mutants != expected {
        return Err(invalid("inconsistent campaign report aggregate total"));
    }
    if request.status.complete()
        && (!request.cleanup.clean()
            || request.error.is_some()
            || request.expected_mutants.is_none()
            || request
                .families
                .iter()
                .any(|family| !family.status.complete())
            || (request.status == Status::NoMutants) != (request.expected_mutants == Some(0)))
    {
        return Err(invalid(
            "campaign report success lacks complete clean evidence",
        ));
    }
    Ok(counts)
}

fn family_summary(
    family: &FamilyInput,
    counts: &Counts,
    request: &Request,
    cleanup: &Value,
) -> Value {
    let mut records = family.records.clone();
    // Identity type and uniqueness were validated before sorting.
    records.sort_by(|left, right| left["mutantId"].as_str().cmp(&right["mutantId"].as_str()));
    let scored = request.status.complete() && family.status.complete();
    let score = if scored {
        counts.score(family.family)
    } else {
        Value::Null
    };
    let mut value = json!({
        "version": 1, "runId": request.run_id, "output": family.output,
        "family": family.family, "status": family.status,
        "complete": family.status.complete(), "profileComplete": request.status.complete(),
        "error": family.error, "expectedMutants": family.expected_mutants,
        "completedMutants": records.len(), "records": records,
        "baseline": family.baseline, "originalControls": family.original_controls,
        "timings": family.timings, "wallSeconds": family.wall_seconds,
        "outcomeCounts": counts.outcomes, "reasonCounts": counts.reasons,
        "mutationScore": if family.family == Family::Classic { &score } else { &Value::Null },
        "failureModeScore": if family.family == Family::FailureMode { &score } else { &Value::Null }
    });
    value
        .as_object_mut()
        .expect("summary object")
        .extend(cleanup.as_object().expect("cleanup object").clone());
    if family.family == Family::LanguageSpecific {
        value["language"] = "dart".into();
        value["languageSpecificScore"] = score;
    }
    value
}

/// Aggregate typed facts. No execution policy, file publication or I/O occurs.
pub(crate) fn summarize(request: Request) -> Result<Value, NamespaceError> {
    let counts = validate(&request)?;
    let cleanup = serde_json::to_value(&request.cleanup)
        .map_err(|_| invalid("invalid campaign report cleanup metadata"))?;
    let selected: BTreeMap<_, _> = request
        .families
        .iter()
        .zip(&counts)
        .map(|(family, counts)| (family.family, (family, counts)))
        .collect();
    if request.mode == Mode::Direct {
        let (family, counts) = selected.values().next().expect("one validated family");
        let mut value = family_summary(family, counts, &request, &cleanup);
        value.as_object_mut().unwrap().remove("profileComplete");
        value["status"] = json!(request.status);
        value["complete"] = request.status.complete().into();
        value["error"] = json!(request.error);
        value["expectedMutants"] = json!(request.expected_mutants);
        value["timings"] = json!(request.timings);
        value["wallSeconds"] = request.wall_seconds.into();
        return Ok(value);
    }
    let mut families = serde_json::Map::new();
    let mut records = Vec::new();
    let mut total = Counts::default();
    for name in FAMILIES {
        let value = if let Some((family, counts)) = selected.get(&name) {
            let summary = family_summary(family, counts, &request, &cleanup);
            records.extend(summary["records"].as_array().unwrap().iter().cloned());
            total.merge(counts);
            let mut value = json!({
                "enabled": true, "status": family.status, "complete": summary["complete"],
                "expectedMutants": family.expected_mutants, "summary": summary
            });
            if name == Family::LanguageSpecific {
                value["language"] = "dart".into();
            }
            value
        } else {
            json!({"enabled":false,"status":"disabled","complete":true,"summary":null})
        };
        families.insert(name.name().to_owned(), value);
    }
    let mut value = json!({
        "kind": "profile", "version": 2, "runId": request.run_id, "output": request.output,
        "complete": request.status.complete(), "status": request.status, "error": request.error,
        "expectedMutants": request.expected_mutants, "timings": request.timings,
        "wallSeconds": request.wall_seconds, "families": families,
        "completedMutants": records.len(), "records": records,
        "outcomeCounts": total.outcomes, "reasonCounts": total.reasons,
        "mutationScore": null, "failureModeScore": null, "languageSpecificScore": null
    });
    value
        .as_object_mut()
        .unwrap()
        .extend(cleanup.as_object().unwrap().clone());
    Ok(value)
}

#[cfg(test)]
#[path = "campaign_report_tests.rs"]
mod tests;
