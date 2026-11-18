//! Retained-directory creation of a fresh attempt report and its parents.
#[cfg(unix)]
#[path = "artifacts_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "artifacts_windows.rs"]
mod platform;
pub(crate) use platform::create;
