//! Bounded, stable reads of unlinked controller-owned request descriptors.
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;

pub(crate) const MAX_REQUEST: u64 = 64 * 1024 * 1024;

pub(crate) fn read(value: &OsStr) -> io::Result<Vec<u8>> {
    let value = value.to_str().ok_or_else(invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let source: RawFd = value.parse().map_err(|_| invalid())?;
    if source < 3 {
        return Err(invalid());
    }
    // SAFETY: duplicate the caller's live descriptor without taking ownership.
    let copied = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful duplication created this uniquely owned descriptor.
    let mut file = unsafe { File::from_raw_fd(copied) };
    let before = file.metadata()?;
    // SAFETY: the getters inspect process state and a live descriptor only.
    let (uid, flags) = unsafe { (libc::geteuid(), libc::fcntl(source, libc::F_GETFL)) };
    if !before.is_file()
        || before.uid() != uid
        || before.mode() & 0o7777 != 0o600
        || before.nlink() != 0
        || before.len() == 0
        || before.len() > MAX_REQUEST
        || flags < 0
        || flags & libc::O_ACCMODE == libc::O_WRONLY
        || flags & libc::O_APPEND != 0
        || file.stream_position()? != 0
    {
        return Err(invalid());
    }
    // SAFETY: mark the retained caller descriptor close-on-exec as well.
    let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(source, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_REQUEST + 1).read_to_end(&mut bytes)?;
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
    Ok(bytes)
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native request")
}
