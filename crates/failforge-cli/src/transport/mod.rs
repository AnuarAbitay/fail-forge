//! Private request, response, and readiness capabilities.
#[cfg(unix)]
pub(crate) mod readiness;
#[cfg(unix)]
pub(crate) mod request;
pub(crate) mod response;
