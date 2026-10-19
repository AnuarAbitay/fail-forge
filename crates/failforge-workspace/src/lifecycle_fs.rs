//! Small Unix descriptor primitives for the campaign V1 protocol.
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;

use crate::{InputError, failure};

const DIRECTORY_FLAGS: i32 =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

fn cstring(path: &OsStr) -> io::Result<CString> {
    CString::new(path.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid workspace entry"))
}

fn open(parent: RawFd, path: &OsStr, flags: i32) -> io::Result<File> {
    let path = cstring(path)?;
    // SAFETY: path is terminated; openat creates a new independently owned fd.
    let fd = unsafe { libc::openat(parent, path.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this newly opened descriptor has no other Rust owner.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn uid() -> libc::uid_t {
    // SAFETY: geteuid takes no pointers and has no side effects.
    unsafe { libc::geteuid() }
}

pub(super) fn info(file: &File) -> io::Result<libc::stat> {
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: file is live and value points to writable output storage.
    if unsafe { libc::fstat(file.as_raw_fd(), value.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstat initialized value.
    Ok(unsafe { value.assume_init() })
}

pub(super) fn stat_at(parent: RawFd, path: &OsStr) -> io::Result<libc::stat> {
    let path = cstring(path)?;
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: path is terminated and value is writable output storage.
    if unsafe {
        libc::fstatat(
            parent,
            path.as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized value.
    Ok(unsafe { value.assume_init() })
}

pub(super) fn owned(info: &libc::stat, uid: libc::uid_t) -> Result<(), InputError> {
    super::require(info.st_uid == uid, "wrong_owner")
}

pub(super) fn directory_info(
    info: &libc::stat,
    uid: libc::uid_t,
    private: bool,
) -> Result<(), InputError> {
    super::require(
        info.st_mode & libc::S_IFMT == libc::S_IFDIR,
        "not_a_directory",
    )?;
    owned(info, uid)?;
    super::require(
        !private || info.st_mode & 0o7777 == 0o700,
        "invalid_directory_permissions",
    )
}

pub(super) fn regular_info(info: &libc::stat, uid: libc::uid_t) -> Result<(), InputError> {
    super::require(
        info.st_mode & libc::S_IFMT == libc::S_IFREG,
        "not_a_regular_file",
    )?;
    owned(info, uid)?;
    super::require(info.st_nlink == 1, "invalid_file_link_count")?;
    super::require(info.st_mode & 0o7777 == 0o600, "invalid_file_permissions")
}

pub(super) fn directory(
    parent: RawFd,
    path: &OsStr,
    uid: libc::uid_t,
    private: bool,
) -> Result<File, InputError> {
    let before = stat_at(parent, path)?;
    directory_info(&before, uid, private)?;
    let file = open(parent, path, DIRECTORY_FLAGS)?;
    let after = info(&file)?;
    directory_info(&after, uid, private)?;
    super::require(
        before.st_dev == after.st_dev && before.st_ino == after.st_ino,
        "directory_changed",
    )?;
    Ok(file)
}

pub(super) fn regular(
    parent: &File,
    path: &OsStr,
    uid: libc::uid_t,
    writable: bool,
    create: bool,
) -> Result<File, InputError> {
    let flags = if writable {
        libc::O_RDWR
    } else {
        libc::O_RDONLY
    } | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if create {
            libc::O_CREAT | libc::O_EXCL
        } else {
            0
        };
    let file = open(parent.as_raw_fd(), path, flags)?;
    regular_info(&info(&file)?, uid)?;
    Ok(file)
}

pub(super) fn read_marker(file: &File, expected: &[u8], partial: bool) -> Result<(), InputError> {
    // Read one extra byte to reject trailing bytes without trusting file size.
    // pread leaves offsets of inherited lease descriptions untouched.
    let mut bytes = vec![0; expected.len() + 1];
    let count = file.read_at(&mut bytes, 0)?;
    bytes.truncate(count);
    super::require(
        if partial {
            expected.starts_with(&bytes)
        } else {
            expected == bytes
        },
        "invalid_marker",
    )
}

pub(super) fn write_marker(mut file: &File, bytes: &[u8]) -> Result<(), InputError> {
    match file.write_all(bytes) {
        Err(error) if error.kind() == io::ErrorKind::WriteZero => {
            return Err(failure("marker_write_failed"));
        }
        other => other?,
    }
    file.sync_all()?;
    Ok(())
}

pub(super) fn try_lock(file: &File) -> io::Result<bool> {
    loop {
        // SAFETY: file is live and flock accepts integer flags only.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::WouldBlock => return Ok(false),
            io::ErrorKind::Interrupted => continue,
            _ => return Err(error),
        }
    }
}

pub(super) fn same_at(
    parent: RawFd,
    path: &OsStr,
    file: &File,
    reason: &'static str,
) -> Result<(), InputError> {
    let current = stat_at(parent, path)?;
    let opened = info(file)?;
    super::require(
        current.st_dev == opened.st_dev && current.st_ino == opened.st_ino,
        reason,
    )
}

pub(super) fn mkdir_at(parent: RawFd, path: &OsStr) -> io::Result<()> {
    let path = cstring(path)?;
    // SAFETY: path is terminated; parent is borrowed and mode is a plain value.
    if unsafe { libc::mkdirat(parent, path.as_ptr(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn unlink(parent: &File, path: &OsStr, directory: bool) -> io::Result<()> {
    let path = cstring(path)?;
    // SAFETY: live parent and terminated relative path; unlink never follows links.
    if unsafe {
        libc::unlinkat(
            parent.as_raw_fd(),
            path.as_ptr(),
            if directory { libc::AT_REMOVEDIR } else { 0 },
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn rename(parent: &File, from: &str, to: &str) -> io::Result<()> {
    let from = cstring(OsStr::new(from))?;
    let to = cstring(OsStr::new(to))?;
    // SAFETY: both terminated paths are anchored to the same retained directory.
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn chmod(file: &File, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: fchmod affects only the retained descriptor, not a followed alias.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn random_token() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    // UUID4 layout, matching legacy names while validation accepts any hex token.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

// Python sorts decoded Unix names using surrogateescape. Preserve that ordering
// even for foreign, invalid UTF-8 registry entries (which are never deleted).
pub(super) fn python_name_key(value: &OsStr) -> Vec<u32> {
    let mut bytes = value.as_bytes();
    let mut result = vec![];
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                result.extend(text.chars().map(u32::from));
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                result.extend(
                    std::str::from_utf8(&bytes[..valid])
                        .expect("validated UTF-8 prefix")
                        .chars()
                        .map(u32::from),
                );
                let invalid = error.error_len().unwrap_or(bytes.len() - valid);
                result.extend(
                    bytes[valid..valid + invalid]
                        .iter()
                        .map(|byte| 0xdc00 + u32::from(*byte)),
                );
                bytes = &bytes[valid + invalid..];
            }
        }
    }
    result
}

#[cfg(target_os = "macos")]
pub(super) fn directory_unlinked(file: &File) -> Result<bool, InputError> {
    // APFS keeps st_nlink == 2 on an open directory after rmdir. F_GETPATH
    // updates after rename but can retain a stale path after unlink. Check that
    // the reported path no longer names this inode. This is not an atomic
    // pathname proof against an uncooperative renamer; callers use it only for
    // an already-absent, empty campaign and never delete via this path.
    let mut bytes = [0_u8; libc::PATH_MAX as usize];
    // SAFETY: file is live and F_GETPATH writes at most PATH_MAX bytes into the
    // supplied buffer, as required by the macOS fcntl contract.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| failure("directory_changed"))?;
    let path = OsStr::from_bytes(&bytes[..end]);
    match stat_at(libc::AT_FDCWD, path) {
        Ok(current) => {
            let opened = info(file)?;
            Ok(current.st_dev != opened.st_dev || current.st_ino != opened.st_ino)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(target_os = "macos"))]
pub(super) fn directory_unlinked(file: &File) -> Result<bool, InputError> {
    Ok(info(file)?.st_nlink == 0)
}

pub(super) fn names(directory: &File) -> io::Result<Vec<OsString>> {
    // readdir must not change the retained/inherited directory's shared offset.
    let descriptor = open(directory.as_raw_fd(), OsStr::new("."), DIRECTORY_FLAGS)?.into_raw_fd();
    // SAFETY: successful fdopendir takes ownership of the newly opened fd.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, leaving descriptor owned by this function.
        unsafe {
            libc::close(descriptor);
        }
        return Err(error);
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: this guard uniquely owns the successful fdopendir stream.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    let mut result = vec![];
    loop {
        // SAFETY: errno storage is thread local; readdir borrows a live stream.
        unsafe {
            *errno_pointer() = 0;
        }
        // SAFETY: the stream stays alive until this function returns.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: errno_pointer returns this thread's initialized errno.
            if unsafe { *errno_pointer() } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        // SAFETY: POSIX dirent.d_name is NUL terminated until the next readdir.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes != b"." && bytes != b".." {
            result.push(OsString::from_vec(bytes.to_vec()));
        }
    }
    result.sort_by_cached_key(|name| python_name_key(name));
    Ok(result)
}

#[cfg(target_os = "macos")]
unsafe fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __error returns thread-local errno storage on macOS.
    unsafe { libc::__error() }
}

#[cfg(not(target_os = "macos"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __errno_location returns thread-local errno storage on Linux.
    unsafe { libc::__errno_location() }
}
