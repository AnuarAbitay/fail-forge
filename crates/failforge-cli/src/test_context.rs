//! Read the controller's private catalogue context before starting any child.
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;

use failforge_dart::{SelectionContext, parse_selection_context};

pub(crate) const CONTEXT_ENV: &str = "FAILFORGE_TEST_CONTEXT_FD";
const MAX_CONTEXT: u64 = 64 * 1024 * 1024;

pub(crate) fn from_env(has_evidence: bool) -> io::Result<Option<SelectionContext>> {
    from_value(std::env::var_os(CONTEXT_ENV).as_deref(), has_evidence)
}

fn from_value(value: Option<&OsStr>, has_evidence: bool) -> io::Result<Option<SelectionContext>> {
    let Some(value) = value else { return Ok(None) };
    if !has_evidence {
        return Err(invalid());
    }
    let value = value.to_str().ok_or_else(invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let source: RawFd = value.parse().map_err(|_| invalid())?;
    if source < 3 {
        return Err(invalid());
    }
    // SAFETY: duplicate a caller-owned descriptor; never take its ownership.
    let copied = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful duplication returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(copied) };
    let before = file.metadata()?;
    // SAFETY: fcntl getters and geteuid do not access user pointers.
    let (uid, flags) = unsafe { (libc::geteuid(), libc::fcntl(source, libc::F_GETFL)) };
    if !before.is_file()
        || before.uid() != uid
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 0
        || before.len() == 0
        || before.len() > MAX_CONTEXT
        || flags < 0
        || flags & libc::O_ACCMODE == libc::O_WRONLY
        || flags & libc::O_APPEND != 0
        || file.stream_position()? != 0
    {
        return Err(invalid());
    }
    // The original inherited descriptor must close on child exec as well.
    // SAFETY: source remains live while its flags are queried and updated.
    let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(source, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_CONTEXT + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
        || after.nlink() != 0
        || bytes.last() != Some(&b'\n')
    {
        return Err(invalid());
    }
    parse_selection_context(&bytes)
        .map(Some)
        .map_err(|_| invalid())
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native test context")
}

#[cfg(test)]
#[path = "test_context_tests.rs"]
mod tests;
