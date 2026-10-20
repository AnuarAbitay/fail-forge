//! Atomic publication through pinned package and directory identities.
//!
//! The preparation binding detects observed concurrent edits without trusting
//! path traversal at publication time. It does not lock other processes out of
//! the namespace after the final check.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::ProfileError;

type Result<T> = std::result::Result<T, ProfileError>;
type Identity = (u64, u64);

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileState {
    identity: Identity,
    mode: u32,
    size: i64,
    mtime: (i64, i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    state: FileState,
    ctime: (i64, i64),
}

impl Entry {
    #[allow(clippy::unnecessary_cast)] // mode_t is narrower than u32 on macOS.
    fn regular(&self) -> bool {
        self.state.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Destination {
    entry: Entry,
    digest: [u8; 32],
}

/// Opaque snapshot of the paths selected during profile preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBinding {
    project: PathBuf,
    output: PathBuf,
    ancestors: Vec<Identity>,
    parents: Vec<Identity>,
    pubspec: Option<Entry>,
    destination: Option<Destination>,
}

fn config(message: &str) -> ProfileError {
    ProfileError::Config(message.to_owned())
}

fn name(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| config("profile path must not contain NUL"))
}

#[cfg(target_os = "linux")]
const DIRECTORY_ACCESS: i32 = libc::O_PATH;
#[cfg(target_os = "macos")]
const DIRECTORY_ACCESS: i32 = libc::O_SEARCH;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const DIRECTORY_ACCESS: i32 = libc::O_RDONLY;
const DIRECTORY_FLAGS: i32 =
    DIRECTORY_ACCESS | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const READ_FLAGS: i32 = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;

fn open_at(directory: RawFd, path: &OsStr, flags: i32, mode: u32) -> Result<File> {
    let path = name(path)?;
    // SAFETY: the path is terminated; the caller owns the directory descriptor
    // (or uses AT_FDCWD). The returned descriptor is adopted exactly once.
    let descriptor = unsafe { libc::openat(directory, path.as_ptr(), flags, mode as libc::c_uint) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: openat returned a new, owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[allow(clippy::unnecessary_cast)] // Native stat field widths differ by platform.
fn from_stat(info: libc::stat) -> Entry {
    Entry {
        state: FileState {
            identity: (info.st_dev as u64, info.st_ino as u64),
            mode: info.st_mode as u32,
            size: info.st_size as i64,
            mtime: (info.st_mtime as i64, info.st_mtime_nsec as i64),
        },
        ctime: (info.st_ctime as i64, info.st_ctime_nsec as i64),
    }
}

fn file_entry(file: &File) -> Result<Entry> {
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the descriptor is live and info is writable native stat storage.
    if unsafe { libc::fstat(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fstat initialized info on success.
    Ok(from_stat(unsafe { info.assume_init() }))
}

fn entry(directory: RawFd, path: &OsStr) -> Result<Option<Entry>> {
    let path = name(path)?;
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: all pointers are valid for this call; querying a single name
    // relative to the pinned directory never follows its final symlink.
    if unsafe {
        libc::fstatat(
            directory,
            path.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    // SAFETY: fstatat initialized info on success.
    Ok(Some(from_stat(unsafe { info.assume_init() })))
}

fn close(file: File) -> Result<()> {
    let descriptor = file.into_raw_fd();
    // SAFETY: ownership was consumed above. Never retry close: after EINTR a
    // retry could close an unrelated descriptor reused by another thread.
    if unsafe { libc::close(descriptor) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

fn destination(directory: RawFd, path: &OsStr) -> Result<Option<Destination>> {
    let Some(initial) = entry(directory, path)? else {
        return Ok(None);
    };
    if !initial.regular() {
        return Err(config(
            "profile output must be a regular file, not a symlink or special file",
        ));
    }
    let mut file = open_at(directory, path, READ_FLAGS, 0)?;
    let result = (|| {
        let before = file_entry(&file)?;
        if !before.regular() || before != initial {
            return Err(config(
                "profile destination changed while binding its contents",
            ));
        }
        let mut digest = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        let after = file_entry(&file)?;
        if before != after || entry(directory, path)?.as_ref() != Some(&after) {
            return Err(config(
                "profile destination changed while binding its contents",
            ));
        }
        Ok(Some(Destination {
            entry: after,
            digest: digest.finalize().into(),
        }))
    })();
    let closed = close(file);
    result.and_then(|value| closed.map(|()| value))
}

fn same_previous(previous: &Destination, current: Option<&Destination>) -> bool {
    // Our backup hard link changes ctime and nlink. Identity, bytes, permission
    // bits and modification time must still match the original document.
    current.is_some_and(|current| {
        previous.entry.state == current.entry.state && previous.digest == current.digest
    })
}

fn pubspec(directory: RawFd) -> Result<Option<Entry>> {
    let info = entry(directory, OsStr::new("pubspec.yaml"))?;
    if info.as_ref().is_some_and(|info| !info.regular()) {
        return Err(config("package pubspec.yaml must remain a regular file"));
    }
    Ok(info)
}

fn paths(project: &Path, output: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let relative = output
        .strip_prefix(project)
        .map_err(|_| config("profile output must be inside the source package"))?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(config(
            "profile output must name a file inside the source package",
        ));
    }
    let absolute = if project.is_absolute() {
        project.to_owned()
    } else {
        std::env::current_dir()?.join(project)
    };
    let mut normalized = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(part.as_os_str()),
        }
    }
    Ok((
        normalized.clone(),
        normalized.join(relative),
        relative.to_owned(),
    ))
}

struct Directories {
    files: Vec<File>,
    identities: Vec<Identity>,
}

impl Directories {
    fn last(&self) -> RawFd {
        self.files
            .last()
            .expect("root descriptor always exists")
            .as_raw_fd()
    }

    fn push(&mut self, file: File) -> Result<Identity> {
        let identity = file_entry(&file)?.state.identity;
        self.files.push(file);
        self.identities.push(identity);
        Ok(identity)
    }

    fn close(self) -> Result<()> {
        let mut failure = None;
        for file in self.files.into_iter().rev() {
            if let Err(error) = close(file) {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn root_directory(project: &Path, expected: Option<&[Identity]>) -> Result<Directories> {
    let mut directories = Directories {
        files: Vec::new(),
        identities: Vec::new(),
    };
    for (index, part) in project.components().enumerate() {
        let descriptor = directories
            .files
            .last()
            .map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd);
        let opened = open_at(descriptor, part.as_os_str(), DIRECTORY_FLAGS, 0)?;
        let identity = directories.push(opened)?;
        if expected.is_some_and(|expected| expected.get(index) != Some(&identity)) {
            return Err(config(
                "selected package directory changed during profile generation",
            ));
        }
    }
    if expected.is_some_and(|expected| expected.len() != directories.identities.len()) {
        return Err(config(
            "selected package directory changed during profile generation",
        ));
    }
    Ok(directories)
}

/// Bind existing paths and package metadata without creating or listing files.
pub fn capture_output_binding(project: &Path, output: &Path) -> Result<OutputBinding> {
    let (project, output, relative) = paths(project, output)?;
    let mut directories = root_directory(&project, None)?;
    let ancestors = directories.identities.clone();
    let selected_pubspec = pubspec(directories.last())?;
    let mut parents = Vec::new();
    let mut complete = true;
    for part in relative
        .parent()
        .expect("relative file has parent")
        .components()
    {
        let opened = match open_at(directories.last(), part.as_os_str(), DIRECTORY_FLAGS, 0) {
            Err(ProfileError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                complete = false;
                break;
            }
            other => other?,
        };
        if entry(opened.as_raw_fd(), OsStr::new("pubspec.yaml"))?.is_some() {
            return Err(config(
                "profile output must remain in the selected source package",
            ));
        }
        parents.push(directories.push(opened)?);
    }
    let destination = if complete {
        destination(
            directories.last(),
            relative.file_name().expect("relative file has name"),
        )?
    } else {
        None
    };
    directories.close()?;
    Ok(OutputBinding {
        project,
        output,
        ancestors,
        parents,
        pubspec: selected_pubspec,
        destination,
    })
}

/// Only explicit force may replace an existing regular file.
pub fn check_destination(path: &Path, force: bool) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if !metadata.is_file() {
        return Err(config(
            "profile output must be a regular file, not a symlink or special file",
        ));
    }
    if !force {
        return Err(config(
            "profile output already exists; use --force to replace it",
        ));
    }
    Ok(())
}

struct CreatedDirectory {
    parent: usize,
    name: OsString,
    identity: Identity,
}

struct ParentDirectory {
    directories: Directories,
    parents: Vec<Identity>,
    created: Vec<CreatedDirectory>,
    committed: bool,
}

impl Drop for ParentDirectory {
    fn drop(&mut self) {
        if !self.committed {
            for created in self.created.iter().rev() {
                let parent = self.directories.files[created.parent].as_raw_fd();
                // Never remove a concurrent replacement or a nonempty directory.
                if matches!(entry(parent, &created.name), Ok(Some(info)) if info.state.identity == created.identity)
                {
                    let _ = unlink_at(parent, &created.name, libc::AT_REMOVEDIR);
                }
            }
        }
        // Releasing final handles cannot turn committed publication into an
        // error after its rollback data has already been removed.
    }
}

fn parent_directory(
    binding: &OutputBinding,
    relative: &Path,
    hook: &mut Hook<'_>,
) -> Result<ParentDirectory> {
    let directories = root_directory(&binding.project, Some(&binding.ancestors))?;
    if pubspec(directories.last())? != binding.pubspec {
        return Err(config(
            "selected package pubspec changed during profile generation",
        ));
    }
    let mut parent = ParentDirectory {
        directories,
        parents: Vec::new(),
        created: Vec::new(),
        committed: false,
    };
    for (index, part) in relative.components().enumerate() {
        let component = part.as_os_str();
        hook(Event::new(Stage::BeforeMkdir))?;
        let c_name = name(component)?;
        let descriptor = parent.directories.last();
        // SAFETY: descriptor is owned by parent, c_name is terminated.
        if unsafe { libc::mkdirat(descriptor, c_name.as_ptr(), 0o755) } == 0 {
            if let Some(info) = entry(descriptor, component)? {
                parent.created.push(CreatedDirectory {
                    parent: parent.directories.files.len() - 1,
                    name: component.to_owned(),
                    identity: info.state.identity,
                });
            }
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        hook(Event::new(Stage::AfterMkdir))?;
        let opened = open_at(descriptor, component, DIRECTORY_FLAGS, 0)?;
        let identity = parent.directories.push(opened)?;
        if binding
            .parents
            .get(index)
            .is_some_and(|expected| *expected != identity)
        {
            return Err(config("profile output directory changed during generation"));
        }
        if entry(parent.directories.last(), OsStr::new("pubspec.yaml"))?.is_some() {
            return Err(config(
                "profile output must remain in the selected source package",
            ));
        }
        parent.parents.push(identity);
    }
    Ok(parent)
}

fn verify_namespace(binding: &OutputBinding, relative: &Path, parents: &[Identity]) -> Result<()> {
    let mut directories = root_directory(&binding.project, Some(&binding.ancestors))?;
    if pubspec(directories.last())? != binding.pubspec {
        return Err(config(
            "selected package pubspec changed during profile generation",
        ));
    }
    for (part, expected) in relative.components().zip(parents) {
        let opened = open_at(directories.last(), part.as_os_str(), DIRECTORY_FLAGS, 0)?;
        if directories.push(opened)? != *expected {
            return Err(config("profile output directory changed during generation"));
        }
        if entry(directories.last(), OsStr::new("pubspec.yaml"))?.is_some() {
            return Err(config(
                "profile output must remain in the selected source package",
            ));
        }
    }
    directories.close()
}

fn verify_destination(
    directory: RawFd,
    path: &OsStr,
    expected: Option<&Destination>,
    force: bool,
) -> Result<Option<Destination>> {
    let info = entry(directory, path)?;
    if info.as_ref().is_some_and(|info| !info.regular()) {
        return Err(config(
            "profile output must be a regular file, not a symlink or special file",
        ));
    }
    if info.is_some() && !force {
        return Err(config(
            "profile output already exists; use --force to replace it",
        ));
    }
    let current = destination(directory, path)?;
    if current.as_ref() != expected {
        return Err(config("profile destination changed during generation"));
    }
    Ok(current)
}

fn verify_document(
    directory: RawFd,
    path: &OsStr,
    identity: Identity,
    encoded: &[u8],
) -> Result<()> {
    let mut file = open_at(directory, path, READ_FLAGS, 0)
        .map_err(|_| config("generated profile staging file changed before publication"))?;
    let result = (|| {
        let info = file_entry(&file)?;
        if !info.regular()
            || info.state.identity != identity
            || info.state.size != encoded.len() as i64
        {
            return Err(config(
                "generated profile staging file changed before publication",
            ));
        }
        let mut actual = Vec::with_capacity(encoded.len());
        (&mut file)
            .take(encoded.len() as u64 + 1)
            .read_to_end(&mut actual)?;
        if actual != encoded {
            return Err(config(
                "generated profile staging file changed before publication",
            ));
        }
        Ok(())
    })();
    let closed = close(file);
    result.and(closed)
}

fn unlink_at(directory: RawFd, path: &OsStr, flags: i32) -> Result<()> {
    let path = name(path)?;
    // SAFETY: directory is live; path is a single terminated entry name.
    if unsafe { libc::unlinkat(directory, path.as_ptr(), flags) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn unlink_owned(directory: RawFd, path: &OsStr, identity: Identity) -> Result<()> {
    if entry(directory, path)?.is_some_and(|info| info.state.identity == identity) {
        unlink_at(directory, path, 0)?;
    }
    Ok(())
}

fn link_at(directory: RawFd, source: &OsStr, target: &OsStr) -> Result<()> {
    let source = name(source)?;
    let target = name(target)?;
    // SAFETY: descriptor and both terminated component names remain live.
    // Flags zero prevents following a source symlink; verification binds bytes.
    if unsafe { libc::linkat(directory, source.as_ptr(), directory, target.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn rename_at(directory: RawFd, source: &OsStr, target: &OsStr) -> Result<()> {
    let source = name(source)?;
    let target = name(target)?;
    // SAFETY: descriptor and both terminated component names remain live.
    if unsafe { libc::renameat(directory, source.as_ptr(), directory, target.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn rollback(
    directory: RawFd,
    target: &OsStr,
    identity: Identity,
    backup: Option<&OsStr>,
    previous: Option<&Destination>,
) -> Result<bool> {
    let info = entry(directory, target)?;
    if !info
        .as_ref()
        .is_some_and(|info| info.state.identity == identity)
    {
        // A concurrent replacement wins. Preserve it and retain the previous
        // backup if it is no longer reachable through the destination.
        let Some(previous) = previous.filter(|_| backup.is_some()) else {
            return Ok(true);
        };
        return Ok(info
            .as_ref()
            .is_some_and(|info| info.state.identity == previous.entry.state.identity)
            || same_previous(previous, destination(directory, target)?.as_ref()));
    }
    if let Some(backup) = backup {
        let previous = previous.expect("backup always belongs to a previous destination");
        if !same_previous(previous, destination(directory, backup)?.as_ref()) {
            return Err(config(
                "previous profile changed during publication rollback",
            ));
        }
        rename_at(directory, backup, target)?;
    } else {
        unlink_at(directory, target, 0)?;
    }
    Ok(true)
}

fn temporary_name(backup: bool) -> OsString {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    // Names are collision-resistant rather than secret. O_EXCL / linkat are
    // the ownership boundary even if another process predicts a candidate.
    let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        ".failforge-profile-{}{:x}-{time:x}-{serial:x}.tmp",
        if backup { "backup-" } else { "" },
        std::process::id()
    )
    .into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    BeforeMkdir,
    AfterMkdir,
    BeforeStageOpen,
    StagingOpened,
    BeforeWrite,
    AfterWrite,
    BeforeSync,
    AfterSync,
    AfterStageClose,
    AfterBackupLink,
    BeforeCommit,
    AfterCommit,
    BeforeStageCleanup,
    BeforeFinalVerify,
    BeforeBackupCleanup,
    BeforeRollback,
}

#[cfg_attr(not(test), allow(dead_code))]
struct Event<'a> {
    stage: Stage,
    temporary: Option<&'a OsStr>,
    backup: Option<&'a OsStr>,
}

impl Event<'_> {
    fn new(stage: Stage) -> Self {
        Self {
            stage,
            temporary: None,
            backup: None,
        }
    }
}

type Hook<'a> = dyn FnMut(Event<'_>) -> Result<()> + 'a;

/// Publish synced whole UTF-8 bytes atomically and verify the namespace before
/// and after publication. With `force`, retain a hard-link backup until every
/// final check succeeds so a failed publication can restore the old document.
pub fn write_profile(
    project: &Path,
    output: &Path,
    text: &str,
    force: bool,
    expected_binding: Option<&OutputBinding>,
) -> Result<()> {
    write_profile_with_hook(project, output, text, force, expected_binding, &mut |_| {
        Ok(())
    })
}

fn write_profile_with_hook(
    project: &Path,
    output: &Path,
    text: &str,
    force: bool,
    expected_binding: Option<&OutputBinding>,
    hook: &mut Hook<'_>,
) -> Result<()> {
    let (project, output, relative) = paths(project, output)?;
    let owned;
    let binding = match expected_binding {
        Some(binding) => binding,
        None => {
            owned = capture_output_binding(&project, &output)?;
            &owned
        }
    };
    if binding.project != project || binding.output != output {
        return Err(config(
            "profile publication binding does not match its destination",
        ));
    }
    let relative_parent = relative.parent().expect("relative file has parent");
    let target = relative.file_name().expect("relative file has name");
    let mut parent = parent_directory(binding, relative_parent, hook)?;
    let directory = parent.directories.last();
    let previous = verify_destination(directory, target, binding.destination.as_ref(), force)?;
    let mut expected_destination = previous.clone();
    let temporary = temporary_name(false);
    hook(Event {
        stage: Stage::BeforeStageOpen,
        temporary: Some(&temporary),
        backup: None,
    })?;
    let mut file = Some(open_at(
        directory,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
        0o644,
    )?);
    let mut identity = None;
    let mut backup = None;
    let mut retain_backup = false;
    let encoded = text.as_bytes();
    let mut result = (|| {
        let opened = file.as_mut().expect("staging descriptor is open");
        identity = Some(file_entry(opened)?.state.identity);
        hook(Event {
            stage: Stage::StagingOpened,
            temporary: Some(&temporary),
            backup: None,
        })?;
        if let Some(previous) = &previous {
            // SAFETY: opened owns this descriptor; keep all original permission bits.
            if unsafe {
                libc::fchmod(
                    opened.as_raw_fd(),
                    (previous.entry.state.mode & 0o7777) as libc::mode_t,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        hook(Event::new(Stage::BeforeWrite))?;
        opened.write_all(encoded)?;
        hook(Event::new(Stage::AfterWrite))?;
        opened.flush()?;
        hook(Event::new(Stage::BeforeSync))?;
        opened.sync_all()?;
        hook(Event {
            stage: Stage::AfterSync,
            temporary: Some(&temporary),
            backup: None,
        })?;
        close(file.take().expect("staging descriptor is open"))?;
        hook(Event::new(Stage::AfterStageClose))?;
        let identity = identity.expect("staging identity is bound");
        verify_namespace(binding, relative_parent, &parent.parents)?;
        verify_destination(directory, target, expected_destination.as_ref(), force)?;
        verify_document(directory, &temporary, identity, encoded)?;
        if let Some(previous) = &previous {
            let backup_name = temporary_name(true);
            link_at(directory, target, &backup_name)?;
            backup = Some(backup_name);
            hook(Event {
                stage: Stage::AfterBackupLink,
                temporary: Some(&temporary),
                backup: backup.as_deref(),
            })?;
            let retained = destination(directory, backup.as_deref().expect("backup exists"))?;
            expected_destination = destination(directory, target)?;
            if !same_previous(previous, retained.as_ref())
                || !same_previous(previous, expected_destination.as_ref())
            {
                return Err(config("previous profile changed before publication"));
            }
        }
        verify_namespace(binding, relative_parent, &parent.parents)?;
        verify_destination(directory, target, expected_destination.as_ref(), force)?;
        verify_document(directory, &temporary, identity, encoded)?;
        hook(Event {
            stage: Stage::BeforeCommit,
            temporary: Some(&temporary),
            backup: backup.as_deref(),
        })?;
        if force {
            rename_at(directory, &temporary, target)?;
        } else {
            match link_at(directory, &temporary, target) {
                Err(ProfileError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    return Err(config(
                        "profile output already exists; use --force to replace it",
                    ));
                }
                other => other?,
            }
        }
        hook(Event {
            stage: Stage::AfterCommit,
            temporary: Some(&temporary),
            backup: backup.as_deref(),
        })?;
        verify_namespace(binding, relative_parent, &parent.parents)?;
        verify_document(directory, target, identity, encoded)?;
        hook(Event::new(Stage::BeforeStageCleanup))?;
        unlink_owned(directory, &temporary, identity)?;
        hook(Event::new(Stage::BeforeFinalVerify))?;
        verify_namespace(binding, relative_parent, &parent.parents)?;
        if let Some(backup) = &backup {
            hook(Event::new(Stage::BeforeBackupCleanup))?;
            unlink_owned(
                directory,
                backup,
                previous
                    .as_ref()
                    .expect("previous destination exists")
                    .entry
                    .state
                    .identity,
            )?;
        }
        Ok(())
    })();
    // If the initial identity query failed, retry while the owned descriptor is
    // still open. Never infer ownership from a concurrently replaceable name.
    if identity.is_none() {
        identity = file
            .as_ref()
            .and_then(|file| file_entry(file).ok())
            .map(|entry| entry.state.identity);
    }
    drop(file);
    if let Some(identity) = identity.filter(|_| result.is_err()) {
        match hook(Event::new(Stage::BeforeRollback)).and_then(|()| {
            rollback(
                directory,
                target,
                identity,
                backup.as_deref(),
                previous.as_ref(),
            )
        }) {
            Ok(restored) => retain_backup = !restored,
            Err(error) => {
                retain_backup = true;
                result = Err(error);
            }
        }
    }
    if let Err(error) = identity.map_or(Ok(()), |identity| {
        unlink_owned(directory, &temporary, identity)
    }) {
        result = Err(error);
    }
    let backup_cleanup = match (result.is_err() && !retain_backup, backup.as_ref()) {
        (true, Some(backup)) => unlink_owned(
            directory,
            backup,
            previous
                .as_ref()
                .expect("previous destination exists")
                .entry
                .state
                .identity,
        ),
        _ => Ok(()),
    };
    if let Err(error) = backup_cleanup {
        result = Err(error);
    }
    parent.committed = result.is_ok();
    result
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
