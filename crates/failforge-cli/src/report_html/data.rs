//! Read-only projection of retained reports for the standalone HTML viewer.
//!
//! The published summary is authoritative. Captures are bounded display-only
//! attachments; they never establish completion, recover records or alter a verdict.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use failforge_workspace::{parse_json, read_regular, read_regular_prefix};
use serde_json::{Value, json};

const MAX_SUMMARY_BYTES: usize = 64 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_RECORDS: usize = 10_000;
const MAX_LOG_BYTES: usize = 64 * 1024;
const MAX_TOTAL_LOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONTEXT_BYTES: usize = 8 * 1024 * 1024;
const FAMILIES: [&str; 3] = ["classic", "failure_mode", "language_specific"];
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

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn read_json(root: &Path, path: &Path, limit: usize) -> io::Result<Value> {
    let path = contained(root, path)?;
    let bytes = read_regular(&path, limit).map_err(|error| invalid(error.to_string()))?;
    parse_json(&bytes).map_err(|_| invalid("invalid or duplicate report JSON"))
}

/// Canonicalize both the input root and every attachment. In particular, copied
/// reports cannot cause this reader to follow their stored absolute capture paths.
/// Internal profile publication aliases remain supported.
fn contained(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let resolved = path.canonicalize()?;
    if !resolved.starts_with(root) {
        return Err(invalid(
            "report attachment resolves outside its run directory",
        ));
    }
    Ok(resolved)
}

fn text<'a>(value: &'a Value, key: &str) -> io::Result<&'a str> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid(format!("report requires a nonempty {key}")))
}

fn boolean(value: &Value, key: &str) -> io::Result<bool> {
    value[key]
        .as_bool()
        .ok_or_else(|| invalid(format!("report requires boolean {key}")))
}

fn optional_bool(value: &Value, key: &str) -> io::Result<Option<bool>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| invalid(format!("invalid report {key}"))),
    }
}

fn optional_count(value: &Value, key: &str) -> io::Result<Option<u64>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(format!("invalid report {key}"))),
    }
}

fn complete(value: &Value, family: bool) -> io::Result<bool> {
    let status = text(value, "status")?;
    if !matches!(
        status,
        "completed" | "no_mutants" | "failed" | "cancelled" | "running" | "quality_stopped"
    ) && !(family && status == "not_started")
    {
        return Err(invalid("unknown report status"));
    }
    let complete = boolean(value, "complete")?;
    if complete != matches!(status, "completed" | "no_mutants") {
        return Err(invalid("report status and completion flag disagree"));
    }
    Ok(complete)
}

fn count_map(value: &Value) -> io::Result<BTreeMap<String, u64>> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("report outcomeCounts must be an object"))?;
    let mut result = BTreeMap::new();
    for (outcome, value) in object {
        if !OUTCOMES.contains(&outcome.as_str()) {
            return Err(invalid("unknown report outcome count"));
        }
        let count = value
            .as_u64()
            .ok_or_else(|| invalid("invalid report outcome count"))?;
        if count != 0 {
            result.insert(outcome.clone(), count);
        }
    }
    Ok(result)
}

fn single_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        && !value.ends_with(['.', ' '])
        && matches!(
            Path::new(value).components().next(),
            Some(Component::Normal(_))
        )
        && Path::new(value).components().count() == 1
}

fn validate_records<'a>(
    summary: &'a Value,
    family: &str,
    total: &mut usize,
) -> io::Result<(&'a [Value], BTreeMap<String, u64>)> {
    let records = summary["records"]
        .as_array()
        .ok_or_else(|| invalid("report records must be an array"))?;
    *total = total
        .checked_add(records.len())
        .filter(|count| *count <= MAX_RECORDS)
        .ok_or_else(|| invalid("report exceeds the 10000 mutant limit"))?;
    let mut ids = BTreeSet::new();
    let mut counts = BTreeMap::new();
    for record in records {
        let id = text(record, "mutantId")?;
        let outcome = text(record, "outcome")?;
        if !record.is_object()
            || record["recordKind"] != "mutation"
            || record["family"] != family
            || !single_component(id)
            || !ids.insert(id)
            || !OUTCOMES.contains(&outcome)
            || !record["reason"].is_string()
        {
            return Err(invalid(
                "invalid, duplicate or mixed-family mutation record",
            ));
        }
        for key in ["operatorId", "file"] {
            if record
                .get(key)
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                return Err(invalid(format!("invalid mutation {key}")));
            }
        }
        if let Some(edit) = record.get("edit").filter(|edit| !edit.is_null()) {
            let expected = edit["expected"].as_str();
            if !edit.is_object()
                || edit["offsetBytes"].as_u64().is_none()
                || edit["lengthBytes"].as_u64().is_none()
                || !edit["replacement"].is_string()
                || expected.is_none()
                || edit["lengthBytes"].as_u64() != expected.map(|text| text.len() as u64)
            {
                return Err(invalid("invalid UTF-8 mutation edit"));
            }
        }
        *counts.entry(outcome.to_owned()).or_default() += 1;
    }
    if count_map(&summary["outcomeCounts"])? != counts
        || optional_count(summary, "completedMutants")? != Some(records.len() as u64)
    {
        return Err(invalid("report counts disagree with retained records"));
    }
    if summary["status"] == "no_mutants" && !records.is_empty() {
        return Err(invalid("no_mutants report contains mutation records"));
    }
    if let Some(expected) = optional_count(summary, "expectedMutants")? {
        if expected > MAX_RECORDS as u64 || expected < records.len() as u64 {
            return Err(invalid("report expected mutant count is inconsistent"));
        }
        if summary["complete"] == true && expected != records.len() as u64 {
            return Err(invalid("completed report is missing mutation records"));
        }
    }
    Ok((records, counts))
}

fn score(
    summary: &Value,
    family: &str,
    counts: &BTreeMap<String, u64>,
    publish: bool,
) -> io::Result<Value> {
    let key = match family {
        "classic" => "mutationScore",
        "failure_mode" => "failureModeScore",
        "language_specific" => "languageSpecificScore",
        _ => unreachable!("family validated"),
    };
    let Some(score) = summary.get(key).filter(|score| !score.is_null()) else {
        return Ok(Value::Null);
    };
    let killed = counts.get("KILLED").copied().unwrap_or(0);
    let survived = counts.get("SURVIVED").copied().unwrap_or(0);
    let denominator = killed + survived;
    let percent_valid = if denominator == 0 {
        score["percent"].is_null()
    } else {
        score["percent"].as_f64().is_some_and(|percent| {
            percent.is_finite()
                && (percent - 100.0 * killed as f64 / denominator as f64).abs() < 0.000_001
        })
    };
    let excluded = counts
        .iter()
        .filter(|(outcome, _)| !matches!(outcome.as_str(), "KILLED" | "SURVIVED"))
        .map(|(key, value)| (key.clone(), *value))
        .collect::<BTreeMap<_, _>>();
    if !score.is_object()
        || score["family"] != family
        || score["killed"].as_u64() != Some(killed)
        || score["survived"].as_u64() != Some(survived)
        || score["denominator"].as_u64() != Some(denominator)
        || !percent_valid
        || count_map(&score["excluded"])? != excluded
    {
        return Err(invalid("report score disagrees with retained outcomes"));
    }
    Ok(if publish { score.clone() } else { Value::Null })
}

fn unavailable(name: &str, message: &str, truncated: bool) -> Value {
    json!({"name": name, "text": "", "truncated": truncated, "unavailable": message})
}

fn log(root: &Path, path: &Path, name: &str, remaining: &mut usize) -> Value {
    if *remaining == 0 {
        return unavailable(name, "Embedded log budget reached.", true);
    }
    let path = match contained(root, path) {
        Ok(path) => path,
        Err(error) => {
            let message = if error.kind() == io::ErrorKind::NotFound {
                "Capture was not retained."
            } else {
                "Capture is unavailable or resolves outside the run directory."
            };
            return unavailable(name, message, false);
        }
    };
    let limit = MAX_LOG_BYTES.min(*remaining);
    let mut bytes = match read_regular_prefix(&path, limit) {
        Ok(bytes) => bytes,
        Err(_) => return unavailable(name, "Capture is not a readable regular file.", false),
    };
    let mut truncated = bytes.len() > limit;
    bytes.truncate(limit);
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    *remaining -= text.len();
    json!({"name": name, "text": text, "truncated": truncated})
}

fn attachments(root: &Path, family: Option<&str>, record: &Value, budget: &mut usize) -> Value {
    let mut directory = root.to_path_buf();
    if let Some(family) = family {
        directory = directory.join("families").join(family);
    }
    directory = directory
        .join("mutants")
        .join(record["mutantId"].as_str().unwrap());
    let mut record = record.clone();
    record["reportLogs"] = json!([
        log(
            root,
            &directory.join("capture/stdout.bin"),
            "stdout.bin",
            budget
        ),
        log(
            root,
            &directory.join("capture/stderr.bin"),
            "stderr.bin",
            budget
        ),
        log(
            root,
            &directory.join("runner.stderr"),
            "runner.stderr",
            budget
        ),
    ]);
    record
}

/// Load retained evidence without requiring the original source project.
pub(crate) fn load(input: &Path) -> io::Result<Value> {
    load_with_source(input, None)
}

/// An explicitly supplied original project may supplement older records, but
/// only exact source hashes and byte edits from the retained run are accepted.
pub(crate) fn load_with_source(input: &Path, source_root: Option<&Path>) -> io::Result<Value> {
    let (root, summary_path) = if fs::metadata(input)?.is_dir() {
        let root = input.canonicalize()?;
        let summary = root.join("summary.json");
        (root, summary)
    } else {
        let parent = input
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        (parent.canonicalize()?, input.to_path_buf())
    };
    let summary = read_json(&root, &summary_path, MAX_SUMMARY_BYTES)?;
    if !summary.is_object() {
        return Err(invalid("report summary must be an object"));
    }
    let profile = summary["kind"] == "profile";
    if (profile && summary["version"] != 2)
        || (!profile && (summary["version"] != 1 || !summary["kind"].is_null()))
    {
        return Err(invalid("unsupported report summary version"));
    }
    let run_id = text(&summary, "runId")?;
    let manifest = read_json(&root, &root.join("manifest.json"), MAX_MANIFEST_BYTES)
        .ok()
        .filter(|manifest| manifest["runId"] == run_id);
    let source_root = source_root.map(Path::canonicalize).transpose()?;
    if source_root.as_ref().is_some_and(|path| !path.is_dir()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source root must be a directory",
        ));
    }
    let mut sources = SourceContexts {
        root: source_root.as_deref(),
        hashes: manifest.as_ref().map(|value| &value["originalInputHashes"]),
        files: BTreeMap::new(),
        remaining_files: MAX_SOURCE_CACHE_BYTES,
        remaining_context: MAX_CONTEXT_BYTES,
    };
    let publication_complete = complete(&summary, false)?;
    if summary["status"] == "quality_stopped" && summary.get("qualityGate").is_none() {
        return Err(invalid("quality-stopped report lacks a gate decision"));
    }
    let profile_complete = optional_bool(&summary, "profileComplete")?;
    let run_complete = publication_complete && profile_complete != Some(false);
    if !summary["error"].is_null() && !summary["error"].is_string() {
        return Err(invalid("invalid report error"));
    }
    if !summary["wallSeconds"].is_null()
        && !summary["wallSeconds"]
            .as_f64()
            .is_some_and(|seconds| seconds.is_finite() && seconds >= 0.0)
    {
        return Err(invalid("invalid report wallSeconds"));
    }
    for key in [
        "originalProjectFilesUnchanged",
        "temporarySnapshotCleaned",
        "workspaceCleanupDeferred",
    ] {
        optional_bool(&summary, key)?;
    }
    let mut record_total = 0;
    let mut budget = MAX_TOTAL_LOG_BYTES;
    let mut families = Vec::new();
    let mut combined_counts = BTreeMap::new();
    let mut combined_records = BTreeMap::new();
    let mut quality_gates = BTreeMap::new();
    let selected = if profile {
        let object = summary["families"]
            .as_object()
            .ok_or_else(|| invalid("profile families must be an object"))?;
        if object.is_empty() || object.keys().any(|key| !FAMILIES.contains(&key.as_str())) {
            return Err(invalid("profile contains unknown or no families"));
        }
        FAMILIES
            .iter()
            .filter_map(|name| object.get(*name).map(|value| (*name, value)))
            .collect()
    } else {
        let family = text(&summary, "family")?;
        if !FAMILIES.contains(&family) {
            return Err(invalid("unknown mutation family"));
        }
        vec![(family, &summary)]
    };
    for (id, entry) in selected {
        let enabled = !profile || boolean(entry, "enabled")?;
        if !enabled {
            if entry["status"] != "disabled" || !entry["summary"].is_null() {
                return Err(invalid("disabled profile family contains a summary"));
            }
            families.push(
                json!({"id":id,"enabled":false,"status":"disabled","complete":true,
                "expectedMutants":null,"outcomeCounts":{},"score":null,"records":[]}),
            );
            continue;
        }
        let child = if profile { &entry["summary"] } else { entry };
        if !child.is_object() || child["family"] != id || child["runId"] != run_id {
            return Err(invalid("profile summary family or run identity disagrees"));
        }
        let child_complete = complete(child, true)?;
        if summary["status"] == "quality_stopped"
            && matches!(text(child, "status")?, "failed" | "cancelled" | "running")
        {
            return Err(invalid(
                "quality stop cannot mask a failed or active family",
            ));
        }
        if run_complete && !child_complete {
            return Err(invalid("completed profile contains an incomplete family"));
        }
        if profile && (entry["status"] != child["status"] || entry["complete"] != child_complete) {
            return Err(invalid("profile family status disagrees with its summary"));
        }
        if profile
            && (child["version"] != 1
                || optional_bool(child, "profileComplete")?
                    .is_some_and(|complete| complete != publication_complete)
                || entry
                    .get("expectedMutants")
                    .is_some_and(|expected| expected != &child["expectedMutants"]))
        {
            return Err(invalid("profile family publication metadata disagrees"));
        }
        let (records, counts) = validate_records(child, id, &mut record_total)?;
        let score = score(child, id, &counts, run_complete && child_complete)?;
        let campaign_status = if !profile && profile_complete == Some(false) {
            child["qualityGate"]["campaignStatus"]
                .as_str()
                .unwrap_or("failed")
        } else {
            text(&summary, "status")?
        };
        if !profile
            && profile_complete == Some(false)
            && matches!(campaign_status, "completed" | "no_mutants")
        {
            return Err(invalid(
                "quality gate claims a complete unpublished profile",
            ));
        }
        if let Some(configured) = manifest
            .as_ref()
            .and_then(|manifest| manifest.get("configuration"))
            .and_then(|configuration| configuration.get("quality_gate"))
        {
            let threshold = if profile {
                &configured["min_scores"][id]
            } else {
                &configured["min_score"]
            };
            if !threshold.is_null() && child.get("qualityGate").is_none() {
                return Err(invalid(
                    "configured quality gate is absent from retained summary",
                ));
            }
        }
        let quality_gate = crate::campaign::quality_gate::validate_stored(
            child.get("qualityGate"),
            text(child, "status")?,
            campaign_status,
            optional_count(child, "expectedMutants")?,
            &counts,
            records.len() as u64,
        )?;
        if !quality_gate.is_null() {
            if matches!(
                campaign_status,
                "completed" | "no_mutants" | "quality_stopped"
            ) && (child["originalProjectFilesUnchanged"] != true
                || child["temporarySnapshotCleaned"] != true
                || child["workspaceCleanupDeferred"] != false
                || !child["error"].is_null())
            {
                return Err(invalid("quality gate lacks clean execution evidence"));
            }
            if let Some(configuration) = manifest
                .as_ref()
                .and_then(|manifest| manifest.get("configuration"))
            {
                if configuration.is_object() {
                    let configured = configuration.get("quality_gate").ok_or_else(|| {
                        invalid("quality gate is absent from retained configuration")
                    })?;
                    let threshold = if profile {
                        &configured["min_scores"][id]
                    } else {
                        &configured["min_score"]
                    };
                    if failforge_config::quality_gate::ScoreThreshold::from_value(threshold).ok()
                        != failforge_config::quality_gate::ScoreThreshold::from_value(
                            &quality_gate["minScore"],
                        )
                        .ok()
                        || configured["stop_when_unreachable"]
                            != quality_gate["stopWhenUnreachable"]
                    {
                        return Err(invalid(
                            "quality gate threshold disagrees with retained configuration",
                        ));
                    }
                }
            }
            quality_gates.insert(id.to_owned(), quality_gate.clone());
        }
        for (outcome, count) in &counts {
            *combined_counts.entry(outcome.clone()).or_insert(0u64) += count;
        }
        for record in records {
            combined_records.insert((id, record["mutantId"].as_str().unwrap()), record);
        }
        let records = records
            .iter()
            .map(|record| {
                let mut record = attachments(&root, profile.then_some(id), record, &mut budget);
                sources.enrich(&mut record);
                record
            })
            .collect::<Vec<_>>();
        let mut projected_family = json!({"id":id,"enabled":true,"status":child["status"],"complete":child_complete,
            "expectedMutants":child["expectedMutants"],"outcomeCounts":counts,"score":score,
            "records":records});
        if !quality_gate.is_null() {
            projected_family["qualityGate"] = quality_gate;
        }
        families.push(projected_family);
    }
    if !families.iter().any(|family| family["enabled"] == true) {
        return Err(invalid("profile has no enabled mutation family"));
    }
    if profile {
        let rebuilt =
            crate::campaign::quality_gate::profile(quality_gates, text(&summary, "status")?);
        if summary.get("qualityGate") != rebuilt.as_ref() {
            return Err(invalid(
                "profile quality gate disagrees with family decisions",
            ));
        }
        let records = summary["records"]
            .as_array()
            .ok_or_else(|| invalid("profile records must be an array"))?;
        if records.len() != record_total || count_map(&summary["outcomeCounts"])? != combined_counts
        {
            return Err(invalid("profile totals disagree with family summaries"));
        }
        let mut seen = BTreeSet::new();
        for record in records {
            let identity = (text(record, "family")?, text(record, "mutantId")?);
            if !seen.insert(identity) || combined_records.get(&identity).copied() != Some(record) {
                return Err(invalid("profile records disagree with family summaries"));
            }
        }
        if FAMILIES.iter().any(|family| {
            let key = match *family {
                "classic" => "mutationScore",
                "failure_mode" => "failureModeScore",
                _ => "languageSpecificScore",
            };
            !summary[key].is_null()
        }) {
            return Err(invalid(
                "profile must not contain a combined mutation score",
            ));
        }
    }
    if optional_count(&summary, "completedMutants")? != Some(record_total as u64) {
        return Err(invalid(
            "report completedMutants disagrees with retained records",
        ));
    }
    if summary["status"] == "no_mutants" && record_total != 0 {
        return Err(invalid("no_mutants profile contains mutation records"));
    }
    if let Some(expected) = optional_count(&summary, "expectedMutants")? {
        if expected > MAX_RECORDS as u64
            || expected < record_total as u64
            || run_complete && expected != record_total as u64
        {
            return Err(invalid(
                "report expectedMutants disagrees with retained records",
            ));
        }
    }
    let configuration = manifest
        .and_then(|manifest| manifest.get("configuration").cloned())
        .filter(Value::is_object)
        .unwrap_or(Value::Null);
    let mut result = json!({"schemaVersion":1,
        "run":{"id":run_id,"status":summary["status"],"complete":run_complete,
            "error":summary["error"],"wallSeconds":summary["wallSeconds"],
            "expectedMutants":summary["expectedMutants"],"completedMutants":record_total,
            "originalProjectFilesUnchanged":summary["originalProjectFilesUnchanged"],
            "temporarySnapshotCleaned":summary["temporarySnapshotCleaned"],
            "workspaceCleanupDeferred":summary["workspaceCleanupDeferred"]},
        "configuration":configuration,"families":families});
    if let Some(gate) = summary.get("qualityGate") {
        result["run"]["qualityGate"] = gate.clone();
    }
    Ok(result)
}

struct SourceContexts<'a> {
    root: Option<&'a Path>,
    hashes: Option<&'a Value>,
    files: BTreeMap<(String, String), Result<Vec<u8>, String>>,
    remaining_files: usize,
    remaining_context: usize,
}

fn valid_source_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_context(context: &Value, expected_hash: Option<&str>) -> bool {
    let coordinates = ["line", "column", "endLine", "startLine"].map(|key| {
        context[key]
            .as_u64()
            .filter(|value| *value > 0 && *value <= 4 * 1024 * 1024 + 1)
    });
    let [Some(line), Some(column), Some(end), Some(start)] = coordinates else {
        return false;
    };
    let Some(lines) = context["lines"].as_array() else {
        return false;
    };
    let Some(hash) = context["sourceHash"].as_str() else {
        return false;
    };
    context["version"] == 1
        && valid_source_hash(hash)
        && expected_hash.is_none_or(|expected| expected.eq_ignore_ascii_case(hash))
        && start <= line
        && end >= line
        && !lines.is_empty()
        && lines.len() <= 7
        && line < start + lines.len() as u64
        && context["truncated"].is_boolean()
        && lines.iter().all(|line| {
            line.as_str()
                .is_some_and(|line| line.len() <= 2 * 1024 && !line.contains(['\r', '\n']))
        })
        && (context["truncated"] == true
            || (end < start + lines.len() as u64
                && column
                    <= lines[(line - start) as usize]
                        .as_str()
                        .map(|line| line.encode_utf16().count() as u64 + 1)
                        .unwrap_or(0)))
        && lines
            .iter()
            .filter_map(Value::as_str)
            .map(str::len)
            .sum::<usize>()
            <= 16 * 1024
}

impl SourceContexts<'_> {
    fn enrich(&mut self, record: &mut Value) {
        let file = record["file"].as_str().unwrap_or_default().to_owned();
        let recorded_hash = record["sourceHash"].as_str();
        let manifest_hash = self.hashes.and_then(|hashes| hashes[&file].as_str());
        let hash = manifest_hash.or(recorded_hash).map(str::to_ascii_lowercase);
        let inconsistent = recorded_hash
            .zip(manifest_hash)
            .is_some_and(|(recorded, manifest)| !recorded.eq_ignore_ascii_case(manifest));
        let retained = record
            .as_object_mut()
            .and_then(|record| record.remove("sourceContext"));
        let mut context =
            retained.filter(|value| !inconsistent && valid_context(value, hash.as_deref()));
        let mut unavailable = inconsistent
            .then_some("Source hashes in the retained record and manifest disagree.".to_owned());
        if let Some(root) = self.root.filter(|_| context.is_none() && !inconsistent) {
            match hash.as_deref().filter(|hash| valid_source_hash(hash)) {
                None => {
                    unavailable = Some("No original source hash was retained for this file.".into())
                }
                Some(hash) => {
                    let key = (file.clone(), hash.to_owned());
                    if !self.files.contains_key(&key) {
                        let contents = if self.remaining_files == 0 {
                            Err("Source preview cache budget reached.".to_owned())
                        } else {
                            crate::report_html::source::read_verified(root, &file, hash)
                                .map_err(|error| error.to_string())
                                .and_then(|bytes| {
                                    if bytes.len() > self.remaining_files {
                                        Err("Source preview cache budget reached.".to_owned())
                                    } else {
                                        self.remaining_files -= bytes.len();
                                        Ok(bytes)
                                    }
                                })
                        };
                        self.files.insert(key.clone(), contents);
                    }
                    match self.files.get(&key).expect("source cache entry") {
                        Ok(bytes) => {
                            match crate::report_html::source::from_bytes(
                                bytes,
                                hash,
                                &record["edit"],
                            ) {
                                Ok(value) if valid_context(&value, Some(hash)) => {
                                    context = Some(value)
                                }
                                _ => {
                                    unavailable = Some(
                                        "The retained edit does not match the original source."
                                            .into(),
                                    )
                                }
                            }
                        }
                        Err(error) => unavailable = Some(error.clone()),
                    }
                }
            }
        }
        if let Some(context) = context {
            let bytes = serde_json::to_vec(&context)
                .map(|bytes| bytes.len())
                .unwrap_or(usize::MAX);
            if bytes <= self.remaining_context {
                self.remaining_context -= bytes;
                record["sourceContext"] = context;
                record
                    .as_object_mut()
                    .unwrap()
                    .remove("sourceContextUnavailable");
                return;
            }
            unavailable = Some("Source context preview budget reached.".into());
        }
        if let Some(message) = unavailable {
            record["sourceContextUnavailable"] =
                message.chars().take(400).collect::<String>().into();
        }
    }
}

#[cfg(test)]
#[path = "data_tests.rs"]
mod tests;
