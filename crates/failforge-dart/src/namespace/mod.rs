//! Byte-bound isolation of a subject package from its test framework dependency.
//!
//! Resolved SDK calls are planned by the Dart analyzer. This consumer checks the
//! complete source/configuration binding, authorizes only namespace-prefix
//! bytes, and commits prepared-copy edits with rollback on failure. Original
//! project and framework files are read-only inputs.
#[cfg(unix)]
pub(crate) mod fs;
#[cfg(windows)]
#[path = "fs_windows.rs"]
pub(crate) mod fs;
mod isolation;
mod plan;
mod scanner;

pub use isolation::isolate_subject;

#[derive(Debug, thiserror::Error)]
pub enum NamespaceError {
    #[error("runtime package URI calls require a resolved namespace plan")]
    PlanRequired,
    #[error("{0}")]
    Invalid(String),
    #[error("namespace I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("namespace isolation cancelled")]
    Cancelled(i32),
}

impl From<failforge_workspace::InputError> for NamespaceError {
    fn from(error: failforge_workspace::InputError) -> Self {
        match error {
            failforge_workspace::InputError::Io(error) => Self::Io(error),
            failforge_workspace::InputError::Invalid(message) => Self::Invalid(message),
            failforge_workspace::InputError::Cancelled(signal) => Self::Cancelled(signal),
        }
    }
}

pub(super) fn invalid(message: impl Into<String>) -> NamespaceError {
    NamespaceError::Invalid(message.into())
}

pub(super) fn require(condition: bool, message: &'static str) -> Result<(), NamespaceError> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 2000;
const MAX_EDITS: usize = 10_000;

#[cfg(test)]
mod tests;
