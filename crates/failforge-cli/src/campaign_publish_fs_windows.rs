//! Windows report capabilities and immutable byte fingerprints.
use failforge_workspace::windows_fs::{self as win, Identity};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path};
use windows_sys::Win32::Storage::FileSystem::*;

pub(super) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub(super) fn directory(parent: &File, name: &str) -> io::Result<File> {
    win::directory_at(parent, OsStr::new(name))
}
pub(super) fn walk(path: &Path) -> io::Result<File> {
    win::directory(path)
}
pub(super) fn create_directory(parent: &File, name: &str) -> io::Result<File> {
    win::create_directory_at(parent, OsStr::new(name))
}
pub(super) fn create_file(parent: &File, name: &str) -> io::Result<File> {
    win::regular_at(parent, OsStr::new(name), true, true)
}
pub(super) fn regular(parent: &File, name: &str) -> io::Result<File> {
    win::regular_at(parent, OsStr::new(name), false, false)
}
pub(super) fn rename_file(parent: &File, file: &File, to: &str, replace: bool) -> io::Result<()> {
    win::rename_handle(file, parent, OsStr::new(to), replace)
}
pub(super) fn verify_file(parent: &File, name: &str, file: &File) -> io::Result<()> {
    win::same_entry(parent, OsStr::new(name), file)
}
pub(super) fn identity(file: &File) -> io::Result<Identity> {
    win::identity(file)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Fingerprint {
    identity: Identity,
    bytes: u64,
    attributes: u32,
    links: u32,
    created: i64,
    modified: i64,
    changed: i64,
}
pub(super) fn fingerprint(file: &File) -> io::Result<Fingerprint> {
    let info = win::information(file)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the retained handle and ABI-sized writable output remain valid.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Fingerprint {
        identity: win::identity(file)?,
        bytes: file.metadata()?.len(),
        attributes: info.dwFileAttributes,
        links: info.nNumberOfLinks,
        created: basic.CreationTime,
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    })
}
pub(super) fn staged(file: &File, bytes: u64) -> io::Result<Fingerprint> {
    win::require_private(file, false)?;
    let fingerprint = fingerprint(file)?;
    if fingerprint.bytes != bytes {
        return Err(invalid("invalid staged publication length"));
    }
    Ok(fingerprint)
}
pub(super) fn transfer(
    file: &File,
    bytes: u64,
    hash: &str,
    before: &Fingerprint,
    destination: &mut impl Write,
) -> io::Result<()> {
    if &fingerprint(file)? != before {
        return Err(invalid("report payload changed"));
    }
    let mut buffer = [0; 128 * 1024];
    let mut offset = 0;
    let mut digest = Sha256::new();
    while offset < bytes {
        let count = (bytes - offset).min(buffer.len() as u64) as usize;
        let read = file.seek_read(&mut buffer[..count], offset)?;
        if read == 0 {
            return Err(invalid("report payload was truncated"));
        }
        digest.update(&buffer[..read]);
        destination.write_all(&buffer[..read])?;
        offset += read as u64;
    }
    if file.seek_read(&mut buffer[..1], offset)? != 0
        || &fingerprint(file)? != before
        || format!("{:x}", digest.finalize()) != hash
    {
        return Err(invalid("report payload changed or hash mismatched"));
    }
    Ok(())
}
pub(super) fn read(file: &File, limit: usize) -> io::Result<Vec<u8>> {
    let bytes = file.metadata()?.len();
    if bytes > limit as u64 {
        return Err(invalid("report payload exceeds its byte limit"));
    }
    let before = fingerprint(file)?;
    let mut result = vec![0; bytes as usize];
    let mut offset = 0;
    while offset < result.len() {
        let read = file.seek_read(&mut result[offset..], offset as u64)?;
        if read == 0 {
            return Err(invalid("report payload truncated"));
        }
        offset += read;
    }
    if fingerprint(file)? != before {
        return Err(invalid("report payload changed"));
    }
    Ok(result)
}
pub(super) fn path_text(path: &Path) -> io::Result<&str> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("non-Unicode report path"))?;
    if !path.is_absolute()
        || text.contains('\0')
        || path.file_name().is_none()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("invalid report path"));
    }
    Ok(text)
}
/// FlushFileBuffers does not support ordinary directory handles on Windows.
/// Every payload is flushed before its native atomic handle rename instead.
pub(super) fn sync_directory(_directory: &File) -> io::Result<()> {
    Ok(())
}
