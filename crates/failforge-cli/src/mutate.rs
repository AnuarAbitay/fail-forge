//! Public campaign entry point: configuration, lifecycle and reports in one process.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use failforge_config::{CampaignConfig, Context, RunConfig};
use failforge_dart::namespace::NamespaceError;
use failforge_workspace::Cancellation;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};

use crate::args::MutateArgs;
use crate::campaign_display::Display;
use crate::campaign_reports::{FinalReport, Reports};
use crate::campaign_session::{self, SessionEvents};
use crate::campaign_workspace::Request;
use crate::family_execution::DirectoryGuard;

struct Plan {
    request: Request,
    configuration: Value,
    families: BTreeMap<String, Value>,
}

fn identity() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn environment(
    installation: &Path,
    values: impl IntoIterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    let mut values: BTreeMap<_, _> = values.into_iter().collect();
    for key in [
        "CI",
        "FLUTTER_SUPPRESS_ANALYTICS",
        "DART_SUPPRESS_ANALYTICS",
    ] {
        values.insert(key.into(), "true".into());
    }
    let cache = installation.join(".toolchain/pub-cache");
    if cache.is_dir() {
        values
            .entry("PUB_CACHE".into())
            .or_insert_with(|| cache.to_string_lossy().into_owned());
    }
    values
}

fn validate_profile_selection(config: &RunConfig) -> Result<(), failforge_config::ConfigError> {
    if let RunConfig::Profile(profile) = config {
        for campaign in &profile.campaigns {
            crate::campaign_workspace::literal_includes(
                &campaign.project,
                &campaign.rules.to_adapter_rules(),
            )
            .map_err(|error| failforge_config::ConfigError::new(error.to_string()))?;
        }
    }
    Ok(())
}

fn prepare(
    config: RunConfig,
    installation: &Path,
    runner: &Path,
    environment: BTreeMap<String, String>,
    run_id: String,
) -> Result<Plan, NamespaceError> {
    let (mode, configs): (&str, Vec<&CampaignConfig>) = match &config {
        RunConfig::Campaign(config) => ("direct", vec![config]),
        RunConfig::Profile(config) => ("profile", config.campaigns.iter().collect()),
    };
    let first = configs
        .first()
        .ok_or_else(|| crate::dart_namespace::invalid("campaign has no selected families"))?;
    let selections = configs.iter().map(|config| json!({
        "family":config.family,"rules":config.rules.to_adapter_rules(),"workers":config.workers,
        "config":{"commandTimeout":config.command_timeout,"testTimeout":config.test_timeout}
    })).collect::<Vec<_>>();
    let request = json!({"operation":"campaign-run","mode":mode,"runId":run_id,
        "project":first.project,"output":first.output,"registry":null,
        "runtime":{"installation":installation,"runner":runner,"dartSdk":first.dart_sdk,
            "flutterSdk":first.flutter_sdk,"adapterRoot":first.adapter_root},
        "environment":environment,"maxMutants":first.max_mutants,"families":selections});
    Ok(Plan {
        request: crate::campaign_workspace::decode(
            &serde_json::to_vec(&request)
                .map_err(|_| crate::dart_namespace::invalid("invalid campaign configuration"))?,
        )?,
        configuration: config.to_value(),
        families: configs
            .iter()
            .map(|config| (config.family.clone(), config.to_value()))
            .collect(),
    })
}

struct Reporter<'a> {
    reports: &'a mut Reports,
    display: &'a Display,
    cancel: &'a Cancellation,
    terminal: Option<(String, Option<String>)>,
    display_error: Option<String>,
    completed: BTreeMap<String, usize>,
    expected: BTreeMap<String, usize>,
}

impl Reporter<'_> {
    fn progress(&mut self, event: &Value) -> io::Result<()> {
        let mut event = event;
        if event["kind"] == "campaign_event" {
            event = &event["event"];
        }
        let family = event["family"].as_str().unwrap_or("").to_owned();
        if event["kind"] == "family_event" {
            event = &event["event"];
        }
        if event["kind"] == "discovery" {
            self.expected.insert(
                family.clone(),
                event["value"]["candidates"].as_array().map_or(0, Vec::len),
            );
        }
        let scope = if family.is_empty() {
            String::new()
        } else {
            format!("{family}: ")
        };
        let text = match event["kind"].as_str() {
            Some("stage_started") => Some(format!(
                "failforge: {scope}{}\n",
                event["stage"].as_str().unwrap_or("unknown")
            )),
            Some("mutant") => {
                let count = self.completed.entry(family.clone()).or_default();
                *count += 1;
                Some(format!(
                    "failforge: {scope}{}/{} {}\n",
                    count,
                    self.expected.get(&family).copied().unwrap_or(0),
                    event["record"]["outcome"].as_str().unwrap_or("UNKNOWN")
                ))
            }
            _ => None,
        };
        self.display.check()?;
        if let Some(text) = text {
            self.display.progress(text)?;
        }
        Ok(())
    }
}

impl SessionEvents for Reporter<'_> {
    fn bind_output(&mut self, directory: &DirectoryGuard) -> io::Result<()> {
        self.reports
            .bind_output(directory.descriptor().map_err(io::Error::other)?)
    }

    fn emit_session(&mut self, event: Value) -> io::Result<()> {
        if self.reports.event(&event).is_err() || self.reports.failure().is_some() {
            self.cancel.request(libc::SIGINT);
        }
        if self.display_error.is_none() {
            if let Err(error) = self.progress(&event) {
                self.display_error = Some(error.to_string());
                self.cancel.request(libc::SIGINT);
            }
        }
        // Reporting failures never suppress already-sealed worker results.
        Ok(())
    }

    fn finish_session(&mut self, status: &str, error: Option<&str>) -> io::Result<()> {
        self.terminal = Some((status.into(), error.map(str::to_owned)));
        Ok(())
    }
}

fn exit_status(summary: &Value, signal: Option<i32>) -> u8 {
    if summary["status"] == "cancelled" {
        return if signal == Some(libc::SIGTERM) {
            143
        } else {
            130
        };
    }
    if summary["complete"] != true
        || summary["records"].as_array().is_none_or(|records| {
            records.iter().any(|record| {
                matches!(
                    record["outcome"].as_str(),
                    Some("INFRA_ERROR" | "UNCLASSIFIED" | "NO_TESTS")
                )
            })
        })
    {
        return 125;
    }
    0
}

async fn run(
    plan: Plan,
    display: &Display,
    parent: File,
) -> Result<(FinalReport, Option<i32>), NamespaceError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancel = Cancellation::default();
    crate::campaign_controller::watch_parent(parent, cancel.clone())?;
    let listening = cancel.clone();
    let listener = tokio::spawn(async move {
        listening.request(tokio::select! { _ = interrupt.recv() => libc::SIGINT, _ = terminate.recv() => libc::SIGTERM });
    });
    let result = async {
        let mut reports = Reports::new(
            plan.request.mode.as_str(),
            plan.configuration,
            plan.families,
        )?;
        let mut sink = Reporter {
            reports: &mut reports,
            display,
            cancel: &cancel,
            terminal: None,
            display_error: None,
            completed: BTreeMap::new(),
            expected: BTreeMap::new(),
        };
        let result = campaign_session::run_owned(Arc::new(plan.request), &cancel, &mut sink).await;
        let (mut status, mut error) = match result {
            Ok(()) => sink
                .terminal
                .take()
                .ok_or_else(|| crate::dart_namespace::invalid("campaign completion is missing"))?,
            Err(error) if !sink.reports.prepared() => return Err(error),
            Err(NamespaceError::Cancelled(_)) => ("cancelled".into(), None),
            Err(error) => ("failed".into(), Some(error.to_string())),
        };
        let failure = sink.display_error.take().or_else(|| {
            display
                .flush(Duration::from_secs(2))
                .err()
                .map(|error| error.to_string())
        });
        if status != "failed" {
            if let Some(failure) = failure {
                status = "failed".into();
                error = Some(failure);
            }
        }
        let report = sink.reports.finish(&status, error.as_deref())?;
        Ok((report, cancel.signum()))
    }
    .await;
    listener.abort();
    let _ = listener.await;
    result
}

fn diagnostic(message: &str) {
    if let Ok(display) = Display::stdio() {
        let _ = display.text(true, format!("failforge: {message}\n"));
        let _ = display.flush(Duration::from_secs(2));
    }
}

pub(crate) fn execute(arguments: MutateArgs, invocation_cwd: &Path) -> ExitCode {
    execute_selected(arguments, invocation_cwd, None)
}

pub(crate) fn execute_replay(
    arguments: MutateArgs,
    invocation_cwd: &Path,
    replay: crate::retained_replay::ReplayPaths,
) -> ExitCode {
    execute_selected(arguments, invocation_cwd, Some(replay))
}

fn execute_selected(
    arguments: MutateArgs,
    invocation_cwd: &Path,
    replay: Option<crate::retained_replay::ReplayPaths>,
) -> ExitCode {
    let Some(channel) = std::env::var_os(crate::campaign_controller::PARENT_ENV) else {
        return crate::campaign_controller::execute();
    };
    let parent = match crate::campaign_controller::inherited_parent(&channel) {
        Ok(parent) => parent,
        Err(error) => {
            diagnostic(&error.to_string());
            return ExitCode::from(125);
        }
    };
    let installation = match crate::installation::resolve(arguments.installation.as_deref()) {
        Ok(installation) => installation,
        Err(error) => {
            diagnostic(&error.to_string());
            return ExitCode::from(125);
        }
    };
    let context = match Context::new(invocation_cwd, &installation) {
        Ok(context) => context.with_sdk_search_path(std::env::var_os("PATH")),
        Err(error) => {
            diagnostic(&error.to_string());
            return ExitCode::from(2);
        }
    };
    let config = arguments
        .configuration_values()
        .map_err(failforge_config::ConfigError::new)
        .and_then(|values| failforge_config::resolve_config(&values, &context))
        .and_then(|config| {
            validate_profile_selection(&config)?;
            Ok(config)
        });
    let config = match config {
        Ok(config) => config,
        Err(error) => {
            diagnostic(&error.to_string());
            return ExitCode::from(2);
        }
    };
    let result = (|| -> Result<u8, NamespaceError> {
        if !context.installation.is_dir() {
            return Err(crate::dart_namespace::invalid(
                "campaign installation is unavailable",
            ));
        }
        let runner = std::env::current_exe()?.canonicalize()?;
        let variables = std::env::vars_os()
            .filter(|(key, _)| key != crate::campaign_controller::PARENT_ENV)
            .map(|(key, value)| {
                key.into_string()
                    .ok()
                    .zip(value.into_string().ok())
                    .ok_or_else(|| {
                        crate::dart_namespace::invalid(
                            "campaign environment must contain valid Unicode",
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut plan = prepare(
            config,
            &context.installation,
            &runner,
            environment(&context.installation, variables),
            identity()?,
        )?;
        if let Some(replay) = replay {
            let metadata = serde_json::to_value(&replay).map_err(io::Error::other)?;
            plan.configuration["retainedReplay"] = metadata.clone();
            for configuration in plan.families.values_mut() {
                configuration["retainedReplay"] = metadata.clone();
            }
            plan.request.replay = Some(replay);
        }
        let display = Display::stdio()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (report, signal) = runtime.block_on(run(plan, &display, parent))?;
        let status = exit_status(&report.summary, signal);
        display.text(
            false,
            format!(
                "{}\nReport: {}/summary.json\n",
                report.terminal,
                report.summary["output"].as_str().unwrap_or("")
            ),
        )?;
        if status == 125 {
            if let Some(error) = report.summary["error"].as_str() {
                display.text(true, format!("failforge: {error}\n"))?;
            }
        }
        display.flush(Duration::from_secs(2))?;
        Ok(status)
    })();
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            diagnostic(&error.to_string());
            ExitCode::from(match error {
                NamespaceError::Cancelled(libc::SIGTERM) => 143,
                NamespaceError::Cancelled(_) => 130,
                _ => 125,
            })
        }
    }
}

#[cfg(test)]
#[path = "mutate_tests.rs"]
mod tests;
