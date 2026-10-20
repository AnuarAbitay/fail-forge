//! In-process report ownership from prepared metadata through final commit.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::time::Instant;

use serde_json::{Value, json};

use crate::campaign_publish::Publisher;
use crate::campaign_publish_fs::invalid;
#[path = "campaign_reports_fs.rs"]
mod filesystem;
#[path = "campaign_replay_report.rs"]
mod replay_report;
use filesystem::Layout;

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

struct Family {
    configuration: Value,
    output: PathBuf,
    expected: Option<usize>,
    status: String,
    error: Option<String>,
    records: Vec<Value>,
    ids: BTreeSet<String>,
    baseline: Option<Value>,
    controls: Vec<Value>,
    classic_metadata: Option<Value>,
    timings: BTreeMap<String, f64>,
    wall: f64,
}

impl Family {
    fn new(configuration: Value) -> Self {
        Self {
            configuration,
            output: PathBuf::new(),
            expected: None,
            status: "not_started".into(),
            error: None,
            records: vec![],
            ids: BTreeSet::new(),
            baseline: None,
            controls: vec![],
            classic_metadata: None,
            timings: BTreeMap::new(),
            wall: 0.0,
        }
    }

    fn record(&mut self, family: &str, value: &Value) -> io::Result<()> {
        let id = value["mutantId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| invalid("invalid retained mutation identity"))?;
        if value["recordKind"] != "mutation"
            || value["family"] != family
            || !value["outcome"]
                .as_str()
                .is_some_and(|value| OUTCOMES.contains(&value))
            || !value["reason"].is_string()
            || self.ids.contains(id)
        {
            return Err(invalid("invalid or duplicate retained mutation result"));
        }
        if self
            .expected
            .is_some_and(|expected| self.records.len() >= expected)
        {
            return Err(invalid(
                "retained mutation count exceeds discovered candidates",
            ));
        }
        self.ids.insert(id.into());
        self.records.push(value.clone());
        Ok(())
    }

    fn projected(&self, family: &str) -> Value {
        let records = self
            .records
            .iter()
            .map(|row| {
                json!({
            "recordKind":row["recordKind"], "family":row["family"], "mutantId":row["mutantId"],
            "outcome":row["outcome"], "reason":row["reason"]})
            })
            .collect::<Vec<_>>();
        json!({"family":family,"language":if family == "language_specific" {json!("dart")} else {Value::Null},
            "output":self.output,"status":self.status,"error":self.error,"expectedMutants":self.expected,
            "records":records,"baseline":self.baseline.as_ref().map(|_| json!({})),
            "originalControls":self.controls.iter().map(|_| json!({})).collect::<Vec<_>>(),
            "timings":self.timings,"wallSeconds":self.wall})
    }

    fn hydrate(&self, summary: &mut Value) -> io::Result<()> {
        let records = self
            .records
            .iter()
            .map(|row| (row["mutantId"].as_str().unwrap(), row))
            .collect::<BTreeMap<_, _>>();
        let rows = summary["records"]
            .as_array()
            .ok_or_else(|| invalid("invalid final report records"))?
            .iter()
            .map(|row| {
                records
                    .get(row["mutantId"].as_str().unwrap_or_default())
                    .map(|row| (*row).clone())
                    .ok_or_else(|| invalid("final report mutation identity changed"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        summary["records"] = json!(rows);
        summary["baseline"] = json!(self.baseline);
        summary["originalControls"] = json!(self.controls);
        if let Some(metadata) = &self.classic_metadata {
            replay_report::annotate_discovered(summary, metadata)?;
        }
        Ok(())
    }
}

pub(crate) struct FinalReport {
    pub(crate) summary: Value,
    pub(crate) terminal: String,
}

/// Reporting faults latch independently of fact collection, so cancellation
/// cannot discard valid results already sealed by concurrent workers.
pub(crate) struct Reports {
    mode: String,
    configuration: Value,
    families: BTreeMap<String, Family>,
    prepared: bool,
    root: Option<File>,
    output: PathBuf,
    run_id: String,
    layout: Option<Layout>,
    publisher: Option<Publisher>,
    started: Instant,
    error: Option<String>,
    expected: Option<usize>,
    current: Option<String>,
    frozen: Option<Value>,
    retained_replay: Option<Value>,
    initialized: bool,
    finished: bool,
    timings: BTreeMap<String, f64>,
    cleanup: Value,
}

impl Reports {
    pub(crate) fn new(
        mode: &str,
        configuration: Value,
        family_configs: BTreeMap<String, Value>,
    ) -> io::Result<Self> {
        if !matches!(mode, "direct" | "profile")
            || !configuration.is_object()
            || family_configs.is_empty()
            || family_configs.len() > 3
            || mode == "direct" && family_configs.len() != 1
            || family_configs
                .iter()
                .any(|(family, config)| !FAMILIES.contains(&family.as_str()) || !config.is_object())
        {
            return Err(invalid("invalid campaign reporting configuration"));
        }
        Ok(Self {
            mode: mode.into(),
            configuration,
            families: family_configs
                .into_iter()
                .map(|(name, config)| (name, Family::new(config)))
                .collect(),
            prepared: false,
            root: None,
            output: PathBuf::new(),
            run_id: String::new(),
            layout: None,
            publisher: None,
            started: Instant::now(),
            error: None,
            expected: None,
            current: None,
            frozen: None,
            retained_replay: None,
            initialized: false,
            finished: false,
            timings: BTreeMap::new(),
            cleanup: json!({"originalProjectFilesUnchanged":false,"temporarySnapshotCleaned":false,
                "workspaceRecovery":{"recovered":[],"skipped":[]},"workspaceDirectory":null,"workspaceCleanupDeferred":true}),
        })
    }

    pub(crate) fn bind_output(&mut self, root: File) -> io::Result<()> {
        if self.root.is_some() || self.prepared {
            return Err(invalid("report output was already bound"));
        }
        self.root = Some(root);
        Ok(())
    }

    pub(crate) fn prepared(&self) -> bool {
        self.prepared
    }
    pub(crate) fn failure(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn fail(&mut self, error: &io::Error) {
        self.error
            .get_or_insert_with(|| format!("campaign report publication failed: {error}"));
    }

    pub(crate) fn event(&mut self, value: &Value) -> io::Result<()> {
        if self.finished {
            return Err(invalid("campaign reports are already committed"));
        }
        let result = self.observe(value);
        if let Err(error) = &result {
            self.fail(error);
        }
        result
    }

    fn prepare(&mut self, value: &Value) -> io::Result<()> {
        if self.prepared {
            return Err(invalid("campaign reports were already prepared"));
        }
        let run_id = value["runId"]
            .as_str()
            .ok_or_else(|| invalid("missing report run identity"))?;
        if run_id.len() != 32
            || !run_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid("invalid report run identity"));
        }
        let output = PathBuf::from(
            value["output"]
                .as_str()
                .ok_or_else(|| invalid("missing report output"))?,
        );
        crate::campaign_publish_fs::path_text(&output)?;
        let selections = value["families"]
            .as_array()
            .ok_or_else(|| invalid("missing prepared report families"))?;
        let names = selections
            .iter()
            .map(|row| {
                row["family"]
                    .as_str()
                    .ok_or_else(|| invalid("invalid prepared report family"))
            })
            .collect::<io::Result<BTreeSet<_>>>()?;
        if names.len() != selections.len()
            || names != self.families.keys().map(String::as_str).collect()
        {
            return Err(invalid("prepared report selection changed"));
        }
        if let Some(binding) = value.get("retainedReplay").filter(|value| !value.is_null()) {
            if !binding.is_object() || self.mode != "direct" {
                return Err(invalid("invalid retained replay binding"));
            }
            self.retained_replay = Some(binding.clone());
        }
        self.prepared = true;
        self.output = output;
        self.run_id = run_id.into();
        for (family, run) in &mut self.families {
            run.output = if self.mode == "profile" {
                self.output.join("families").join(family)
            } else {
                self.output.clone()
            };
            run.configuration["flutter_sdk"] = value["runtime"]["flutterSdk"].clone();
        }
        let families = self.selected();
        let root = self
            .root
            .take()
            .ok_or_else(|| invalid("report output capability was not bound"))?;
        // Even a partially initialized profile can publish an honest root-only
        // failure report through its originally retained output capability.
        self.publisher = Some(Publisher::new(
            &self.output,
            root.try_clone()?,
            false,
            &families[..1],
        )?);
        let layout = Layout::new(
            &self.output,
            root,
            &self.run_id,
            &families,
            self.mode == "profile",
        )?;
        self.layout = Some(layout);
        if self.mode == "profile" {
            self.layout.as_mut().unwrap().profile(&families)?;
            self.publisher = Some(self.layout.as_ref().unwrap().publisher(true, &families)?);
        }
        let mut common = json!({"version":1,"runId":self.run_id,"runner":value["runtime"]["runner"],
            "executionEngine":"rust","originalInputHashes":value["originalHashes"]});
        if let Some(binding) = &self.retained_replay {
            common["retainedReplay"] = binding.clone();
            self.layout
                .as_mut()
                .unwrap()
                .write("replay-binding.json", binding)?;
        }
        for selection in selections {
            let family = selection["family"].as_str().unwrap();
            let mut manifest = common.clone();
            manifest["configuration"] = self.families[family].configuration.clone();
            manifest["effectiveRules"] = selection["rules"].clone();
            let path = if self.mode == "profile" {
                manifest["profileRunId"] = self.run_id.clone().into();
                format!("families/{family}/manifest.json")
            } else {
                "manifest.json".into()
            };
            self.layout.as_mut().unwrap().write(&path, &manifest)?;
        }
        if self.mode == "profile" {
            let mut manifest = common;
            manifest["version"] = 2.into();
            manifest["kind"] = "profile".into();
            manifest["configuration"] = self.configuration.clone();
            manifest["families"] = json!(FAMILIES.into_iter().map(|family| {
                let selected = self.families.contains_key(family);
                (family, json!({"enabled":selected,"report":selected.then(||format!("families/{family}"))}))
            }).collect::<BTreeMap<_, _>>());
            self.layout
                .as_mut()
                .unwrap()
                .write("manifest.json", &manifest)?;
        }
        Ok(())
    }

    fn selected(&self) -> Vec<String> {
        FAMILIES
            .into_iter()
            .filter(|name| self.families.contains_key(*name))
            .map(str::to_owned)
            .collect()
    }

    fn observe(&mut self, value: &Value) -> io::Result<()> {
        let kind = value["kind"]
            .as_str()
            .ok_or_else(|| invalid("report event requires a kind"))?;
        if kind == "prepared" {
            return self.prepare(value);
        }
        if !self.prepared {
            return Err(invalid("report event before preparation"));
        }
        if kind == "campaign_event" {
            return self.campaign(&value["event"], None);
        }
        match kind {
            "workspace" => {
                self.cleanup["workspaceDirectory"] = value["directory"].clone();
                let skipped = value["skipped"]
                    .as_array()
                    .ok_or_else(|| invalid("invalid workspace recovery skips"))?
                    .iter()
                    .map(|row| json!({"name":row["name"],"reason":row["reason"]}))
                    .collect::<Vec<_>>();
                self.cleanup["workspaceRecovery"] =
                    json!({"recovered":value["recovered"],"skipped":skipped});
            }
            "bindings" => {
                self.frozen = Some(value["value"].clone());
                if self.error.is_none() {
                    self.layout_mut()?
                        .write("input-bindings.json", &value["value"])?;
                }
            }
            "cleanup" => {
                self.cleanup["originalProjectFilesUnchanged"] = value["originalUnchanged"].clone();
                self.cleanup["temporarySnapshotCleaned"] = value["snapshotCleaned"].clone();
                self.cleanup["workspaceCleanupDeferred"] = value["cleanupDeferred"].clone();
                self.cleanup["workspaceDirectory"] = value["workspaceDirectory"].clone();
            }
            "stage_finished" => add_timing(&mut self.timings, value)?,
            _ => {}
        }
        if self.error.is_none()
            && matches!(
                kind,
                "stage_started" | "stage_finished" | "workspace" | "cleanup"
            )
        {
            self.layout_mut()?
                .record(kind, fields(value, &["kind", "sequence", "version"])?)?;
        }
        Ok(())
    }

    fn layout_mut(&mut self) -> io::Result<&mut Layout> {
        self.layout
            .as_mut()
            .ok_or_else(|| invalid("report layout was not prepared"))
    }

    fn campaign(&mut self, value: &Value, inherited_family: Option<&str>) -> io::Result<()> {
        let kind = value["kind"]
            .as_str()
            .ok_or_else(|| invalid("campaign report event requires a kind"))?;
        let family = inherited_family.or_else(|| value["family"].as_str());
        if kind == "family_event" {
            return self.campaign(&value["event"], family);
        }
        match kind {
            "discovery" => {
                let family = family.ok_or_else(|| invalid("discovery report lacks family"))?;
                let count = value["value"]["candidates"]
                    .as_array()
                    .ok_or_else(|| invalid("invalid discovered candidates"))?
                    .len();
                let run = self
                    .families
                    .get_mut(family)
                    .ok_or_else(|| invalid("unknown report family"))?;
                if run.expected.is_some() {
                    return Err(invalid("duplicate report discovery"));
                }
                run.expected = Some(count);
                if family == "classic" && self.retained_replay.is_none() {
                    run.classic_metadata = replay_report::discovered_metadata(&value["value"]);
                }
                if self.error.is_none() && self.mode == "profile" {
                    let frozen = self
                        .frozen
                        .clone()
                        .ok_or_else(|| invalid("discovery precedes input bindings"))?;
                    self.layout_mut()?
                        .write(&format!("families/{family}/input-bindings.json"), &frozen)?;
                }
            }
            "discovery_finished" => {
                self.expected = Some(
                    usize::try_from(
                        value["expectedMutants"]
                            .as_u64()
                            .ok_or_else(|| invalid("invalid discovered mutant total"))?,
                    )
                    .map_err(|_| invalid("mutant total overflow"))?,
                );
                self.current = None;
            }
            "stage_started" => {
                if matches!(
                    value["stage"].as_str(),
                    Some(
                        "discovery_validation"
                            | "verify_discovery_inputs"
                            | "verify_execution_inputs"
                            | "verify_inputs"
                    )
                ) {
                    self.current = family.map(str::to_owned);
                } else if value["stage"] == "verify_profile_inputs" {
                    self.current = None;
                }
            }
            "stage_finished" => {
                let timings = match (self.mode.as_str(), family) {
                    ("direct", _) | (_, None) => &mut self.timings,
                    (_, Some(family)) => {
                        &mut self
                            .families
                            .get_mut(family)
                            .ok_or_else(|| invalid("unknown timing family"))?
                            .timings
                    }
                };
                add_timing(timings, value)?;
            }
            "family_started" => {
                let family = family.ok_or_else(|| invalid("family start lacks family"))?;
                self.families
                    .get_mut(family)
                    .ok_or_else(|| invalid("unknown started family"))?
                    .status = "running".into();
                self.current = Some(family.into());
            }
            "family_finished" => {
                let run = self
                    .families
                    .get_mut(family.ok_or_else(|| invalid("family finish lacks family"))?)
                    .ok_or_else(|| invalid("unknown finished family"))?;
                let status = value["status"]
                    .as_str()
                    .filter(|status| {
                        matches!(*status, "completed" | "no_mutants" | "failed" | "cancelled")
                    })
                    .ok_or_else(|| invalid("invalid report family status"))?;
                run.status = status.into();
                run.error = nullable_text(&value["error"])?;
                run.wall = seconds(&value["wallSeconds"])?;
                self.current = None;
            }
            "baseline" | "original_before" | "original_after" | "mutant" => {
                let family = family.ok_or_else(|| invalid("result report lacks family"))?;
                let run = self
                    .families
                    .get_mut(family)
                    .ok_or_else(|| invalid("unknown result family"))?;
                if kind == "mutant" {
                    run.record(family, &value["record"])?;
                } else {
                    let attempt = &value["attempt"];
                    if !attempt.is_object() {
                        return Err(invalid("retained attempt must be an object"));
                    }
                    if kind == "baseline" {
                        run.baseline = Some(attempt.clone());
                    } else {
                        run.controls.push(attempt.clone());
                    }
                }
            }
            _ => {}
        }
        if self.error.is_some() {
            return Ok(());
        }
        if matches!(
            kind,
            "stage_started" | "stage_finished" | "family_started" | "family_finished"
        ) {
            let mut fields = fields(value, &["kind", "family"])?;
            if let Some(family) = family {
                fields["family"] = family.into();
            }
            self.layout_mut()?.record(kind, fields)?;
        } else if kind == "mutant" {
            let family = family.unwrap();
            let run = &self.families[family];
            let record = &value["record"];
            let fields = json!({"family":family,"mutantId":record["mutantId"],"outcome":record["outcome"],
                "reason":record["reason"],"completed":run.records.len(),"total":run.expected});
            self.layout_mut()?.record("mutant_completed", fields)?;
        }
        if matches!(
            kind,
            "discovery_finished" | "family_started" | "family_finished" | "baseline" | "mutant"
        ) && !self.initialized
        {
            let mut summary = self.summary("failed", None)?;
            summary["status"] = "running".into();
            let families = self.selected();
            let profile = self.mode == "profile";
            self.layout_mut()?.initial(&summary, profile, &families)?;
            self.initialized = true;
        }
        Ok(())
    }

    fn summary(&self, status: &str, error: Option<&str>) -> io::Result<Value> {
        let request = json!({"operation":"campaign-report","mode":self.mode,"runId":self.run_id,
            "output":self.output,"status":status,"error":error,"expectedMutants":self.expected,
            "timings":self.timings,"wallSeconds":self.started.elapsed().as_secs_f64(),"cleanup":self.cleanup,
            "families":self.selected().iter().map(|family| self.families[family].projected(family)).collect::<Vec<_>>()});
        let mut summary = crate::campaign_report::build(&serde_json::to_vec(&request)?)
            .map_err(|error| invalid(&error.to_string()))?;
        if self.mode == "direct" {
            self.families
                .values()
                .next()
                .unwrap()
                .hydrate(&mut summary)?;
            if let Some(binding) = &self.retained_replay {
                replay_report::annotate(&mut summary, binding)?;
                summary["retainedReplay"] = binding.clone();
            }
        } else {
            let mut records = vec![];
            for family in self.selected() {
                let child = &mut summary["families"][&family]["summary"];
                self.families[&family].hydrate(child)?;
                records.extend(child["records"].as_array().unwrap().iter().cloned());
            }
            summary["records"] = json!(records);
        }
        Ok(summary)
    }

    pub(crate) fn finish(&mut self, status: &str, error: Option<&str>) -> io::Result<FinalReport> {
        if !self.prepared || self.finished {
            return Err(invalid("campaign reports cannot be finalized"));
        }
        if !matches!(status, "completed" | "no_mutants" | "failed" | "cancelled") {
            return Err(invalid("invalid final report status"));
        }
        let mut status = status.to_owned();
        let mut error = error.map(str::to_owned);
        if self.error.is_some() && status != "failed" {
            status = "failed".into();
            error = self.error.clone();
        }
        self.apply_final(&status, error.as_deref());
        if self.error.is_none() {
            let kind = if self.mode == "profile" {
                "profile_finished"
            } else {
                "campaign_finished"
            };
            let fields = json!({"status":status,"complete":matches!(status.as_str(), "completed" | "no_mutants"),"output":self.output});
            if let Err(caught) = self
                .layout_mut()
                .and_then(|layout| layout.record(kind, fields))
            {
                self.fail(&caught);
                if status != "failed" {
                    status = "failed".into();
                    error = self.error.clone();
                }
                self.apply_final(&status, error.as_deref());
            }
        }
        let mut summary = self.summary(&status, error.as_deref())?;
        let mut terminal = crate::campaign_report_terminal::format_summary(&summary)
            .map_err(|error| invalid(&error.to_string()))?;
        let progress = crate::campaign_progress::from_summary(&summary)?;
        let publisher = self
            .publisher
            .as_ref()
            .ok_or_else(|| invalid("final report layout was not bound"))?;
        if let Err(caught) = publisher.publish(&summary, Some(&progress)) {
            // Existing publication code guarantees Err means its final rename
            // did not succeed. Its stages are preserved and a partial retry is safe.
            if status != "failed" {
                status = "failed".into();
                error = Some(format!("report publication failed: {caught}"));
            }
            self.apply_final(&status, error.as_deref());
            summary = self.summary(&status, error.as_deref())?;
            terminal = crate::campaign_report_terminal::format_summary(&summary)
                .map_err(|error| invalid(&error.to_string()))?;
            self.publisher.as_ref().unwrap().publish(&summary, None)?;
        }
        self.finished = true;
        Ok(FinalReport { summary, terminal })
    }

    fn apply_final(&mut self, status: &str, error: Option<&str>) {
        let family = if self.mode == "direct" {
            self.families.keys().next().cloned()
        } else if matches!(status, "failed" | "cancelled") {
            self.current.clone()
        } else {
            None
        };
        if let Some(run) = family.and_then(|name| self.families.get_mut(&name)) {
            run.status = status.into();
            run.error = error.map(str::to_owned);
        }
    }
}

fn fields(value: &Value, excluded: &[&str]) -> io::Result<Value> {
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("event fields must be an object"))?
        .iter()
        .filter(|(key, _)| !excluded.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    Ok(Value::Object(fields))
}
fn nullable_text(value: &Value) -> io::Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::String(value) => Ok(Some(value.clone())),
        _ => Err(invalid("report error must be text or null")),
    }
}
fn seconds(value: &Value) -> io::Result<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| invalid("report timing must be finite and nonnegative"))
}
fn add_timing(timings: &mut BTreeMap<String, f64>, value: &Value) -> io::Result<()> {
    let stage = value["stage"]
        .as_str()
        .ok_or_else(|| invalid("report stage requires a name"))?;
    let total = timings.get(stage).copied().unwrap_or_default() + seconds(&value["wallSeconds"])?;
    if !total.is_finite() {
        return Err(invalid("report stage timing overflow"));
    }
    timings.insert(stage.into(), total);
    Ok(())
}

#[cfg(test)]
#[path = "campaign_reports_tests.rs"]
mod tests;
