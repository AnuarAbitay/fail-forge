use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;

pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: EnvSpec,
    pub timeout_policy: TimeoutPolicy,
    pub capture_policy: CapturePolicy,
}

pub struct TimeoutPolicy {
    pub timeout: Option<Duration>,
    pub grace_period: Duration,
    pub forced_wait: Duration,
    pub output_drain: Duration,
}

#[derive(Default)]
pub struct EnvSpec {
    pub clear: bool,
    pub set: BTreeMap<OsString, OsString>,
    pub remove: BTreeSet<OsString>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpecError {
    EmptyProgram,
    ProgramContainsNul,
    ArgumentContainsNul { index: usize },
    CwdContainsNul,
    ZeroTimeout,
    EmptyEnvKey,
    EnvKeyContainsNul,
    EnvKeyContainsEquals,
    EnvValueContainsNul,
    EnvKeyConflict,
    ClearEnvRequiresPath,
    ZeroReadChunk,
    CaptureDirContainsNul,
}

impl CommandSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.program.is_empty() {
            return Err(SpecError::EmptyProgram);
        }

        if contains_nul(self.program.as_os_str()) {
            return Err(SpecError::ProgramContainsNul);
        }

        if let Some(index) = self
            .args
            .iter()
            .position(|arg| contains_nul(arg.as_os_str()))
        {
            return Err(SpecError::ArgumentContainsNul { index });
        }

        if contains_nul(self.cwd.as_os_str()) {
            return Err(SpecError::CwdContainsNul);
        }

        self.env.validate()?;

        #[cfg(unix)]
        let has_program_path = self.program.as_encoded_bytes().contains(&b'/');
        #[cfg(windows)]
        let has_program_path = self
            .program
            .as_encoded_bytes()
            .iter()
            .any(|byte| matches!(byte, b'/' | b'\\'))
            || std::path::Path::new(&self.program).has_root();
        let has_explicit_path = self
            .env
            .set
            .keys()
            .any(|key| env_keys_equal(key, OsStr::new("PATH")));

        if self.env.clear && !has_program_path && !has_explicit_path {
            return Err(SpecError::ClearEnvRequiresPath);
        }

        self.timeout_policy.validate()?;
        self.capture_policy.validate()
    }
}

fn contains_nul(value: &OsStr) -> bool {
    value.as_encoded_bytes().contains(&0)
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProgram => write!(f, "program must not be empty"),
            Self::ProgramContainsNul => write!(f, "program contains a NUL byte"),
            Self::ArgumentContainsNul { index } => {
                write!(f, "argument at index {index} contains a NUL byte")
            }
            Self::CwdContainsNul => write!(f, "cwd contains a NUL byte"),
            Self::ZeroTimeout => write!(f, "timeout must be greater than zero"),
            Self::EmptyEnvKey => {
                write!(f, "environment variable key must not be empty")
            }
            Self::EnvKeyContainsNul => {
                write!(f, "environment variable key contains a NUL byte")
            }
            Self::EnvKeyContainsEquals => {
                write!(f, "environment variable key contains '='")
            }
            Self::EnvValueContainsNul => {
                write!(f, "environment variable value contains a NUL byte")
            }
            Self::EnvKeyConflict => {
                write!(f, "environment variable key is both set and removed")
            }
            Self::ClearEnvRequiresPath => {
                write!(f, "cleared environment requires PATH or a program path")
            }
            Self::ZeroReadChunk => {
                write!(f, "read chunk size must be greater than zero")
            }
            Self::CaptureDirContainsNul => {
                write!(f, "capture directory contains a NUL byte")
            }
        }
    }
}

impl std::error::Error for SpecError {}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            timeout: None,
            grace_period: Duration::from_secs(2),
            forced_wait: Duration::from_secs(5),
            output_drain: Duration::from_secs(1),
        }
    }
}

impl TimeoutPolicy {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(SpecError::ZeroTimeout);
        }
        Ok(())
    }
}

impl EnvSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        for key in self.set.keys().chain(self.remove.iter()) {
            if key.is_empty() {
                return Err(SpecError::EmptyEnvKey);
            } else if contains_nul(key) {
                return Err(SpecError::EnvKeyContainsNul);
            } else if key.as_encoded_bytes().contains(&b'=') {
                return Err(SpecError::EnvKeyContainsEquals);
            }
        }

        for value in self.set.values() {
            if contains_nul(value) {
                return Err(SpecError::EnvValueContainsNul);
            }
        }

        for key in self.set.keys() {
            if self
                .remove
                .iter()
                .any(|removed| env_keys_equal(key, removed))
            {
                return Err(SpecError::EnvKeyConflict);
            }
        }

        #[cfg(windows)]
        for (index, key) in self.set.keys().enumerate() {
            if self
                .set
                .keys()
                .skip(index + 1)
                .any(|other| env_keys_equal(key, other))
            {
                return Err(SpecError::EnvKeyConflict);
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn env_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    left == right
}
#[cfg(windows)]
fn env_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    crate::prepare::env_keys_equal(left, right)
}

#[derive(Debug, PartialEq, Eq, Default)]
pub enum OutputMode {
    #[default]
    Capture,
    StreamAndCapture,
}

pub struct CapturePolicy {
    pub mode: OutputMode,
    pub read_chunk_bytes: usize,
    pub tail_bytes_per_stream: usize,
    pub stream_queue_bytes: usize,
    pub file_quota_bytes_per_stream: u64,
    pub capture_dir: Option<PathBuf>,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            mode: OutputMode::Capture,
            read_chunk_bytes: 32 * 1024,
            tail_bytes_per_stream: 1024 * 1024,
            stream_queue_bytes: 1024 * 1024,
            file_quota_bytes_per_stream: 256 * 1024 * 1024,
            capture_dir: None,
        }
    }
}

impl CapturePolicy {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.read_chunk_bytes == 0 {
            return Err(SpecError::ZeroReadChunk);
        }

        if self
            .capture_dir
            .as_ref()
            .is_some_and(|dir| contains_nul(dir.as_os_str()))
        {
            return Err(SpecError::CaptureDirContainsNul);
        }

        Ok(())
    }
}
