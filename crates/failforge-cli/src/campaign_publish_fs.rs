//! Small descriptor-relative primitives for final report publication.
use std::ffi::CString;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

pub(super) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn component(name: &str) -> io::Result<CString> {
    if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
        return Err(invalid("invalid report component"));
    }
    CString::new(name).map_err(|_| invalid("invalid report component"))
}

pub(super) fn stat(parent: &File, name: &str) -> io::Result<libc::stat> {
    let name = component(name)?;
    let mut result = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the live parent and terminated component identify one entry.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            result.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized result.
    Ok(unsafe { result.assume_init() })
}

#[allow(clippy::unnecessary_cast)] // dev_t is signed on macOS and u64 on Linux.
pub(super) fn same(info: &libc::stat, file: &File, kind: libc::mode_t) -> io::Result<bool> {
    let owned = file.metadata()?;
    Ok(info.st_mode & libc::S_IFMT == kind
        && info.st_dev as u64 == owned.dev()
        && info.st_ino == owned.ino())
}

pub(super) fn directory(parent: &File, name: &str) -> io::Result<File> {
    let name = component(name)?;
    // SAFETY: openat returns a unique descriptor and refuses a symlink.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful open created this owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

pub(super) fn walk(path: &Path) -> io::Result<File> {
    let mut current = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = directory(
                    &current,
                    name.to_str()
                        .ok_or_else(|| invalid("non-Unicode report path"))?,
                )?;
            }
            _ => return Err(invalid("invalid report path")),
        }
    }
    Ok(current)
}

pub(super) fn create_directory(parent: &File, name: &str) -> io::Result<File> {
    let terminated = component(name)?;
    // SAFETY: mkdirat acts only relative to the retained directory.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), terminated.as_ptr(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let result = directory(parent, name)?;
    let named = stat(parent, name)?;
    if !same(&named, &result, libc::S_IFDIR)? {
        return Err(invalid("new report directory changed"));
    }
    Ok(result)
}

pub(super) fn create_file(parent: &File, name: &str) -> io::Result<File> {
    let name = component(name)?;
    // SAFETY: O_EXCL returns a fresh owned regular file without following links.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a unique live descriptor.
    let file = unsafe { File::from_raw_fd(descriptor) };
    // SAFETY: chmod acts on our fresh file, not its possibly replaced name.
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

pub(super) fn read_link(parent: &File, name: &str) -> io::Result<String> {
    let name = component(name)?;
    let mut bytes = [0u8; 4096];
    // SAFETY: readlinkat writes at most the provided buffer length.
    let length = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize == bytes.len() {
        return Err(invalid("report alias is too long"));
    }
    String::from_utf8(bytes[..length as usize].to_vec())
        .map_err(|_| invalid("invalid report alias"))
}

pub(super) fn symlink(parent: &File, name: &str, target: &str) -> io::Result<()> {
    let name = component(name)?;
    let target = CString::new(target).map_err(|_| invalid("invalid report alias"))?;
    // SAFETY: both strings are terminated and creation is relative to the held FD.
    if unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn rename(parent: &File, from: &str, to: &str) -> io::Result<()> {
    let from = component(from)?;
    let to = component(to)?;
    // SAFETY: both single names are resolved against the same held directory.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    device: u64,
    inode: u64,
    size: u64,
    uid: u32,
    mode: u32,
    links: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<Metadata> for Fingerprint {
    fn from(value: Metadata) -> Self {
        Self {
            device: value.dev(),
            inode: value.ino(),
            size: value.len(),
            uid: value.uid(),
            mode: value.mode(),
            links: value.nlink(),
            modified: (value.mtime(), value.mtime_nsec()),
            changed: (value.ctime(), value.ctime_nsec()),
        }
    }
}

pub(super) fn staged(file: &File, bytes: u64) -> io::Result<Fingerprint> {
    let value = file.metadata()?;
    // SAFETY: geteuid observes process state only.
    if !value.is_file()
        || value.uid() != unsafe { libc::geteuid() }
        || value.mode() & 0o7777 != 0o600
        || value.nlink() != 1
        || value.len() != bytes
    {
        return Err(invalid("invalid staged publication file"));
    }
    Ok(value.into())
}

pub(super) fn payload(file: &File, bytes: u64, hash: &str) -> io::Result<Fingerprint> {
    let metadata = file.metadata()?;
    // SAFETY: getters inspect process state and the live descriptor only.
    let (uid, flags, offset) = unsafe {
        (
            libc::geteuid(),
            libc::fcntl(file.as_raw_fd(), libc::F_GETFL),
            libc::lseek(file.as_raw_fd(), 0, libc::SEEK_CUR),
        )
    };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 0
        || metadata.len() != bytes
        || bytes == 0
        || offset != 0
        || flags < 0
        || flags & libc::O_ACCMODE == libc::O_WRONLY
        || flags & libc::O_APPEND != 0
    {
        return Err(invalid("invalid private report payload"));
    }
    let before = Fingerprint::from(metadata);
    transfer(file, bytes, hash, &before, &mut io::sink())?;
    Ok(before)
}

pub(super) fn transfer(
    file: &File,
    bytes: u64,
    hash: &str,
    before: &Fingerprint,
    destination: &mut impl Write,
) -> io::Result<()> {
    if &Fingerprint::from(file.metadata()?) != before {
        return Err(invalid("report payload changed"));
    }
    let mut buffer = [0u8; 128 * 1024];
    let mut offset = 0u64;
    let mut digest = Sha256::new();
    while offset < bytes {
        let count =
            usize::try_from((bytes - offset).min(buffer.len() as u64)).expect("bounded buffer");
        let read = file.read_at(&mut buffer[..count], offset)?;
        if read == 0 {
            return Err(invalid("report payload was truncated"));
        }
        digest.update(&buffer[..read]);
        destination.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    if file.read_at(&mut buffer[..1], offset)? != 0
        || &Fingerprint::from(file.metadata()?) != before
        || format!("{:x}", digest.finalize()) != hash
    {
        return Err(invalid("report payload changed or hash mismatched"));
    }
    Ok(())
}

pub(super) fn path_text(path: &Path) -> io::Result<&str> {
    let value = path
        .to_str()
        .ok_or_else(|| invalid("non-Unicode report path"))?;
    if !path.is_absolute()
        || value.contains('\0')
        || value.split('/').any(|part| matches!(part, "." | ".."))
        || path.file_name().is_none()
    {
        return Err(invalid("invalid report path"));
    }
    Ok(value)
}
