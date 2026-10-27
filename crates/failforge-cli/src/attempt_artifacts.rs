//! Retained-directory creation of a fresh attempt report and its parents.
#[cfg(unix)]
#[path = "attempt_artifacts_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "attempt_artifacts_windows.rs"]
mod platform;
pub(crate) use platform::create;
