//! Console display, diagnostics, signal handling, and awake requests.
pub(crate) mod diagnostics;
#[cfg_attr(windows, path = "display_windows.rs")]
pub(crate) mod display;
pub(crate) mod keep_awake;
pub(crate) mod signals;
#[cfg(windows)]
pub(crate) mod windows_output;
