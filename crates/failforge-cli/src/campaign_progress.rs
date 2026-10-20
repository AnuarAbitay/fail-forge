//! Compact campaign progress and descriptor-bound atomic checkpoint writes.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::campaign_publish_fs as files;

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
const STATUSES: [&str; 6] = [
    "not_started",
    "running",
    "completed",
    "no_mutants",
    "failed",
    "cancelled",
];
const FINAL: [&str; 4] = ["completed", "no_mutants", "failed", "cancelled"];

#[derive(Default)]
struct Counts {
    ids: BTreeSet<String>,
    outcomes: BTreeMap<String, u64>,
    reasons: BTreeMap<String, u64>,
}

impl Counts {
    fn add(&mut self, family: &str, record: &Value) -> io::Result<()> {
        let id = record["mutantId"].as_str().filter(|id| !id.is_empty());
        let outcome = record["outcome"]
            .as_str()
            .filter(|name| OUTCOMES.contains(name));
        let reason = record["reason"].as_str();
        if record["recordKind"] != "mutation"
            || record["family"] != family
            || id.is_none()
            || outcome.is_none()
            || reason.is_none()
            || self.ids.contains(id.unwrap_or_default())
        {
            return Err(files::invalid("invalid or duplicate progress mutation"));
        }
        self.ids.insert(id.expect("validated identity").into());
        *self
            .outcomes
            .entry(outcome.expect("validated outcome").into())
            .or_default() += 1;
        *self
            .reasons
            .entry(reason.expect("validated reason").into())
            .or_default() += 1;
        Ok(())
    }

    fn snapshot(&self) -> Value {
        json!({"completedMutants":self.ids.len(),"outcomeCounts":self.outcomes,
            "reasonCounts":self.reasons})
    }
}

struct Family {
    expected: Option<u64>,
    status: String,
    counts: Counts,
}

/// Owns only IDs and counters; opaque attempt results never enter checkpoints.
pub(crate) struct State {
    mode: String,
    run_id: String,
    output: PathBuf,
    families: BTreeMap<String, Family>,
    expected: Option<u64>,
    active: Option<String>,
}

impl State {
    pub(crate) fn new(
        mode: &str,
        run_id: &str,
        output: &Path,
        families: &[String],
    ) -> io::Result<Self> {
        if !matches!(mode, "direct" | "profile")
            || !identity(run_id)
            || families.is_empty()
            || families.len() > 3
            || mode == "direct" && families.len() != 1
            || families
                .iter()
                .any(|name| !FAMILIES.contains(&name.as_str()))
            || families.iter().collect::<BTreeSet<_>>().len() != families.len()
        {
            return Err(files::invalid("invalid progress campaign selection"));
        }
        files::path_text(output)?;
        Ok(Self {
            mode: mode.into(),
            run_id: run_id.into(),
            output: output.into(),
            families: families
                .iter()
                .map(|family| {
                    (
                        family.clone(),
                        Family {
                            expected: None,
                            status: "not_started".into(),
                            counts: Counts::default(),
                        },
                    )
                })
                .collect(),
            expected: None,
            active: None,
        })
    }

    pub(crate) fn observe(&mut self, event: &Value) -> io::Result<bool> {
        self.observe_family(event, event["family"].as_str())
    }

    fn observe_family(&mut self, event: &Value, family: Option<&str>) -> io::Result<bool> {
        let kind = event["kind"]
            .as_str()
            .ok_or_else(|| files::invalid("invalid progress event"))?;
        if kind == "family_event" {
            return self.observe_family(&event["event"], family);
        }
        if kind == "campaign_event" {
            return self.observe(&event["event"]);
        }
        if kind == "discovery_finished" {
            self.expected = Some(
                event["expectedMutants"]
                    .as_u64()
                    .ok_or_else(|| files::invalid("invalid progress total"))?,
            );
            self.active = None;
            return Ok(true);
        }
        if kind == "stage_started" {
            match event["stage"].as_str() {
                Some(
                    "discovery_validation"
                    | "verify_discovery_inputs"
                    | "verify_execution_inputs"
                    | "verify_inputs",
                ) => {
                    if family.is_some_and(|name| !self.families.contains_key(name)) {
                        return Err(files::invalid("unknown progress family"));
                    }
                    self.active = family.map(str::to_owned);
                }
                Some("verify_profile_inputs") => self.active = None,
                _ => {}
            }
            return Ok(false);
        }
        if !matches!(
            kind,
            "discovery" | "family_started" | "family_finished" | "baseline" | "mutant"
        ) {
            return Ok(false);
        }
        let family = family.ok_or_else(|| files::invalid("progress event lacks family"))?;
        let run = self
            .families
            .get_mut(family)
            .ok_or_else(|| files::invalid("unknown progress family"))?;
        match kind {
            "discovery" => {
                let candidates = event["value"]["candidates"]
                    .as_array()
                    .ok_or_else(|| files::invalid("invalid progress discovery"))?;
                if run.expected.is_some() {
                    return Err(files::invalid("duplicate progress discovery"));
                }
                run.expected = Some(candidates.len() as u64);
                Ok(false)
            }
            "family_started" => {
                run.status = "running".into();
                self.active = Some(family.into());
                Ok(true)
            }
            "family_finished" => {
                let status = event["status"]
                    .as_str()
                    .filter(|name| FINAL.contains(name))
                    .ok_or_else(|| files::invalid("invalid progress family status"))?;
                run.status = status.into();
                self.active = None;
                Ok(true)
            }
            "mutant" => {
                if run
                    .expected
                    .is_none_or(|expected| run.counts.ids.len() as u64 >= expected)
                {
                    return Err(files::invalid("progress mutations exceed discovery"));
                }
                run.counts.add(family, &event["record"])?;
                Ok(true)
            }
            _ => Ok(true),
        }
    }

    pub(crate) fn snapshot(&self, seconds: f64, intended: Option<&str>) -> io::Result<Value> {
        if !seconds.is_finite()
            || seconds < 0.0
            || intended.is_some_and(|status| !FINAL.contains(&status))
        {
            return Err(files::invalid("invalid progress timing or status"));
        }
        let mut value = json!({"kind":if self.mode == "profile" {"profile"} else {"campaign"},
            "version":1,"runId":self.run_id,"output":self.output,"complete":false,
            "status":if intended.is_some() {"finalizing"} else {"running"},
            "expectedMutants":self.expected,"wallSeconds":seconds});
        if let Some(status) = intended {
            value["intendedStatus"] = status.into();
        }
        if self.mode == "direct" {
            let (family, run) = self
                .families
                .first_key_value()
                .expect("validated direct family");
            extend(&mut value, run.counts.snapshot());
            value["family"] = family.clone().into();
            if family == "language_specific" {
                value["language"] = "dart".into();
            }
        } else {
            let mut families = json!({});
            let mut total = 0;
            let mut outcomes = BTreeMap::new();
            let mut reasons = BTreeMap::new();
            for family in FAMILIES {
                families[family] = match self.families.get(family) {
                    None => json!({"enabled":false,"status":"disabled"}),
                    Some(run) => {
                        total += run.counts.ids.len();
                        merge(&mut outcomes, &run.counts.outcomes)?;
                        merge(&mut reasons, &run.counts.reasons)?;
                        let mut row = run.counts.snapshot();
                        row["enabled"] = true.into();
                        row["status"] = run.status.clone().into();
                        row["expectedMutants"] = json!(run.expected);
                        if family == "language_specific" {
                            row["language"] = "dart".into();
                        }
                        row
                    }
                };
            }
            extend(
                &mut value,
                json!({"families":families,"completedMutants":total,
                "outcomeCounts":outcomes,"reasonCounts":reasons,
                "activeFamily":if intended.is_some() {None} else {self.active.as_deref()}}),
            );
        }
        Ok(value)
    }
}

fn extend(target: &mut Value, extra: Value) {
    target
        .as_object_mut()
        .expect("progress object")
        .extend(extra.as_object().expect("progress fields").clone());
}

fn merge(target: &mut BTreeMap<String, u64>, source: &BTreeMap<String, u64>) -> io::Result<()> {
    for (name, count) in source {
        let previous = target.entry(name.clone()).or_default();
        *previous = previous
            .checked_add(*count)
            .ok_or_else(|| files::invalid("progress counter overflow"))?;
    }
    Ok(())
}

fn identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Project a validated final summary without copying records or attempt data.
pub(crate) fn from_summary(summary: &Value) -> io::Result<Value> {
    let profile = summary["kind"] == "profile";
    let mut value = json!({"kind":if profile {"profile"} else {"campaign"},"version":1,
        "runId":summary["runId"],"output":summary["output"],"complete":false,
        "status":"finalizing","intendedStatus":summary["status"],
        "expectedMutants":summary["expectedMutants"],"completedMutants":summary["completedMutants"],
        "outcomeCounts":summary["outcomeCounts"],"reasonCounts":summary["reasonCounts"],
        "wallSeconds":summary["wallSeconds"]});
    if profile {
        let mut families = json!({});
        for family in FAMILIES {
            let selected = &summary["families"][family];
            families[family] = if selected["enabled"] == false {
                json!({"enabled":false,"status":"disabled"})
            } else {
                let source = &selected["summary"];
                let mut row = json!({"enabled":true,"status":selected["status"],
                    "expectedMutants":source["expectedMutants"],"completedMutants":source["completedMutants"],
                    "outcomeCounts":source["outcomeCounts"],"reasonCounts":source["reasonCounts"]});
                if family == "language_specific" {
                    row["language"] = source["language"].clone();
                }
                row
            };
        }
        value["families"] = families;
        value["activeFamily"] = Value::Null;
    } else {
        value["family"] = summary["family"].clone();
        if summary["family"] == "language_specific" {
            value["language"] = summary["language"].clone();
        }
    }
    let run_id = summary["runId"]
        .as_str()
        .ok_or_else(|| files::invalid("missing progress identity"))?;
    let output = summary["output"]
        .as_str()
        .ok_or_else(|| files::invalid("missing progress output"))?;
    let complete = summary["complete"]
        .as_bool()
        .ok_or_else(|| files::invalid("missing progress completeness"))?;
    validate_final(&value, run_id, Path::new(output), complete)?;
    Ok(value)
}

fn exact(value: &Value, keys: &[&str]) -> io::Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| files::invalid("progress requires objects"))?;
    if object.len() != keys.len() || keys.iter().any(|key| !object.contains_key(*key)) {
        return Err(files::invalid("invalid progress fields"));
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct Totals {
    completed: u64,
    outcomes: BTreeMap<String, u64>,
    reasons: BTreeMap<String, u64>,
}

fn counts(value: &Value) -> io::Result<Totals> {
    let completed = value["completedMutants"]
        .as_u64()
        .ok_or_else(|| files::invalid("invalid progress count"))?;
    match &value["expectedMutants"] {
        Value::Null => {}
        expected if expected.as_u64().is_some_and(|number| number >= completed) => {}
        _ => return Err(files::invalid("invalid expected progress count")),
    }
    let mut maps = Vec::new();
    for key in ["outcomeCounts", "reasonCounts"] {
        let rows = value[key]
            .as_object()
            .ok_or_else(|| files::invalid("invalid progress counters"))?;
        let mut total = 0u64;
        let mut map = BTreeMap::new();
        for (name, count) in rows {
            let count = count
                .as_u64()
                .filter(|number| *number != 0)
                .ok_or_else(|| files::invalid("invalid progress counter"))?;
            if key == "outcomeCounts" && !OUTCOMES.contains(&name.as_str()) {
                return Err(files::invalid("unknown progress outcome"));
            }
            total = total
                .checked_add(count)
                .ok_or_else(|| files::invalid("progress counter overflow"))?;
            map.insert(name.clone(), count);
        }
        if total != completed {
            return Err(files::invalid("progress counters disagree"));
        }
        maps.push(map);
    }
    Ok(Totals {
        completed,
        outcomes: maps.remove(0),
        reasons: maps.remove(0),
    })
}

pub(crate) fn validate_final(
    value: &Value,
    run_id: &str,
    output: &Path,
    complete: bool,
) -> io::Result<()> {
    files::path_text(output)?;
    let profile = value["kind"] == "profile";
    let mut keys = vec![
        "kind",
        "version",
        "runId",
        "output",
        "complete",
        "status",
        "intendedStatus",
        "expectedMutants",
        "completedMutants",
        "outcomeCounts",
        "reasonCounts",
        "wallSeconds",
    ];
    if profile {
        keys.extend(["families", "activeFamily"]);
    } else {
        keys.push("family");
        if value["family"] == "language_specific" {
            keys.push("language");
        }
    }
    exact(value, &keys)?;
    if !identity(run_id)
        || value["runId"] != run_id
        || value["output"] != files::path_text(output)?
        || value["version"].as_u64() != Some(1)
        || value["complete"] != false
        || value["status"] != "finalizing"
        || !value["intendedStatus"]
            .as_str()
            .is_some_and(|status| FINAL.contains(&status))
        || matches!(
            value["intendedStatus"].as_str(),
            Some("completed" | "no_mutants")
        ) != complete
        || complete && value["expectedMutants"].is_null()
        || !value["wallSeconds"]
            .as_f64()
            .is_some_and(|seconds| seconds.is_finite() && seconds >= 0.0)
    {
        return Err(files::invalid("invalid final progress identity or status"));
    }
    let totals = counts(value)?;
    if complete
        && (value["expectedMutants"].as_u64() != Some(totals.completed)
            || (value["intendedStatus"] == "no_mutants") != (totals.completed == 0))
    {
        return Err(files::invalid("inconsistent completed progress totals"));
    }
    if !profile {
        if value["kind"] != "campaign"
            || !value["family"]
                .as_str()
                .is_some_and(|name| FAMILIES.contains(&name))
            || value["family"] == "language_specific" && value["language"] != "dart"
        {
            return Err(files::invalid("invalid progress family"));
        }
        return Ok(());
    }
    exact(&value["families"], &FAMILIES)?;
    if !value["activeFamily"].is_null() {
        return Err(files::invalid(
            "final progress cannot have an active family",
        ));
    }
    let (mut completed, mut outcomes, mut reasons, mut enabled) =
        (0u64, BTreeMap::new(), BTreeMap::new(), 0);
    let mut expected = Some(0u64);
    for family in FAMILIES {
        let row = &value["families"][family];
        if row["enabled"] == false {
            exact(row, &["enabled", "status"])?;
            if row["status"] != "disabled" {
                return Err(files::invalid("invalid disabled progress family"));
            }
            continue;
        }
        let mut keys = vec![
            "enabled",
            "status",
            "expectedMutants",
            "completedMutants",
            "outcomeCounts",
            "reasonCounts",
        ];
        if family == "language_specific" {
            keys.push("language");
        }
        exact(row, &keys)?;
        if row["enabled"] != true
            || !row["status"]
                .as_str()
                .is_some_and(|name| STATUSES.contains(&name))
            || family == "language_specific" && row["language"] != "dart"
        {
            return Err(files::invalid("invalid enabled progress family"));
        }
        enabled += 1;
        let Totals {
            completed: count,
            outcomes: family_outcomes,
            reasons: family_reasons,
        } = counts(row)?;
        let family_complete = matches!(row["status"].as_str(), Some("completed" | "no_mutants"));
        if complete && !family_complete
            || family_complete && row["expectedMutants"].as_u64() != Some(count)
            || row["status"] == "no_mutants" && count != 0
        {
            return Err(files::invalid("inconsistent completed progress family"));
        }
        expected = match (expected, row["expectedMutants"].as_u64()) {
            (Some(total), Some(additional)) => Some(
                total
                    .checked_add(additional)
                    .ok_or_else(|| files::invalid("progress counter overflow"))?,
            ),
            _ => None,
        };
        completed = completed
            .checked_add(count)
            .ok_or_else(|| files::invalid("progress counter overflow"))?;
        merge(&mut outcomes, &family_outcomes)?;
        merge(&mut reasons, &family_reasons)?;
    }
    if enabled == 0
        || totals
            != (Totals {
                completed,
                outcomes,
                reasons,
            })
        || !value["expectedMutants"].is_null() && value["expectedMutants"].as_u64() != expected
    {
        return Err(files::invalid("profile progress counters disagree"));
    }
    Ok(())
}

struct Existing {
    file: File,
    fingerprint: files::Fingerprint,
    hash: String,
}

fn existing(root: &File) -> io::Result<Option<Existing>> {
    let name = c"progress.json";
    // SAFETY: the held directory and fixed filename cannot escape the output.
    let descriptor = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: successful openat created the unique descriptor.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let bytes = file.metadata()?.len();
    let fingerprint = files::staged(&file, bytes)?;
    if !files::same(&files::stat(root, "progress.json")?, &file, libc::S_IFREG)? {
        return Err(files::invalid("progress file changed"));
    }
    let hash = digest(&file, bytes)?;
    if files::Fingerprint::from(file.metadata()?) != fingerprint {
        return Err(files::invalid("progress file changed"));
    }
    Ok(Some(Existing {
        file,
        fingerprint,
        hash,
    }))
}

fn digest(file: &File, bytes: u64) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut offset = 0;
    let mut buffer = [0u8; 64 * 1024];
    while offset < bytes {
        let length = ((bytes - offset).min(buffer.len() as u64)) as usize;
        let count = file.read_at(&mut buffer[..length], offset)?;
        if count == 0 {
            return Err(files::invalid("progress file truncated"));
        }
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    if file.read_at(&mut buffer[..1], offset)? != 0 {
        return Err(files::invalid("progress file grew"));
    }
    Ok(format!("{:x}", hash.finalize()))
}

/// Existing files are checked but never opened for mutation. Only fresh private
/// stages receive bytes, then one rename replaces the fixed public filename.
pub(crate) struct Writer {
    path: PathBuf,
    root: File,
    current: Option<Existing>,
}

impl Writer {
    pub(crate) fn new(path: &Path, root: File) -> io::Result<Self> {
        files::path_text(path)?;
        let mut result = Self {
            path: path.into(),
            root,
            current: None,
        };
        result.verify_root()?;
        result.current = existing(&result.root)?;
        Ok(result)
    }

    fn verify_root(&self) -> io::Result<()> {
        let named = files::walk(&self.path)?.metadata()?;
        let held = self.root.metadata()?;
        if !held.is_dir() || named.dev() != held.dev() || named.ino() != held.ino() {
            return Err(files::invalid("progress output directory changed"));
        }
        Ok(())
    }

    fn verify_current(&self) -> io::Result<()> {
        self.verify_root()?;
        match &self.current {
            Some(current) => {
                if !files::same(
                    &files::stat(&self.root, "progress.json")?,
                    &current.file,
                    libc::S_IFREG,
                )? || files::Fingerprint::from(current.file.metadata()?) != current.fingerprint
                    || digest(&current.file, current.file.metadata()?.len())? != current.hash
                {
                    return Err(files::invalid("progress checkpoint changed"));
                }
            }
            None => match files::stat(&self.root, "progress.json") {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(files::invalid("unexpected progress checkpoint")),
            },
        }
        Ok(())
    }

    pub(crate) fn write(&mut self, value: &Value) -> io::Result<()> {
        self.write_with_hook(value, &mut |_| Ok(()))
    }

    pub(crate) fn write_with_hook(
        &mut self,
        value: &Value,
        hook: &mut impl FnMut(&str) -> io::Result<()>,
    ) -> io::Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        self.verify_current()?;
        let mut encoded = serde_json::to_vec(value).map_err(io::Error::other)?;
        encoded.push(b'\n');
        let bytes = encoded.len() as u64;
        let hash = format!("{:x}", Sha256::digest(&encoded));
        let token = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| files::invalid("invalid progress clock"))?
            .as_nanos();
        let name = format!(
            ".progress-{}-{token:x}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        hook("create-stage")?;
        let mut file = files::create_file(&self.root, &name)?;
        // Uncertain stages are deliberately retained on error; cleanup must
        // never unlink a name that could have been replaced by another actor.
        hook("write-stage")?;
        file.write_all(&encoded)?;
        hook("sync-file")?;
        file.sync_all()?;
        let fingerprint = files::staged(&file, bytes)?;
        if digest(&file, bytes)? != hash {
            return Err(files::invalid("staged progress bytes changed"));
        }
        hook("verify-stage")?;
        self.verify_current()?;
        if files::staged(&file, bytes)? != fingerprint
            || !files::same(&files::stat(&self.root, &name)?, &file, libc::S_IFREG)?
        {
            return Err(files::invalid("staged progress changed"));
        }
        hook("sync-root")?;
        self.root.sync_all()?;
        hook("commit")?;
        self.verify_current()?;
        if files::staged(&file, bytes)? != fingerprint
            || !files::same(&files::stat(&self.root, &name)?, &file, libc::S_IFREG)?
            || digest(&file, bytes)? != hash
        {
            return Err(files::invalid("staged progress changed"));
        }
        files::rename(&self.root, &name, "progress.json")?;
        // Rename can update ctime. Capture the committed fingerprint before the
        // durability step, permitting a fresh owner to retry after fsync fails.
        self.current = Some(Existing {
            fingerprint: files::staged(&file, bytes)?,
            file,
            hash,
        });
        hook("sync-commit")?;
        self.root.sync_all()
    }
}

#[cfg(test)]
#[path = "campaign_progress_tests.rs"]
mod tests;
