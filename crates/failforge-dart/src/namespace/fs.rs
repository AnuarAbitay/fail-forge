//! Descriptor-relative reads and conditional atomic replacement in a fresh copy.
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use failforge_workspace::Cancellation;
use sha2::{Digest, Sha256};

use super::{NamespaceError, invalid, require};

const DIR_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
pub(super) type Inventory = BTreeMap<String, String>;

fn name(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid namespace path"))
}

fn open(parent: RawFd, value: &OsStr, flags: i32) -> io::Result<File> {
    let value = name(value)?;
    // SAFETY: value is a terminated path; successful openat returns a fresh fd.
    let fd = unsafe { libc::openat(parent, value.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this successful descriptor has no other Rust owner.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat(parent: RawFd, value: &OsStr) -> io::Result<libc::stat> {
    let value = name(value)?;
    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: value is terminated and info is writable native output storage.
    if unsafe {
        libc::fstatat(
            parent,
            value.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized info.
    Ok(unsafe { info.assume_init() })
}

// dev_t is signed on macOS and already u64 on supported Linux targets.
#[allow(clippy::unnecessary_cast)]
fn same(info: &libc::stat, opened: &Metadata) -> bool {
    info.st_dev as u64 == opened.dev() && info.st_ino == opened.ino()
}

fn same_metadata(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
        && left.mode() == right.mode()
        && left.nlink() == right.nlink()
}

fn directory(parent: RawFd, value: &OsStr) -> Result<File, NamespaceError> {
    let before = stat(parent, value)?;
    require(
        before.st_mode & libc::S_IFMT == libc::S_IFDIR,
        "namespace directory is not a real directory",
    )?;
    let file = open(parent, value, DIR_FLAGS)?;
    let after = file.metadata()?;
    require(
        after.is_dir() && same(&before, &after),
        "namespace directory changed",
    )?;
    Ok(file)
}

fn regular(parent: RawFd, value: &OsStr) -> Result<File, NamespaceError> {
    let before = stat(parent, value)?;
    require(
        before.st_mode & libc::S_IFMT == libc::S_IFREG,
        "namespace source must be a regular file without symlinks",
    )?;
    let file = open(
        parent,
        value,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
    )?;
    require(
        file.metadata()?.is_file() && same(&before, &file.metadata()?),
        "namespace source changed",
    )?;
    Ok(file)
}

fn private(info: &Metadata) -> Result<(), NamespaceError> {
    // SAFETY: geteuid has no arguments or memory effects.
    require(
        info.uid() == unsafe { libc::geteuid() } && info.nlink() == 1,
        "namespace copy file must be owned and have no hard links",
    )
}

fn unchanged(file: &File, before: &Metadata) -> Result<(), NamespaceError> {
    require(
        same_metadata(before, &file.metadata()?),
        "namespace input changed while reading",
    )
}

fn hash(mut file: &File, cancel: &Cancellation) -> Result<String, NamespaceError> {
    let before = file.metadata()?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        cancel.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    unchanged(file, &before)?;
    Ok(format!("{:x}", digest.finalize()))
}

/// Match POSIX fsync (and the original Python os.fsync) on every Unix target.
/// Apple's std::fs::File::sync_all additionally requests F_FULLFSYNC hardware
/// cache flushing. These disposable copies need the same file-before-rename
/// and directory-after-rename ordering, without that stronger durability level.
pub(super) fn sync(file: &File) -> io::Result<()> {
    loop {
        // SAFETY: fsync borrows this live descriptor without retaining it.
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) struct Root {
    pub path: PathBuf,
    file: File,
}

impl Root {
    pub fn open(path: &Path) -> Result<Self, NamespaceError> {
        require(path.is_absolute(), "namespace roots must be absolute")?;
        // Accept platform ancestor aliases such as macOS /tmp, but no root
        // symlink. All descendants are subsequently opened with O_NOFOLLOW.
        let original = directory(libc::AT_FDCWD, path.as_os_str())?;
        let canonical = path.canonicalize()?;
        let current = stat(libc::AT_FDCWD, canonical.as_os_str())?;
        require(
            same(&current, &original.metadata()?),
            "namespace root changed",
        )?;
        Ok(Self {
            path: canonical,
            file: original,
        })
    }

    pub fn verify(&self) -> Result<(), NamespaceError> {
        require(
            same(
                &stat(libc::AT_FDCWD, self.path.as_os_str())?,
                &self.file.metadata()?,
            ),
            "namespace root changed",
        )
    }

    fn parent(&self, relative: &str) -> Result<(File, OsString), NamespaceError> {
        crate::validate_relative_target(relative)
            .map_err(|_| invalid("unsafe namespace relative path"))?;
        self.verify()?;
        let mut directory_file = self.file.try_clone()?;
        let parts: Vec<_> = relative.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            directory_file = directory(directory_file.as_raw_fd(), OsStr::new(part))?;
        }
        Ok((directory_file, OsString::from(parts[parts.len() - 1])))
    }

    pub fn read(
        &self,
        relative: &str,
        limit: usize,
        owned: bool,
    ) -> Result<(Vec<u8>, BoundFile), NamespaceError> {
        let (parent, name) = self.parent(relative)?;
        let mut file = regular(parent.as_raw_fd(), &name)?;
        let info = file.metadata()?;
        if owned {
            private(&info)?;
        }
        let mut bytes = vec![];
        Read::by_ref(&mut file)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        require(
            bytes.len() <= limit,
            "namespace source exceeds its byte limit",
        )?;
        unchanged(&file, &info)?;
        require(
            same(&stat(parent.as_raw_fd(), &name)?, &info),
            "namespace source changed",
        )?;
        Ok((
            bytes,
            BoundFile {
                relative: relative.into(),
                parent,
                name,
                file,
                info,
                generation: 0,
            },
        ))
    }

    pub fn inventory(
        &self,
        prefix: &str,
        owned: bool,
        cancel: &Cancellation,
    ) -> Result<Inventory, NamespaceError> {
        self.verify()?;
        let directory_file = directory(self.file.as_raw_fd(), OsStr::new(prefix))?;
        let mut files = Inventory::new();
        inventory(&directory_file, "", owned, cancel, 0, &mut files)?;
        Ok(files)
    }
}

pub(crate) struct BoundFile {
    relative: String,
    parent: File,
    name: OsString,
    file: File,
    info: Metadata,
    generation: usize,
}

impl BoundFile {
    pub fn generation(&self) -> usize {
        self.generation
    }
    pub fn verify(&self, root: &Root) -> Result<(), NamespaceError> {
        let (parent, _) = root.parent(&self.relative)?;
        let current_parent = parent.metadata()?;
        let original_parent = self.parent.metadata()?;
        require(
            current_parent.dev() == original_parent.dev()
                && current_parent.ino() == original_parent.ino(),
            "namespace parent directory changed",
        )?;
        let current = stat(self.parent.as_raw_fd(), &self.name)?;
        require(same(&current, &self.info), "namespace target inode changed")?;
        unchanged(&self.file, &self.info)?;
        private(&self.info)
    }

    pub fn replace(&mut self, root: &Root, bytes: &[u8]) -> Result<(), NamespaceError> {
        self.verify(root)?;
        let mut random = [0; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let temporary_name = format!(
            ".failforge-namespace-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let temporary = OsStr::new(&temporary_name);
        let mut file = open(
            self.parent.as_raw_fd(),
            temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )?;
        struct Temp<'a> {
            parent: &'a File,
            name: &'a OsStr,
            info: Metadata,
            armed: bool,
        }
        impl Drop for Temp<'_> {
            fn drop(&mut self) {
                if self.armed
                    && stat(self.parent.as_raw_fd(), self.name)
                        .is_ok_and(|current| same(&current, &self.info))
                {
                    let _ = unlink(self.parent, self.name);
                }
            }
        }
        let mut guard = Temp {
            parent: &self.parent,
            name: temporary,
            info: file.metadata()?,
            armed: true,
        };
        file.write_all(bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(self.info.mode() & 0o7777))?;
        sync(&file)?;
        let committed_info = file.metadata()?;
        #[cfg(test)]
        super::isolation::stage(super::isolation::Stage::TemporaryReady)?;
        self.verify(root)?;
        require(
            same(&stat(self.parent.as_raw_fd(), temporary)?, &committed_info),
            "namespace temporary inode changed",
        )?;
        unchanged(&file, &committed_info)?;
        private(&committed_info)?;
        let from = name(temporary)?;
        let to = name(&self.name)?;
        // SAFETY: both single names are anchored to the retained target parent.
        if unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                from.as_ptr(),
                self.parent.as_raw_fd(),
                to.as_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // The temporary name becomes vacant at rename. Its next occupant, if
        // any, belongs to somebody else and must never be removed by our guard.
        guard.armed = false;
        // Update ownership immediately after rename, before fallible fsync. A
        // caller can roll this exact inode back even when directory sync fails.
        self.file = file;
        self.info = committed_info;
        self.generation += 1;
        #[cfg(test)]
        super::isolation::stage(super::isolation::Stage::Renamed)?;
        self.info = self.file.metadata()?;
        sync(&self.parent)?;
        self.verify(root)
    }

    pub fn restore(
        &mut self,
        root: &Root,
        expected: &[u8],
        original: &[u8],
    ) -> Result<(), NamespaceError> {
        let (actual, mut current) = root.read(&self.relative, expected.len(), true)?;
        require(
            current.info.dev() == self.info.dev()
                && current.info.ino() == self.info.ino()
                && actual == expected,
            "namespace rollback target changed",
        )?;
        // A previous rename may have succeeded before a failing metadata query.
        // Rebind that exact inode only after checking its complete written bytes.
        current.generation = self.generation;
        *self = current;
        self.replace(root, original)
    }
}

fn unlink(parent: &File, value: &OsStr) -> io::Result<()> {
    let value = name(value)?;
    // SAFETY: the retained directory and terminated single name are valid.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), value.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn inventory(
    root: &File,
    prefix: &str,
    owned: bool,
    cancel: &Cancellation,
    depth: usize,
    result: &mut Inventory,
) -> Result<(), NamespaceError> {
    cancel.check()?;
    require(depth <= 128, "namespace inventory depth exceeds limit")?;
    for name in names(root)? {
        cancel.check()?;
        let text = name
            .to_str()
            .ok_or_else(|| invalid("namespace source path must be UTF-8"))?;
        let relative = if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}/{text}")
        };
        let info = stat(root.as_raw_fd(), &name)?;
        if info.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = directory(root.as_raw_fd(), &name)?;
            inventory(&child, &relative, owned, cancel, depth + 1, result)?;
        } else {
            let file = regular(root.as_raw_fd(), &name)?;
            if owned {
                private(&file.metadata()?)?;
            }
            require(
                result.len() < 200_000,
                "namespace inventory exceeds entry limit",
            )?;
            result.insert(relative, hash(&file, cancel)?);
        }
    }
    Ok(())
}

fn names(root: &File) -> io::Result<Vec<OsString>> {
    let descriptor = open(root.as_raw_fd(), OsStr::new("."), DIR_FLAGS)?.into_raw_fd();
    // SAFETY: successful fdopendir takes ownership of this newly opened fd.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: failed fdopendir leaves descriptor ownership with us.
        unsafe {
            libc::close(descriptor);
        }
        return Err(error);
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: the guard uniquely owns its successful fdopendir stream.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    let mut result = vec![];
    loop {
        // SAFETY: errno storage is local to this thread.
        unsafe {
            *errno_pointer() = 0;
        }
        // SAFETY: stream stays live until this function exits.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: errno storage is local to this thread.
            if unsafe { *errno_pointer() } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        // SAFETY: dirent.d_name is a terminated string until the next readdir.
        let value = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if value != b"." && value != b".." {
            result.push(OsString::from_vec(value.to_vec()));
        }
    }
    result.sort();
    Ok(result)
}

#[cfg(target_os = "macos")]
unsafe fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __error returns thread-local errno on macOS.
    unsafe { libc::__error() }
}
#[cfg(not(target_os = "macos"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __errno_location returns thread-local errno on Linux.
    unsafe { libc::__errno_location() }
}
