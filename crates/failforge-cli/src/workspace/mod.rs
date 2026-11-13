//! Prepare and manage private campaign workspaces.
#[cfg(unix)]
pub(crate) mod handoff;
pub(crate) mod inputs;
#[cfg(unix)]
pub(crate) mod lifecycle;
