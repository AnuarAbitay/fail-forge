//! Configuration normalization shared by native campaign entry points.
//!
//! Loading a profile reads bounded configuration metadata only. It never runs
//! author code, installs dependencies, or probes an SDK for readiness.

pub mod catalogue;
pub mod duration;
pub mod models;
pub mod normalize;
pub mod paths;
pub mod yaml;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

pub use models::{CampaignConfig, CampaignRules, MutationSelection, ProfileConfig};
pub use normalize::{from_mapping, from_profile_mapping};

pub const MAX_YAML_BYTES: usize = 1024 * 1024;
pub const MAX_YAML_DEPTH: usize = 32;
pub const MAX_YAML_SCALAR_CHARACTERS: usize = 65_536;
pub const MAX_RULE_VALUES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Context {
    pub cwd: PathBuf,
    pub installation: PathBuf,
}

impl Context {
    pub fn new(cwd: &Path, installation: &Path) -> Result<Self, ConfigError> {
        let process_cwd = std::env::current_dir()
            .map_err(|_| ConfigError::new("cannot determine current directory"))?;
        let cwd = paths::resolve_native_path(cwd, "cwd", &process_cwd)?;
        let installation = paths::resolve_native_path(installation, "installation", &cwd)?;
        Ok(Self { cwd, installation })
    }
}

#[derive(Clone, Debug)]
pub enum RunConfig {
    Campaign(Box<CampaignConfig>),
    Profile(ProfileConfig),
}

impl RunConfig {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Campaign(value) => value.to_value(),
            Self::Profile(value) => value.to_value(),
        }
    }

    /// A transitional handoff to execution owners that have not migrated yet.
    /// Both the effective document and every selected campaign are retained.
    pub fn to_transport_value(&self) -> Value {
        match self {
            Self::Campaign(value) => json!({"kind":"campaign", "configuration":value.to_value(),
                                            "campaigns":[value.to_value()]}),
            Self::Profile(value) => json!({"kind":"profile", "configuration":value.to_value(),
                "campaigns":value.campaigns.iter().map(CampaignConfig::to_value).collect::<Vec<_>>()}),
        }
    }
}

pub fn load_yaml(path: &Path, context: &Context) -> Result<RunConfig, ConfigError> {
    let source = paths::resolve_native_path(path, "config", &context.cwd)?;
    let raw = paths::read_regular_bytes(&source, MAX_YAML_BYTES, true).map_err(|_| {
        ConfigError::new(format!(
            "cannot read configuration file: {}",
            source.display()
        ))
    })?;
    if raw.len() > MAX_YAML_BYTES {
        return Err(ConfigError::new("YAML configuration exceeds 1 MiB"));
    }
    let text = std::str::from_utf8(&raw)
        .map_err(|_| ConfigError::new("YAML configuration must be UTF-8"))?;
    let values = yaml::parse_yaml(text)?;
    let base = source
        .parent()
        .ok_or_else(|| ConfigError::new("configuration has no parent"))?;
    let project = paths::profile_project(base)?;
    from_profile_mapping(&values, &project, context, Some(base)).map(RunConfig::Profile)
}

/// Resolve either an explicit profile or the semantic flags of `mutate`.
pub fn resolve_config(args: &Value, context: &Context) -> Result<RunConfig, ConfigError> {
    let values = args
        .as_object()
        .ok_or_else(|| ConfigError::new("CLI arguments must be a mapping"))?;
    const SEMANTIC: &[&str] = &[
        "project",
        "family",
        "workers",
        "test_timeout",
        "command_timeout",
        "operators",
        "include",
        "exclude",
        "classes",
        "methods",
        "max_mutants",
        "dart_sdk",
        "flutter_sdk",
        "adapter_root",
        "output",
        "language",
    ];
    let mut unknown = values
        .keys()
        .filter(|key| {
            !SEMANTIC.contains(&key.as_str())
                && !["config", "installation", "runner"].contains(&key.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(ConfigError::new(format!(
            "CLI arguments has unknown settings: {}",
            unknown.join(", ")
        )));
    }
    if let Some(config) = values.get("config").filter(|value| !value.is_null()) {
        let mut conflicts = SEMANTIC
            .iter()
            .filter(|key| values.get(**key).is_some_and(|v| !v.is_null()))
            .copied()
            .collect::<Vec<_>>();
        conflicts.sort_unstable();
        if !conflicts.is_empty() {
            return Err(ConfigError::new(format!(
                "--config cannot be combined with semantic flags: {}",
                conflicts.join(", ")
            )));
        }
        let path = paths::resolve_path(config, "config", &context.cwd)?;
        return load_yaml(&path, context);
    }
    let mut mapping = serde_json::Map::new();
    for key in [
        "project",
        "family",
        "workers",
        "dart_sdk",
        "flutter_sdk",
        "adapter_root",
        "output",
        "language",
    ] {
        if let Some(value) = values.get(key).filter(|v| !v.is_null()) {
            mapping.insert(key.into(), value.clone());
        }
    }
    for (group, keys) in [
        (
            "timeouts",
            &[("test_timeout", "test"), ("command_timeout", "command")][..],
        ),
        (
            "rules",
            &[
                ("operators", "operators"),
                ("include", "include"),
                ("exclude", "exclude"),
                ("classes", "classes"),
                ("methods", "methods"),
            ][..],
        ),
        ("limits", &[("max_mutants", "max_mutants")][..]),
    ] {
        let mut selected = serde_json::Map::new();
        for (input, output) in keys {
            if let Some(value) = values.get(*input).filter(|v| !v.is_null()) {
                selected.insert((*output).into(), value.clone());
            }
        }
        if !selected.is_empty() {
            mapping.insert(group.into(), Value::Object(selected));
        }
    }
    from_mapping(&Value::Object(mapping), context, None)
        .map(|value| RunConfig::Campaign(Box::new(value)))
}

#[cfg(test)]
mod tests;
