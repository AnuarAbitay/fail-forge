//! Unix retained-descriptor report artifacts.
use crate::dart_namespace::invalid;
use failforge_dart::namespace::NamespaceError;
use serde_json::Value;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

fn same_directory(left: &Metadata, right: &Metadata) -> bool {
    left.is_dir() && right.is_dir() && left.dev() == right.dev() && left.ino() == right.ino()
}

#[allow(clippy::unnecessary_cast)] // dev_t is signed on macOS and u64 on Linux.
fn same_identity(named: &libc::stat, owned: &Metadata) -> bool {
    named.st_dev as u64 == owned.dev() && named.st_ino == owned.ino()
}

fn component(value: &OsStr) -> Result<CString, NamespaceError> {
    CString::new(value.as_bytes()).map_err(|_| invalid("invalid namespace artifact name"))
}

fn stat_at(parent: &File, name: &OsStr) -> Result<libc::stat, NamespaceError> {
    let name = component(name)?;
    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: parent owns a live descriptor, name is terminated and info is
    // writable native storage; the final component is never followed.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: successful fstatat initialized info.
    Ok(unsafe { info.assume_init() })
}

/// A report pathname is checked for visibility, but it never authorizes a
/// write. All private artifacts are created relative to the retained directory
/// descriptor, including failure receipts and partial-publication cleanup.
pub(crate) struct ReportDirectory {
    path: PathBuf,
    parent_path: PathBuf,
    parent: File,
    name: OsString,
    file: File,
}

impl ReportDirectory {
    pub(crate) fn verify_file(&self, name: &str, file: &File) -> Result<(), NamespaceError> {
        if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
            return Err(invalid("invalid namespace artifact name"));
        }
        self.verify()?;
        let named = stat_at(&self.file, OsStr::new(name))?;
        let owned = file.metadata()?;
        if !owned.is_file()
            || named.st_mode & libc::S_IFMT != libc::S_IFREG
            || !same_identity(&named, &owned)
        {
            return Err(invalid("namespace report artifact changed"));
        }
        Ok(())
    }

    pub(crate) fn canonical_path(&self) -> Result<PathBuf, NamespaceError> {
        self.verify()?;
        Ok(self.parent_path.join(&self.name))
    }

    pub(crate) fn verify(&self) -> Result<(), NamespaceError> {
        let parent = self.parent.metadata()?;
        let owned = self.file.metadata()?;
        let named = stat_at(&self.parent, &self.name)?;
        if !same_directory(&parent, &fs::symlink_metadata(&self.parent_path)?)
            || !same_directory(&owned, &fs::symlink_metadata(&self.path)?)
            || named.st_mode & libc::S_IFMT != libc::S_IFDIR
            || !same_identity(&named, &owned)
        {
            return Err(invalid("namespace report directory changed"));
        }
        Ok(())
    }

    pub(super) fn remove_owned(&self, name: &OsStr, file: &File) -> Result<(), NamespaceError> {
        let named = match stat_at(&self.file, name) {
            Err(NamespaceError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            result => result?,
        };
        let owned = file.metadata()?;
        if named.st_mode & libc::S_IFMT != libc::S_IFREG || !same_identity(&named, &owned) {
            return Err(invalid("namespace report artifact changed"));
        }
        let name = component(name)?;
        // SAFETY: a retained directory and validated single filename are used;
        // only the inode created by this owner is eligible for cleanup.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub(crate) fn write_private(&self, name: &str, raw: &[u8]) -> Result<File, NamespaceError> {
        if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
            return Err(invalid("invalid namespace artifact name"));
        }
        self.verify()?;
        let name = OsStr::new(name);
        let terminated = component(name)?;
        // SAFETY: the fresh O_EXCL descriptor is uniquely owned below. Artifact
        // creation remains anchored even if an ancestor is renamed concurrently.
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                terminated.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful openat created this unique live descriptor.
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let result = (|| {
            file.write_all(raw)?;
            file.sync_all()?;
            self.verify()?;
            let named = stat_at(&self.file, name)?;
            let owned = file.metadata()?;
            if !same_identity(&named, &owned) || named.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(invalid("namespace report artifact changed"));
            }
            Ok::<_, NamespaceError>(())
        })();
        if let Err(error) = result {
            let _ = self.remove_owned(name, &file);
            return Err(error);
        }
        Ok(file)
    }

    pub(crate) fn write_json(&self, name: &str, value: &Value) -> Result<File, NamespaceError> {
        let mut raw =
            serde_json::to_vec_pretty(value).map_err(|_| invalid("invalid namespace receipt"))?;
        raw.push(b'\n');
        self.write_private(name, &raw)
    }
}

pub(crate) fn create_report_directory(
    directory: &Path,
    protected: &[PathBuf],
) -> Result<ReportDirectory, NamespaceError> {
    // Campaign outputs already have an existing parent. Anchor validation there
    // so an alias cannot place analyzer artifacts inside an input tree.
    let parent = directory
        .parent()
        .ok_or_else(|| invalid("invalid namespace report directory"))?
        .canonicalize()?;
    let name = directory
        .file_name()
        .ok_or_else(|| invalid("invalid namespace report directory"))?
        .to_owned();
    let output = parent.join(&name);
    let parent_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&parent)?;
    if protected
        .iter()
        .any(|root| output.starts_with(root) || root.starts_with(&output))
    {
        return Err(invalid(
            "namespace report directory overlaps protected inputs",
        ));
    }
    if !same_directory(&parent_file.metadata()?, &fs::symlink_metadata(&parent)?)
        || directory
            .parent()
            .ok_or_else(|| invalid("invalid namespace report directory"))?
            .canonicalize()?
            != parent
    {
        return Err(invalid("namespace report parent changed"));
    }
    let terminated = component(&name)?;
    // SAFETY: the validated single name is created beneath the retained parent.
    if unsafe { libc::mkdirat(parent_file.as_raw_fd(), terminated.as_ptr(), 0o700) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returns a unique descriptor and never follows a replacement
    // final-component symlink.
    let descriptor = unsafe {
        libc::openat(
            parent_file.as_raw_fd(),
            terminated.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let report = ReportDirectory {
        path: directory.into(),
        parent_path: parent,
        parent: parent_file,
        name,
        // SAFETY: this successful descriptor has exactly one owner.
        file: unsafe { File::from_raw_fd(descriptor) },
    };
    report.verify()?;
    Ok(report)
}
