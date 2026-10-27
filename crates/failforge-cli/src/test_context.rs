//! Read the controller's private catalogue context before starting any child.
#[cfg(unix)]
use failforge_dart::{SelectionContext, parse_selection_context};
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::io;

#[cfg(unix)]
pub(crate) const CONTEXT_ENV: &str = "FAILFORGE_TEST_CONTEXT_FD";

#[cfg(unix)]
pub(crate) fn from_env(has_evidence: bool) -> io::Result<Option<SelectionContext>> {
    from_value(std::env::var_os(CONTEXT_ENV).as_deref(), has_evidence)
}

#[cfg(unix)]
fn from_value(value: Option<&OsStr>, has_evidence: bool) -> io::Result<Option<SelectionContext>> {
    let Some(value) = value else { return Ok(None) };
    if !has_evidence {
        return Err(invalid());
    }
    let bytes = crate::private_request::read(value).map_err(|_| invalid())?;
    parse_selection_context(&bytes)
        .map(Some)
        .map_err(|_| invalid())
}

#[cfg(unix)]
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native test context")
}

#[cfg(all(test, unix))]
#[path = "test_context_tests.rs"]
mod tests;
