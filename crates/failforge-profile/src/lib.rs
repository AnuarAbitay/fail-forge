//! Native generation and static applicability of file-scoped mutation profiles.

pub mod analysis;
#[cfg(unix)]
pub mod output;
#[cfg(windows)]
#[path = "output_windows.rs"]
pub mod output;
pub mod preparation;
mod pubspec;

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("{0}")]
    Config(String),
    #[error("cannot read package inputs or publish the generated profile")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Analysis(String),
    #[error("profile generation cancelled")]
    Cancelled(i32),
}

impl From<failforge_config::ConfigError> for ProfileError {
    fn from(error: failforge_config::ConfigError) -> Self {
        Self::Config(error.to_string())
    }
}

impl ProfileError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) => 2,
            Self::Cancelled(signal) if *signal == libc::SIGTERM => 143,
            Self::Cancelled(_) => 130,
            Self::Io(_) | Self::Analysis(_) => 125,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GenerateOptions {
    pub source: PathBuf,
    /// Omit to mirror the source path under the package's `profiles/` directory.
    pub output: Option<PathBuf>,
    pub families: Vec<String>,
    pub language: Option<String>,
    pub operators: Vec<String>,
    pub force: bool,
    pub dart_sdk: Option<PathBuf>,
    pub flutter_sdk: Option<PathBuf>,
    pub adapter_root: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ProfileDraft {
    pub project: PathBuf,
    pub source: PathBuf,
    pub output: PathBuf,
    pub text: String,
    pub values: serde_json::Value,
    pub binding: output::OutputBinding,
    pub framework_flutter: Option<bool>,
}

impl From<failforge_workspace::InputError> for ProfileError {
    fn from(error: failforge_workspace::InputError) -> Self {
        match error {
            failforge_workspace::InputError::Io(error) => Self::Io(error),
            failforge_workspace::InputError::Invalid(message) => Self::Analysis(message),
            failforge_workspace::InputError::Cancelled(signal) => Self::Cancelled(signal),
        }
    }
}
