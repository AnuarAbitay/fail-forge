use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum, error::ErrorKind};
use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, OutputMode, TimeoutPolicy};

#[derive(Parser)]
#[command(
    name = "failforge",
    version,
    about = "Run mutation campaigns and external commands with bounded cleanup"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Run a program directly; child arguments must follow --.
    Run(RunArgs),
    /// Run a mutation campaign from a config file or explicit project options.
    Mutate(Box<MutateArgs>),
    /// Replay a retained classic discovery transcript against fresh isolated copies.
    Replay(Box<ReplayArgs>),
    /// Generate mutation profiles using static analysis or a catalogue template.
    Profile(ProfileArgs),
    /// Internal prepared-input operations for the campaign controller.
    #[command(hide = true)]
    WorkspaceInputs,
    /// Internal leased workspace lifecycle for the campaign controller.
    #[command(hide = true)]
    WorkspaceLifecycle,
    /// Internal Dart namespace planning and isolated byte rewriting.
    #[command(hide = true)]
    DartNamespace,
    /// Internal complete test attempt owned by one native process.
    #[command(hide = true)]
    TestAttempt,
    /// Internal owned adapter discovery and source-bound validation.
    #[command(hide = true)]
    AdapterDiscover,
    /// Internal complete mutant execution and causal controls.
    #[command(hide = true)]
    MutantExecute,
    /// Internal bounded family execution with streamed completions.
    #[command(hide = true)]
    FamilyExecute,
    /// Private campaign discovery and family scheduling operation.
    #[command(hide = true)]
    CampaignExecute,
    /// Private owner of preflight, snapshot, campaign execution and cleanup.
    #[command(hide = true)]
    CampaignRun,
    /// Private pure final summary and terminal rendering operation.
    #[command(hide = true)]
    CampaignReport,
    /// Private descriptor-bound atomic publication of final campaign reports.
    #[command(hide = true)]
    CampaignPublish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum MutationFamily {
    Classic,
    #[value(name = "failure_mode")]
    FailureMode,
    #[value(name = "language_specific")]
    LanguageSpecific,
}

impl MutationFamily {
    fn as_str(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::FailureMode => "failure_mode",
            Self::LanguageSpecific => "language_specific",
        }
    }
}

#[derive(Args)]
pub(crate) struct ProfileArgs {
    #[command(subcommand)]
    pub(crate) command: ProfileCommands,
}

#[derive(Subcommand)]
pub(crate) enum ProfileCommands {
    /// Generate a YAML mutation profile for one Dart source file.
    Generate(Box<ProfileGenerateArgs>),
}

#[derive(Args)]
pub(crate) struct ProfileGenerateArgs {
    /// Dart source file, relative to the invocation directory.
    #[arg(long, value_name = "PATH")]
    file: PathBuf,
    /// Enable a mutation family; repeat to include several.
    #[arg(long, value_enum, required = true, action = clap::ArgAction::Append, requires_if("language_specific", "language"))]
    family: Vec<MutationFamily>,
    /// Required for language_specific; currently only Dart is supported.
    #[arg(long, value_parser = ["dart"])]
    language: Option<String>,
    /// Restrict analysis or template selection to one operator ID; repeat as needed.
    #[arg(long, value_name = "ID", action = clap::ArgAction::Append)]
    operator: Vec<String>,
    /// Destination YAML file within the source file's Dart package.
    #[arg(long, value_name = "PATH")]
    output: PathBuf,
    /// Replace an existing profile file.
    #[arg(long)]
    force: bool,
    /// Print the generated YAML without publishing a profile file.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Generate a catalogue template without analysis, an SDK or dependencies.
    #[arg(long, conflicts_with_all = ["dart_sdk", "flutter_sdk", "adapter_root"])]
    pub(crate) template: bool,
    /// Dart SDK root for applicability analysis; conflicts with --flutter-sdk.
    #[arg(long, value_name = "PATH", conflicts_with = "flutter_sdk")]
    dart_sdk: Option<PathBuf>,
    /// Flutter SDK root for applicability analysis using its bundled Dart SDK.
    #[arg(long, value_name = "PATH")]
    flutter_sdk: Option<PathBuf>,
    /// Prepared Dart adapter directory for applicability analysis.
    #[arg(long, value_name = "PATH")]
    adapter_root: Option<PathBuf>,
    /// Installation root; defaults to the running executable's installation.
    #[arg(long, value_name = "PATH")]
    pub(crate) installation: Option<PathBuf>,
}

impl ProfileGenerateArgs {
    pub(crate) fn has_valid_language_selection(&self) -> bool {
        self.family.contains(&MutationFamily::LanguageSpecific) == self.language.is_some()
    }

    pub(crate) fn generate_options(&self) -> failforge_profile::GenerateOptions {
        failforge_profile::GenerateOptions {
            source: self.file.clone(),
            output: self.output.clone(),
            families: self
                .family
                .iter()
                .map(|family| family.as_str().to_owned())
                .collect(),
            language: self.language.clone(),
            operators: self.operator.clone(),
            force: self.force,
            dart_sdk: self.dart_sdk.clone(),
            flutter_sdk: self.flutter_sdk.clone(),
            adapter_root: self.adapter_root.clone(),
        }
    }
}

#[derive(Args)]
pub(crate) struct MutateArgs {
    /// Campaign configuration file; cannot be mixed with campaign flags.
    #[arg(long, value_name = "FILE", conflicts_with_all = ["project", "family", "language", "workers", "test_timeout", "command_timeout", "output", "dart_sdk", "flutter_sdk", "operator", "include", "exclude", "method", "class", "max_mutants", "adapter_root"])]
    config: Option<PathBuf>,
    /// Root of the project to mutate; defaults to the invocation directory.
    #[arg(long, value_name = "PATH", requires = "family")]
    project: Option<PathBuf>,
    /// Mutation family; failure_mode uses the existing adapter operator family.
    #[arg(long, value_enum, required_unless_present = "config")]
    family: Option<MutationFamily>,
    /// Language-specific subtype; Dart is the currently supported language.
    #[arg(long, value_parser = ["dart"])]
    language: Option<String>,
    /// Maximum simultaneously executing mutants.
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=4))]
    workers: Option<u8>,
    /// Dart's default per-test timeout: positive integer followed by ms, s or m.
    #[arg(long, value_name = "DURATION", value_parser = parse_campaign_timeout)]
    test_timeout: Option<String>,
    /// Runner limit for an entire command: positive integer with ms, s or m.
    #[arg(long, value_name = "DURATION", value_parser = parse_campaign_timeout)]
    command_timeout: Option<String>,
    /// New directory for campaign reports and raw captures.
    #[arg(long, value_name = "PATH")]
    output: Option<PathBuf>,
    /// Dart SDK root; mutually exclusive with --flutter-sdk.
    #[arg(long, value_name = "PATH", conflicts_with = "flutter_sdk")]
    dart_sdk: Option<PathBuf>,
    /// Flutter SDK root, whose bundled Dart SDK will be used.
    #[arg(long, value_name = "PATH")]
    flutter_sdk: Option<PathBuf>,
    /// Enable one operator ID; repeat this flag to select several.
    #[arg(long, value_name = "ID", action = clap::ArgAction::Append)]
    operator: Vec<String>,
    /// Include a project-relative path pattern; repeat as needed.
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
    include: Vec<String>,
    /// Exclude a project-relative path pattern; repeat as needed.
    #[arg(long, value_name = "PATTERN", action = clap::ArgAction::Append)]
    exclude: Vec<String>,
    /// Select one method name; repeat this flag to select several.
    #[arg(long, value_name = "NAME", action = clap::ArgAction::Append)]
    method: Vec<String>,
    /// Select one class name; repeat this flag to select several.
    #[arg(long, value_name = "NAME", action = clap::ArgAction::Append)]
    class: Vec<String>,
    /// Maximum retained candidates, from 0 to 10000.
    #[arg(long, value_parser = clap::value_parser!(u32).range(0..=10_000))]
    max_mutants: Option<u32>,
    /// Explicit prepared Dart adapter directory.
    #[arg(long, value_name = "PATH")]
    adapter_root: Option<PathBuf>,
    /// Installation root; defaults to the running executable's installation.
    #[arg(long, value_name = "PATH")]
    pub(crate) installation: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct ReplayArgs {
    /// Prepared project whose source bytes match the retained candidates.
    #[arg(long)]
    project: PathBuf,
    /// Unmodified upstream framework checkout matching the subject library.
    #[arg(long)]
    framework: PathBuf,
    /// Complete JSONL discovery and static-validation transcript.
    #[arg(long)]
    transcript: PathBuf,
    /// Retained passing baseline with capture and source provenance.
    #[arg(long)]
    baseline: PathBuf,
    /// New directory for replay reports and raw captures.
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=4))]
    workers: u8,
    #[arg(long, value_name = "DURATION", value_parser = parse_campaign_timeout)]
    test_timeout: Option<String>,
    #[arg(long, alias = "timeout", value_name = "DURATION", value_parser = parse_campaign_timeout)]
    command_timeout: Option<String>,
    #[arg(long, conflicts_with = "flutter_sdk")]
    dart_sdk: Option<PathBuf>,
    #[arg(long)]
    flutter_sdk: Option<PathBuf>,
    #[arg(long)]
    adapter_root: Option<PathBuf>,
    /// Installation root; defaults to the running executable's installation.
    #[arg(long)]
    installation: Option<PathBuf>,
}

impl ReplayArgs {
    pub(crate) fn into_campaign(
        self,
        cwd: &Path,
    ) -> (MutateArgs, crate::retained_replay::ReplayPaths) {
        let replay = crate::retained_replay::ReplayPaths {
            transcript: cwd.join(self.transcript),
            baseline: cwd.join(self.baseline),
            framework: cwd.join(self.framework),
        };
        let campaign = MutateArgs {
            config: None,
            project: Some(self.project),
            family: Some(MutationFamily::Classic),
            language: None,
            workers: Some(self.workers),
            test_timeout: self.test_timeout,
            command_timeout: self.command_timeout,
            output: Some(self.output),
            dart_sdk: self.dart_sdk,
            flutter_sdk: self.flutter_sdk,
            operator: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            method: Vec::new(),
            class: Vec::new(),
            max_mutants: Some(10_000),
            adapter_root: self.adapter_root,
            installation: self.installation,
        };
        (campaign, replay)
    }
}

impl MutateArgs {
    /// Semantic input for the native normalizer, preserving omitted options.
    pub(crate) fn configuration_values(&self) -> Result<serde_json::Value, &'static str> {
        let mut values = serde_json::Map::new();
        for (name, path) in [
            ("config", self.config.as_ref()),
            ("project", self.project.as_ref()),
            ("output", self.output.as_ref()),
            ("dart_sdk", self.dart_sdk.as_ref()),
            ("flutter_sdk", self.flutter_sdk.as_ref()),
            ("adapter_root", self.adapter_root.as_ref()),
        ] {
            if let Some(path) = path {
                let text = path
                    .to_str()
                    .ok_or("configuration paths must contain valid Unicode")?;
                values.insert(name.into(), text.into());
            }
        }
        if let Some(family) = self.family {
            values.insert("family".into(), family.as_str().into());
        }
        if let Some(language) = &self.language {
            values.insert("language".into(), language.clone().into());
        }
        if let Some(workers) = self.workers {
            values.insert("workers".into(), workers.into());
        }
        if let Some(maximum) = self.max_mutants {
            values.insert("max_mutants".into(), maximum.into());
        }
        for (name, value) in [
            ("test_timeout", self.test_timeout.as_ref()),
            ("command_timeout", self.command_timeout.as_ref()),
        ] {
            if let Some(value) = value {
                values.insert(name.into(), value.clone().into());
            }
        }
        for (name, items) in [
            ("operators", &self.operator),
            ("include", &self.include),
            ("exclude", &self.exclude),
            ("methods", &self.method),
            ("classes", &self.class),
        ] {
            if !items.is_empty() {
                values.insert(name.into(), serde_json::json!(items));
            }
        }
        Ok(serde_json::Value::Object(values))
    }
}

fn parse_campaign_timeout(text: &str) -> Result<String, String> {
    parse_timeout(text)?;
    Ok(text.to_owned())
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Execution limit: an integer followed by ms, s, or m.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    timeout: Option<Duration>,
    /// Working directory of the child, relative to this invocation.
    #[arg(long, value_name = "PATH")]
    cwd: Option<PathBuf>,
    /// Display child output as well as capturing it.
    #[arg(long)]
    stream: bool,
    /// Save raw output in a directory relative to this invocation.
    #[arg(long, value_name = "PATH")]
    capture_dir: Option<PathBuf>,
    /// Internal reporter selection; requires a private native evidence channel.
    #[arg(long, value_enum, hide = true, requires = "capture_dir")]
    pub(crate) test_reporter: Option<TestReporter>,
    /// Override a child environment variable; the last assignment wins.
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<OsString>,
    /// Remove a child environment variable.
    #[arg(long, value_name = "KEY")]
    env_remove: Vec<OsString>,
    /// Start with an empty child environment.
    #[arg(long)]
    clear_env: bool,
    /// Wait after the graceful signal before forcing termination.
    #[arg(long, value_name = "DURATION", default_value = "2s", value_parser = parse_duration)]
    grace_period: Duration,
    #[arg(last = true, required = true, num_args = 1.., value_name = "PROGRAM [ARGS...]")]
    command: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum TestReporter {
    Dart,
    Flutter,
}

impl TestReporter {
    pub(crate) fn mode(self) -> failforge_dart::ReporterMode {
        match self {
            Self::Dart => failforge_dart::ReporterMode::Dart,
            Self::Flutter => failforge_dart::ReporterMode::Flutter,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Dart => "dart",
            Self::Flutter => "flutter",
        }
    }
}

impl RunArgs {
    pub(crate) fn into_spec(self, invocation_cwd: &Path) -> Result<CommandSpec, clap::Error> {
        if !invocation_cwd.is_absolute() {
            return Err(config_error("the invocation directory must be absolute"));
        }
        let mut command = self.command.into_iter();
        let program = command
            .next()
            .ok_or_else(|| config_error("a program is required after --"))?;
        let mut env = EnvSpec {
            clear: self.clear_env,
            ..EnvSpec::default()
        };
        for assignment in self.env {
            let (key, value) = split_env_assignment(assignment)?;
            env.set.insert(key, value);
        }
        env.remove.extend(self.env_remove);

        let cwd = self
            .cwd
            .map(|path| invocation_cwd.join(path))
            .unwrap_or_else(|| invocation_cwd.to_path_buf());
        let capture_dir = self.capture_dir.map(|path| invocation_cwd.join(path));
        let spec = CommandSpec {
            program,
            args: command.collect(),
            cwd,
            env,
            timeout_policy: TimeoutPolicy {
                timeout: self.timeout,
                grace_period: self.grace_period,
                ..TimeoutPolicy::default()
            },
            capture_policy: CapturePolicy {
                mode: if self.stream {
                    OutputMode::StreamAndCapture
                } else {
                    OutputMode::Capture
                },
                capture_dir,
                ..CapturePolicy::default()
            },
        };
        spec.validate()
            .map_err(|error| config_error(error.to_string()))?;
        Ok(spec)
    }
}

fn config_error(message: impl Into<String>) -> clap::Error {
    Cli::command().error(ErrorKind::ValueValidation, message.into())
}

fn split_env_assignment(assignment: OsString) -> Result<(OsString, OsString), clap::Error> {
    let bytes = assignment.as_os_str().as_bytes();
    let separator = bytes
        .iter()
        .position(|&byte| byte == b'=')
        .ok_or_else(|| config_error("--env requires KEY=VALUE"))?;
    Ok((
        OsString::from_vec(bytes[..separator].to_vec()),
        OsString::from_vec(bytes[separator + 1..].to_vec()),
    ))
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    let (number, unit) = if let Some(number) = text.strip_suffix("ms") {
        (number, "ms")
    } else if let Some(number) = text.strip_suffix('s') {
        (number, "s")
    } else if let Some(number) = text.strip_suffix('m') {
        (number, "m")
    } else {
        return Err("duration must be an integer followed by ms, s, or m".into());
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("duration must contain an unsigned decimal integer".into());
    }
    let amount = number
        .parse::<u64>()
        .map_err(|_| "duration is too large".to_owned())?;
    match unit {
        "ms" => Ok(Duration::from_millis(amount)),
        "s" => Ok(Duration::from_secs(amount)),
        "m" => amount
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| "duration is too large".to_owned()),
        _ => unreachable!("unit selected from the three recognized suffixes"),
    }
}

fn parse_timeout(text: &str) -> Result<Duration, String> {
    let duration = parse_duration(text)?;
    if duration.is_zero() {
        return Err("timeout must be greater than zero".into());
    }
    Ok(duration)
}

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mutate_args_tests.rs"]
mod mutate_tests;

#[cfg(test)]
#[path = "profile_args_tests.rs"]
mod profile_tests;
