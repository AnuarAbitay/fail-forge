//! Sequential, manifest-bound public entrypoint regression. This module never
//! edits historical evidence or decides mutation outcomes itself.
use crate::common::{self, Result, ensure};
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    manifest: PathBuf,
    /// Required explicit scope; no default silently omits archived cases.
    #[arg(long)]
    group: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    installation: Option<PathBuf>,
    #[arg(long)]
    binary_dir: Option<PathBuf>,
    #[arg(long)]
    dart_sdk: Option<PathBuf>,
    /// Validate the selected manifest and retained files without invoking an SDK.
    #[arg(long)]
    check_only: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    version: u32,
    cases: Vec<Case>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Case {
    name: String,
    group: String,
    project: PathBuf,
    generation: Option<Generation>,
    configuration: Value,
    profile: Option<PathBuf>,
    variants: Vec<Variant>,
    expected: Option<PathBuf>,
    historical_manifest: Option<PathBuf>,
    budget_seconds: u64,
    /// A deliberate partial campaign is separate from a complete score check.
    cancel_after_records: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Generation {
    file: PathBuf,
    families: Vec<String>,
    #[serde(default)]
    operators: Vec<String>,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Variant {
    Direct,
    Profile,
}
impl Variant {
    fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Profile => "profile",
        }
    }
}
fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}
fn keys(value: &Value, allowed: &[&str]) -> Result<()> {
    ensure(
        value
            .as_object()
            .is_some_and(|map| map.keys().all(|key| allowed.contains(&key.as_str()))),
        "configuration contains unsupported fields",
    )
}
fn read_manifest(path: &Path) -> Result<Manifest> {
    let bytes = failforge_workspace::read_regular(path, 16 * 1024 * 1024)?;
    let manifest: Manifest = serde_json::from_value(failforge_workspace::parse_json(&bytes)?)?;
    ensure(manifest.version == 1, "unsupported public manifest version")?;
    let mut names = BTreeSet::new();
    for case in &manifest.cases {
        ensure(
            !case.name.is_empty()
                && relative(Path::new(&case.name))
                && case
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
                && names.insert(&case.name),
            "duplicate or unsafe public case name",
        )?;
        ensure(
            case.project.is_absolute() && case.budget_seconds > 0 && case.budget_seconds <= 172_800,
            "case requires an absolute prepared project and bounded deadline",
        )?;
        ensure(
            case.cancel_after_records.is_none_or(|count| count > 0),
            "cancellation requires a positive completed-record barrier",
        )?;
        if let Some(generation) = &case.generation {
            ensure(
                relative(&generation.file),
                "generation source must be relative",
            )?;
            ensure(
                !generation.families.is_empty()
                    && generation.families.iter().all(|v| {
                        matches!(v.as_str(), "classic" | "failure_mode" | "language_specific")
                    }),
                "unknown generation family",
            )?;
        }
        let variants: BTreeSet<_> = case.variants.iter().map(|v| v.name()).collect();
        ensure(
            variants.len() == case.variants.len(),
            "duplicate execution variant",
        )?;
        ensure(
            case.generation.is_some() || !case.variants.is_empty(),
            "empty public case",
        )?;
        if variants.contains("direct") {
            keys(
                &case.configuration,
                &["family", "language", "rules", "timeouts", "limits"],
            )?;
            keys(
                &case.configuration["rules"],
                &["operators", "include", "exclude", "methods", "classes"],
            )?;
            keys(&case.configuration["timeouts"], &["test", "command"])?;
            keys(&case.configuration["limits"], &["max_mutants"])?;
            let family = case.configuration["family"]
                .as_str()
                .ok_or("missing family")?;
            ensure(
                (family == "language_specific" && case.configuration["language"] == "dart")
                    || (matches!(family, "classic" | "failure_mode")
                        && case.configuration["language"].is_null()),
                "family/language mismatch",
            )?;
        }
        ensure(
            case.cancel_after_records.is_some()
                || case.variants.is_empty()
                || case.expected.is_some(),
            "complete campaign requires a per-ID verdict oracle",
        )?;
    }
    Ok(manifest)
}
fn strings<'a>(value: &'a Value, key: &str) -> Result<Vec<&'a str>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| format!("{key} must contain strings").into())
            })
            .collect(),
        _ => Err(format!("{key} must be an array").into()),
    }
}
fn record_map(records: &Value) -> Result<BTreeMap<String, Value>> {
    let mut result = BTreeMap::new();
    for record in records.as_array().ok_or("missing records")? {
        let id = record["mutantId"].as_str().ok_or("missing mutant ID")?;
        ensure(
            result.insert(id.to_owned(), record.clone()).is_none(),
            "duplicate mutant ID",
        )?;
    }
    Ok(result)
}
fn differences(actual: &Value, expected: &Value) -> Result<Vec<Value>> {
    let actual = record_map(actual)?;
    let expected = record_map(expected)?;
    let mut differences = Vec::new();
    for id in actual
        .keys()
        .chain(expected.keys())
        .collect::<BTreeSet<_>>()
    {
        match (actual.get(id), expected.get(id)) {
            (Some(a), Some(e)) => {
                for field in ["outcome", "operatorId", "file", "edit", "family"] {
                    if !e[field].is_null() && a[field] != e[field] {
                        differences.push(json!({"mutantId":id,"field":field,"expected":e[field],"actual":a[field]}));
                    }
                }
            }
            (Some(_), None) => differences.push(json!({"mutantId":id,"kind":"added"})),
            (None, Some(_)) => differences.push(json!({"mutantId":id,"kind":"missing"})),
            _ => unreachable!(),
        }
    }
    Ok(differences)
}
fn candidate_membership(records: &Value, candidates: &[Value], complete: bool) -> Result<()> {
    let records = record_map(records)?;
    let mut candidates_by_id = BTreeMap::new();
    for candidate in candidates {
        let mutant = &candidate["mutant"];
        let id = mutant["id"].as_str().ok_or("missing candidate ID")?;
        ensure(
            candidates_by_id.insert(id, mutant).is_none(),
            "duplicate fresh candidate ID",
        )?;
    }
    for (id, record) in &records {
        let candidate = candidates_by_id
            .get(id.as_str())
            .ok_or("record is not a freshly discovered candidate")?;
        for field in ["operatorId", "file", "edit", "family"] {
            ensure(
                record[field] == candidate[field],
                format!("record {field} differs from candidate"),
            )?;
        }
    }
    ensure(
        !complete || candidates_by_id.len() == records.len(),
        "complete report omits discovered candidates",
    )
}
fn cancellation_barrier_contract(summary: &Value, receipt: &Value) -> Result<()> {
    let observed = receipt["completedAtCancellationBarrier"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or("missing positive cancellation barrier")?;
    ensure(
        summary["completedMutants"]
            .as_u64()
            .is_some_and(|n| n >= observed),
        "cancelled summary lost records already observed at the barrier",
    )
}
fn attempt_clean(attempt: &Value) -> Result<()> {
    ensure(
        attempt["temporaryCopyCleaned"] == true && attempt["runner"]["cleanup"] == "complete",
        "executed attempt lacks confirmed copy/process cleanup",
    )
}
fn family_contract(summary: &Value, cancelled: bool) -> Result<()> {
    let records = summary["records"]
        .as_array()
        .ok_or("missing family records")?;
    let mut counts = BTreeMap::<String, u64>::new();
    for record in records {
        *counts
            .entry(
                record["outcome"]
                    .as_str()
                    .ok_or("missing outcome")?
                    .to_owned(),
            )
            .or_default() += 1;
    }
    ensure(
        summary["completedMutants"].as_u64() == Some(records.len() as u64)
            && summary["outcomeCounts"] == serde_json::to_value(counts)?,
        "family counts disagree with records",
    )?;
    score_contract(summary)?;
    for record in records {
        if record["reason"] == "static_analysis" {
            ensure(
                record["outcome"] == "INVALID",
                "static rejection mislabeled",
            )?;
        } else {
            attempt_clean(record)?;
        }
    }
    if records.is_empty() && summary["status"] == "no_mutants" {
        return Ok(());
    }
    ensure(
        summary["baseline"]["outcome"] == "SURVIVED",
        "unhealthy baseline",
    )?;
    attempt_clean(&summary["baseline"])?;
    if !cancelled {
        let controls = summary["originalControls"]
            .as_array()
            .ok_or("missing controls")?;
        ensure(!controls.is_empty(), "missing final original control")?;
        for control in controls {
            ensure(
                control["outcome"] == "SURVIVED",
                "unhealthy original control",
            )?;
            attempt_clean(control)?;
        }
    }
    Ok(())
}
fn score_contract(summary: &Value) -> Result<()> {
    let complete = summary["complete"]
        .as_bool()
        .ok_or("score evidence lacks a completion flag")?;
    if summary["kind"] == "profile" {
        for field in ["mutationScore", "failureModeScore", "languageSpecificScore"] {
            ensure(
                summary.get(field) == Some(&Value::Null),
                "profile aggregate must not publish a combined score",
            )?;
        }
        return Ok(());
    }
    let profile_complete = summary
        .get("profileComplete")
        .map(|value| {
            value
                .as_bool()
                .ok_or("invalid enclosing profile completion")
        })
        .transpose()?
        .unwrap_or(true);
    let scored = complete && profile_complete;
    for (family, field) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        let selected = summary["family"] == family;
        if scored {
            ensure(
                summary[field].is_object() == selected,
                "complete selected family lacks its score or disabled family exposes one",
            )?;
        } else {
            ensure(
                summary[field].is_null(),
                "incomplete campaign or enclosing profile exposes a score",
            )?;
        }
        if summary[field].is_null() {
            continue;
        }
        let mut killed = 0u64;
        let mut survived = 0u64;
        let mut excluded = BTreeMap::<String, u64>::new();
        for record in summary["records"]
            .as_array()
            .ok_or("missing score records")?
        {
            if record["family"] != family {
                continue;
            }
            match record["outcome"].as_str().ok_or("missing outcome")? {
                "KILLED" => killed += 1,
                "SURVIVED" => survived += 1,
                outcome => *excluded.entry(outcome.to_owned()).or_default() += 1,
            }
        }
        let score = &summary[field];
        ensure(
            score["family"] == family
                && score["killed"] == killed
                && score["survived"] == survived
                && score["denominator"] == killed + survived
                && score["excluded"] == serde_json::to_value(excluded)?,
            "score counts disagree with raw verdicts",
        )?;
        let denominator = killed + survived;
        ensure(
            if denominator == 0 {
                score["percent"].is_null()
            } else {
                score["percent"].as_f64().is_some_and(|actual| {
                    (actual - 100.0 * killed as f64 / denominator as f64).abs() < 0.000_001
                })
            },
            "score percentage disagrees with raw verdicts",
        )?;
    }
    Ok(())
}
fn report_contract(summary: &Value, stdout: &[u8], cancelled: bool) -> Result<()> {
    crate::regression::summary_contract(summary, stdout)?;
    ensure(
        summary.get("profileComplete").is_none(),
        "top-level report must not override its own completion",
    )?;
    ensure(
        summary["complete"] == !cancelled && (!cancelled || summary["status"] == "cancelled"),
        "unexpected campaign completion",
    )?;
    record_map(&summary["records"])?;
    score_contract(summary)?;
    if summary["kind"] == "profile" {
        let mut family_records = Vec::new();
        for family in summary["families"]
            .as_object()
            .ok_or("missing families")?
            .values()
        {
            if family["enabled"] == true && family["status"] != "disabled" {
                ensure(
                    family["summary"].is_object(),
                    "selected family lacks its summary and score evidence",
                )?;
            }
            if family["summary"].is_object()
                && !matches!(family["status"].as_str(), Some("disabled" | "not_started"))
            {
                ensure(
                    family["summary"]["profileComplete"] == summary["complete"],
                    "family enclosing-profile completion disagrees with parent",
                )?;
                family_contract(&family["summary"], cancelled)?;
                family_records.extend(
                    family["summary"]["records"]
                        .as_array()
                        .ok_or("missing nested records")?
                        .iter()
                        .cloned(),
                );
            }
        }
        ensure(
            differences(&summary["records"], &json!(family_records))?.is_empty(),
            "profile records differ from nested family records",
        )?;
    } else {
        family_contract(summary, cancelled)?;
    }
    Ok(())
}
fn generated_contract(
    profile: &Value,
    generation: &Generation,
    subject: &Path,
    installation: &Path,
) -> Result<()> {
    let context = failforge_config::Context::new(subject, installation)?;
    failforge_config::from_profile_mapping(
        profile,
        subject,
        &context,
        Some(&subject.join("profiles")),
    )?;
    let expected = generation
        .file
        .components()
        .map(|part| part.as_os_str().to_str().ok_or("non-UTF-8 source"))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("/");
    ensure(
        profile["rules"]["include"] == json!([expected]),
        "generated profile targets another source",
    )
}
struct Harness {
    root: PathBuf,
    binary: PathBuf,
    sdk: PathBuf,
    output: PathBuf,
    temp_root: PathBuf,
}
impl Harness {
    fn command(&self) -> std::process::Command {
        let mut command = common::command(&self.binary);
        command
            .current_dir(&self.root)
            .env("PUB_CACHE", self.root.join(".toolchain/pub-cache"))
            .env("XDG_CONFIG_HOME", self.output.join("tool-state"));
        command
            .env("TMPDIR", &self.temp_root)
            .env("TMP", &self.temp_root)
            .env("TEMP", &self.temp_root);
        command
    }
    fn registry_clean(&self) -> Result<()> {
        #[cfg(unix)]
        let names = {
            let account = common::account_id()?;
            [
                format!("failforge-workspaces-{account}"),
                format!("failforge-profile-workspaces-{account}"),
            ]
        };
        #[cfg(windows)]
        let names = [
            failforge_workspace::windows_fs::campaign_registry_name()?,
            failforge_workspace::windows_fs::profile_registry_name()?,
        ];
        for name in names {
            let registry = self.temp_root.join(name);
            if registry.exists() {
                for entry in fs::read_dir(registry)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    ensure(
                        name == ".global.lock" || name == ".owner",
                        "workspace registry retains active or pending campaign",
                    )?;
                }
            }
        }
        Ok(())
    }
    fn selected_command(
        &self,
        case: &Case,
        variant: Variant,
        subject: &Path,
        report: &Path,
    ) -> Result<std::process::Command> {
        let mut command = self.command();
        command.arg("mutate").arg("--installation").arg(&self.root);
        match variant {
            Variant::Profile => {
                let profile = case
                    .profile
                    .as_ref()
                    .ok_or("profile variant requires a profile")?;
                let mut config = failforge_config::yaml::parse_yaml(&fs::read_to_string(profile)?)?;
                if case.generation.is_some() {
                    let generated = report
                        .parent()
                        .ok_or("report lacks parent")?
                        .join("generation-subject/profiles/current-public-regression.yml");
                    let fresh =
                        failforge_config::yaml::parse_yaml(&fs::read_to_string(generated)?)?;
                    config["mutations"] = fresh["mutations"].clone();
                    config["rules"]["include"] = fresh["rules"]["include"].clone();
                }
                config["workers"] = 1.into();
                config["output"] = serde_json::to_value(report)?;
                config["dart_sdk"] = serde_json::to_value(&self.sdk)?;
                config["adapter_root"] = serde_json::to_value(self.root.join("adapters/dart"))?;
                config
                    .as_object_mut()
                    .ok_or("profile must be a mapping")?
                    .remove("flutter_sdk");
                let path = subject.join("profiles/retained-public-regression.json");
                ensure(!path.exists(), "reserved public profile already exists")?;
                common::write_json(&path, &config)?;
                command.arg("--config").arg(path);
            }
            Variant::Direct => {
                let config = &case.configuration;
                let family = config["family"].as_str().ok_or("missing direct family")?;
                ensure(
                    matches!(family, "classic" | "failure_mode" | "language_specific"),
                    "invalid direct family",
                )?;
                command
                    .arg("--project")
                    .arg(subject)
                    .args(["--family", family, "--workers", "1"])
                    .arg("--dart-sdk")
                    .arg(&self.sdk)
                    .arg("--adapter-root")
                    .arg(self.root.join("adapters/dart"))
                    .arg("--output")
                    .arg(report);
                if family == "language_specific" {
                    command.args(["--language", "dart"]);
                }
                for (field, flag) in [
                    ("operators", "--operator"),
                    ("include", "--include"),
                    ("exclude", "--exclude"),
                    ("methods", "--method"),
                    ("classes", "--class"),
                ] {
                    for value in strings(&config["rules"], field)? {
                        command.args([flag, value]);
                    }
                }
                for (field, flag) in [("test", "--test-timeout"), ("command", "--command-timeout")]
                {
                    command.arg(flag).arg(
                        config["timeouts"][field]
                            .as_str()
                            .ok_or("missing duration")?,
                    );
                }
                let limit = config["limits"]["max_mutants"]
                    .as_u64()
                    .ok_or("missing mutant cap")?;
                ensure(limit <= 10_000, "unsupported mutant cap")?;
                command.arg("--max-mutants").arg(limit.to_string());
            }
        }
        Ok(command)
    }
    fn discovery_contract(
        &self,
        subject: &Path,
        report: &Path,
        summary: &Value,
        expected: Option<&Value>,
        cancelled: bool,
    ) -> Result<()> {
        let families: Vec<_> = if summary["kind"] == "profile" {
            summary["families"]
                .as_object()
                .ok_or("missing families")?
                .iter()
                .filter(|(_, item)| item["enabled"] == true && item["status"] != "disabled")
                .map(|(name, item)| (name.as_str(), &item["summary"]))
                .collect()
        } else {
            vec![(summary["family"].as_str().ok_or("missing family")?, summary)]
        };
        let mut discovered = BTreeSet::new();
        for (family, family_summary) in families {
            let directory = if summary["kind"] == "profile" {
                report.join("families").join(family)
            } else {
                report.to_owned()
            };
            let transcript = directory.join("discovery/adapter-session.jsonl");
            if !transcript.exists() && cancelled && family_summary.is_null() {
                continue;
            }
            let bytes = failforge_workspace::read_regular(&transcript, 512 * 1024 * 1024)?;
            let loaded =
                failforge_dart::discovery::load_retained_transcript(&bytes, subject, family)?;
            let ids: BTreeSet<_> = loaded
                .candidates
                .iter()
                .map(|c| {
                    c["mutant"]["id"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("candidate lacks ID")
                })
                .collect::<std::result::Result<_, _>>()?;
            let empty_records = Value::Array(Vec::new());
            candidate_membership(
                if family_summary.is_object() {
                    &family_summary["records"]
                } else {
                    &empty_records
                },
                &loaded.candidates,
                !cancelled,
            )?;
            for id in ids {
                ensure(discovered.insert(id), "duplicate candidate across families")?;
            }
        }
        if let Some(ids) = expected.and_then(|e| e["discoveryIds"].as_array()) {
            let expected: BTreeSet<_> = ids
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or("invalid discovery ID"))
                .collect::<std::result::Result<_, _>>()?;
            ensure(discovered == expected, "retained discovery ID set differs")?;
        }
        Ok(())
    }
    fn generate(&self, case: &Case, subject: &Path, directory: &Path) -> Result<Value> {
        let generation = case
            .generation
            .as_ref()
            .ok_or("missing generation request")?;
        let profile = subject.join("profiles/current-public-regression.yml");
        ensure(
            !profile.exists(),
            "reserved generated profile already exists",
        )?;
        let mut command = self.command();
        command
            .args(["profile", "generate"])
            .arg("--installation")
            .arg(&self.root)
            .arg("--file")
            .arg(subject.join(&generation.file))
            .arg("--output")
            .arg(&profile)
            .arg("--dart-sdk")
            .arg(&self.sdk);
        for family in &generation.families {
            command.args(["--family", family]);
        }
        if generation.families.iter().any(|f| f == "language_specific") {
            command.args(["--language", "dart"]);
        }
        for operator in &generation.operators {
            command.args(["--operator", operator]);
        }
        let receipt = common::execute(
            &mut command,
            &directory.join("generation-command"),
            Duration::from_secs(case.budget_seconds),
            0,
        )?;
        let current = failforge_config::yaml::parse_yaml(&fs::read_to_string(profile)?)?;
        generated_contract(&current, generation, subject, &self.root)?;
        if let Some(historical) = &case.profile {
            let historical = failforge_config::yaml::parse_yaml(&fs::read_to_string(historical)?)?;
            ensure(
                current["mutations"] == historical["mutations"],
                "generated applicability differs from historical profile",
            )?;
        }
        Ok(receipt)
    }
    fn execute_cancelled(
        &self,
        command: &mut std::process::Command,
        logs: &Path,
        report: &Path,
        case: &Case,
    ) -> Result<Value> {
        fs::create_dir(logs)?;
        command
            .stdout(File::create(logs.join("stdout.log"))?)
            .stderr(File::create(logs.join("stderr.log"))?);
        let argv: Vec<_> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let started = Instant::now();
        let mut barrier_count = 0;
        let mut child = common::OwnedChild::spawn(command)?;
        let result: Result<i32> = (|| {
            loop {
                common::check_interrupt()?;
                ensure(
                    started.elapsed() < Duration::from_secs(case.budget_seconds),
                    "cancellation record barrier deadline",
                )?;
                ensure(
                    child.0.try_wait()?.is_none(),
                    "campaign exited before cancellation barrier",
                )?;
                let progress = report.join("progress.json");
                if progress.is_file() {
                    let progress = common::read_json(&progress)?;
                    if progress["completedMutants"]
                        .as_u64()
                        .is_some_and(|n| n >= case.cancel_after_records.unwrap())
                    {
                        barrier_count = progress["completedMutants"].as_u64().unwrap();
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            child.signal(libc::SIGINT)?;
            child.wait(Duration::from_secs(120))
        })();
        // On barrier failure or an interrupt, explicitly drain before writing
        // the receipt; Drop remains a last-resort bounded ownership guard.
        let result = match result {
            Ok(code) => Ok(code),
            Err(error) => {
                let _ = child.signal(libc::SIGINT);
                let _ = child.wait(Duration::from_secs(120));
                Err(error)
            }
        };
        let receipt = json!({"command":argv,"exitCode":result.as_ref().ok(),"error":result.as_ref().err().map(ToString::to_string),"wallSeconds":started.elapsed().as_secs_f64(),"cleanupUnconfirmed":!common::cleanup_confirmed(),"ownerPid":child.0.id(),"completedAtCancellationBarrier":barrier_count});
        common::write_json(&logs.join("command.json"), &receipt)?;
        ensure(result? == 130, "cancelled public campaign did not exit 130")?;
        Ok(receipt)
    }
    fn case(&self, case: &Case) -> Result<Value> {
        common::check_interrupt()?;
        self.registry_clean()?;
        let directory = self.output.join(&case.name);
        fs::create_dir(&directory)?;
        #[cfg(unix)]
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let cancel = failforge_workspace::Cancellation::default();
        let before = failforge_workspace::source_hashes(&case.project, &cancel)?;
        common::write_json(
            &directory.join("inputs-before.json"),
            &serde_json::to_value(&before)?,
        )?;
        let mut results = Vec::new();
        let mut errors = Vec::new();
        if case.generation.is_some() {
            let subject = failforge_workspace::copy_project(
                &case.project,
                &directory.join("generation-subject"),
                &cancel,
            )?;
            let copied_before = failforge_workspace::source_hashes(&subject.root, &cancel)?;
            match self.generate(case, &subject.root, &directory) {
                Ok(receipt) => {
                    results.push(json!({"variant":"generation","passed":true,"receipt":receipt}))
                }
                Err(error) => errors.push(format!("generation: {error}")),
            }
            let mut copied_after = failforge_workspace::source_hashes(&subject.root, &cancel)?;
            copied_after.remove(Path::new("profiles/current-public-regression.yml"));
            if copied_before != copied_after {
                errors.push("generation changed copied source inputs".to_owned());
            }
            if let Err(error) = self.registry_clean() {
                errors.push(format!("generation cleanup: {error}"));
            }
        }
        let expected = case
            .expected
            .as_ref()
            .map(|p| common::read_json(p))
            .transpose()?;
        for &variant in &case.variants {
            common::check_interrupt()?;
            if !common::cleanup_confirmed() || self.registry_clean().is_err() {
                errors.push("remaining variants not started after unconfirmed cleanup".to_owned());
                break;
            }
            let name = variant.name();
            let result: Result<Value> = (|| {
                let subject = failforge_workspace::copy_project(
                    &case.project,
                    &directory.join(format!("{name}-subject")),
                    &cancel,
                )?;
                let report = directory.join(format!("{name}-report"));
                let logs = directory.join(format!("{name}-command"));
                let mut command = self.selected_command(case, variant, &subject.root, &report)?;
                let copied_before = failforge_workspace::source_hashes(&subject.root, &cancel)?;
                let receipt = if case.cancel_after_records.is_some() {
                    self.execute_cancelled(&mut command, &logs, &report, case)?
                } else {
                    common::execute(
                        &mut command,
                        &logs,
                        Duration::from_secs(case.budget_seconds),
                        0,
                    )?
                };
                ensure(
                    failforge_workspace::source_hashes(&subject.root, &cancel)? == copied_before,
                    "campaign changed copied subject inputs",
                )?;
                let summary = common::read_json(&report.join("summary.json"))?;
                report_contract(
                    &summary,
                    &fs::read(logs.join("stdout.log"))?,
                    case.cancel_after_records.is_some(),
                )?;
                if case.cancel_after_records.is_some() {
                    cancellation_barrier_contract(&summary, &receipt)?;
                }
                self.discovery_contract(
                    &subject.root,
                    &report,
                    &summary,
                    expected.as_ref(),
                    case.cancel_after_records.is_some(),
                )?;
                self.registry_clean()?;
                let diffs = if case.cancel_after_records.is_none() {
                    differences(
                        &summary["records"],
                        &expected.as_ref().ok_or("missing oracle")?["records"],
                    )?
                } else {
                    Vec::new()
                };
                common::write_json(
                    &directory.join(format!("{name}-differences.json")),
                    &json!(diffs),
                )?;
                Ok(
                    json!({"variant":name,"passed":diffs.is_empty(),"receipt":receipt,"summary":report.join("summary.json"),"completedMutants":summary["completedMutants"],"outcomeCounts":summary["outcomeCounts"],"differences":diffs.len(),"partialCancellationProbe":case.cancel_after_records.is_some(),"historicalWallSeconds":expected.as_ref().map(|e|&e["historicalWallSeconds"]),"scores":{"mutationScore":summary["mutationScore"],"failureModeScore":summary["failureModeScore"],"languageSpecificScore":summary["languageSpecificScore"]}}),
                )
            })();
            match result {
                Ok(result) => {
                    if result["passed"] != true {
                        errors.push(format!("{name}: mutant IDs or verdicts differ"));
                    }
                    results.push(result);
                }
                Err(error) => errors.push(format!("{name}: {error}")),
            }
            if !common::cleanup_confirmed() {
                break;
            }
        }
        let after = failforge_workspace::source_hashes(&case.project, &cancel)?;
        common::write_json(
            &directory.join("inputs-after.json"),
            &serde_json::to_value(&after)?,
        )?;
        if before != after {
            errors.push("original source inputs changed".to_owned());
        }
        let result = json!({"name":case.name,"group":case.group,"passed":errors.is_empty(),"errors":errors,"results":results,"originalUnchanged":before==after,"historicalManifest":case.historical_manifest,"partialCancellationProbe":case.cancel_after_records.is_some()});
        common::write_json(&directory.join("result.json"), &result)?;
        Ok(result)
    }
}
fn binary_verification(actual: Result<String>, expected: &str) -> (Option<bool>, Option<String>) {
    match actual {
        Ok(hash) => (Some(hash == expected), None),
        Err(error) => (None, Some(error.to_string())),
    }
}
pub fn run(options: Options) -> Result<()> {
    let manifest = read_manifest(&options.manifest)?;
    let cases: Vec<_> = manifest
        .cases
        .iter()
        .filter(|c| c.group == options.group)
        .collect();
    ensure(
        !cases.is_empty(),
        "selected public regression group has no cases",
    )?;
    if options.check_only {
        for case in &cases {
            ensure(
                case.project.is_dir(),
                format!("missing prepared project: {}", case.project.display()),
            )?;
            for path in [&case.profile, &case.expected, &case.historical_manifest]
                .into_iter()
                .flatten()
            {
                ensure(
                    path.is_file(),
                    format!("missing retained input: {}", path.display()),
                )?;
            }
            if let Some(expected) = &case.expected {
                let expected = common::read_json(expected)?;
                record_map(&expected["records"])?;
            }
            if let Some(profile) = &case.profile {
                failforge_config::yaml::parse_yaml(&fs::read_to_string(profile)?)?;
            }
        }
        println!(
            "validated {} public cases in group {}; no SDK commands executed",
            cases.len(),
            options.group
        );
        return Ok(());
    }
    common::create_output(&options.output)?;
    let root = options
        .installation
        .unwrap_or_else(common::root)
        .canonicalize()?;
    let binary = options
        .binary_dir
        .unwrap_or_else(|| root.join("target/release"))
        .join(failforge_config::platform::binary_name("failforge").as_ref())
        .canonicalize()?;
    let sdk = options
        .dart_sdk
        .unwrap_or_else(|| root.join(".toolchain/flutter/bin/cache/dart-sdk"))
        .canonicalize()?;
    let temporary = common::Temporary::new("ff-public")?;
    let harness = Harness {
        root,
        binary,
        sdk,
        output: options.output.canonicalize()?,
        temp_root: temporary.0.clone(),
    };
    let binary_hash = common::hash(&harness.binary)?;
    common::write_json(
        &harness.output.join("plan.json"),
        &json!({"manifest":options.manifest,"manifestSha256":common::hash(&options.manifest)?,"group":options.group,"cases":cases.iter().map(|c|&c.name).collect::<Vec<_>>(),"binary":harness.binary,"binarySha256":binary_hash,"workers":1,"sequential":true}),
    )?;
    let started = Instant::now();
    let mut results = Vec::new();
    for case in cases {
        println!("public regression {}", case.name);
        match harness.case(case) {
            Ok(result) => results.push(result),
            Err(error) => {
                results.push(json!({"name":case.name,"passed":false,"error":error.to_string()}))
            }
        }
        common::write_json(&harness.output.join("results.json"), &json!(results))?;
        if common::interrupt_signal().is_some()
            || !common::cleanup_confirmed()
            || harness.registry_clean().is_err()
        {
            break;
        }
    }
    for case in manifest
        .cases
        .iter()
        .filter(|case| case.group == options.group)
    {
        if !results.iter().any(|result| result["name"] == case.name) {
            results.push(json!({"name":case.name,"passed":false,"status":"not_started","error":"interrupted or preceding process/workspace cleanup unconfirmed"}));
        }
    }
    let (unchanged, binary_verification_error) =
        binary_verification(common::hash(&harness.binary), &binary_hash);
    let registry_clean = harness.registry_clean().is_ok();
    if !registry_clean {
        std::mem::forget(temporary);
    }
    let passed = unchanged == Some(true)
        && registry_clean
        && results.len()
            == manifest
                .cases
                .iter()
                .filter(|c| c.group == options.group)
                .count()
        && results.iter().all(|v| v["passed"] == true);
    common::write_json(
        &harness.output.join("summary.json"),
        &json!({"allPassed":passed,"group":options.group,"cases":results,"binaryUnchanged":unchanged,"binaryVerificationError":binary_verification_error,"wallSeconds":started.elapsed().as_secs_f64(),"cleanupConfirmed":common::cleanup_confirmed(),"registryClean":registry_clean,"temporaryRoot":harness.temp_root}),
    )?;
    ensure(
        passed,
        "public regression contains failures; see retained results",
    )
}

#[cfg(all(test, unix))]
#[path = "public_regression_tests.rs"]
mod tests;

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[test]
    fn generated_profile_matches_native_source_selection_and_rejects_another_file() {
        let temporary = common::Temporary::new("ff-public-generation").unwrap();
        let subject = temporary.0.join("subject");
        let source = Path::new("lib").join("nested").join("value.dart");
        fs::create_dir_all(subject.join(source.parent().unwrap())).unwrap();
        fs::write(subject.join(&source), "bool value() => true;\n").unwrap();
        fs::write(subject.join("pubspec.yaml"), "name: subject\n").unwrap();
        let path = subject.join("profiles").join("generated.yml");
        common::write_json(
            &path,
            &json!({"mutations":{"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}}},
                "rules":{"include":["lib/nested/value.dart"]},"dart_sdk":temporary.0.join("dart-sdk")}),
        )
        .unwrap();
        let mut profile =
            failforge_config::yaml::parse_yaml(&fs::read_to_string(path).unwrap()).unwrap();
        let generation = Generation {
            file: source,
            families: vec!["classic".into()],
            operators: vec!["BOOLEAN_LITERAL_FLIP".into()],
        };
        assert!(relative(&generation.file));
        generated_contract(&profile, &generation, &subject, &temporary.0).unwrap();
        profile["rules"]["include"] = json!(["lib/nested/other.dart"]);
        assert!(generated_contract(&profile, &generation, &subject, &temporary.0).is_err());
    }
}

#[cfg(test)]
#[path = "public_regression_adversarial_tests.rs"]
mod adversarial_tests;
