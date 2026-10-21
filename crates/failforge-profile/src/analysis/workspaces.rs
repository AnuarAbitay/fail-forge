//! Private leased snapshots. Recovery uses opened directories and never follows
//! payload symlinks or trusts a recorded PID/path when deleting stale work.
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use failforge_runner::WORKSPACE_LEASE_MAGIC;

use super::failure;
use crate::ProfileError;

const DIRECTORY_FLAGS: i32 =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
const CLEANUP_LOCK_BUDGET: Duration = Duration::from_secs(1);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(5);

fn name(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid workspace entry"))
}

fn open(parent: RawFd, path: &OsStr, flags: i32) -> io::Result<File> {
    let path = name(path)?;
    // SAFETY: path is a live C string; openat returns a new owned descriptor.
    let descriptor = unsafe { libc::openat(parent, path.as_ptr(), flags, 0o600) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this successful openat descriptor has no other Rust owner.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn uid() -> u32 {
    // SAFETY: geteuid has no pointers or side effects.
    unsafe { libc::geteuid() }
}

fn directory(parent: RawFd, path: &OsStr, private: bool) -> Result<File, ProfileError> {
    let file = open(parent, path, DIRECTORY_FLAGS)?;
    let info = file.metadata()?;
    if !info.is_dir() || info.uid() != uid() || private && info.mode() & 0o7777 != 0o700 {
        return Err(failure(
            "invalid workspace directory ownership or permissions",
        ));
    }
    Ok(file)
}

fn regular(parent: RawFd, path: &OsStr, create: bool) -> Result<File, ProfileError> {
    let flags = libc::O_RDWR
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | libc::O_CLOEXEC
        | if create {
            libc::O_CREAT | libc::O_EXCL
        } else {
            0
        };
    let file = open(parent, path, flags)?;
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != uid() || info.nlink() != 1 || info.mode() & 0o7777 != 0o600
    {
        return Err(failure("invalid workspace control file"));
    }
    Ok(file)
}

fn marker(file: &File, expected: &[u8], partial: bool) -> Result<(), ProfileError> {
    let mut actual = vec![0; expected.len() + 1];
    let count = file.read_at(&mut actual, 0)?;
    actual.truncate(count);
    if if partial {
        !expected.starts_with(&actual)
    } else {
        actual != expected
    } {
        return Err(failure("invalid workspace ownership marker"));
    }
    Ok(())
}

fn write_marker(mut file: &File, bytes: &[u8]) -> Result<(), ProfileError> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn try_lock(file: &File) -> io::Result<bool> {
    // SAFETY: flock borrows a live file descriptor and takes integer flags.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

fn wait_for_lock(attempt: impl FnMut() -> io::Result<bool>, deadline: Instant) -> io::Result<bool> {
    wait_for_lock_with_clock(attempt, deadline, Instant::now, std::thread::sleep)
}

fn wait_for_lock_with_clock(
    mut attempt: impl FnMut() -> io::Result<bool>,
    deadline: Instant,
    mut now: impl FnMut() -> Instant,
    mut sleep: impl FnMut(Duration),
) -> io::Result<bool> {
    loop {
        match attempt() {
            Ok(true) => return Ok(true),
            Ok(false) => (),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(error) => return Err(error),
        }
        let Some(remaining) = deadline.checked_duration_since(now()) else {
            return Ok(false);
        };
        sleep(remaining.min(LOCK_RETRY_INTERVAL));
    }
}

fn lock_until(file: &File, deadline: Instant) -> io::Result<bool> {
    wait_for_lock(|| try_lock(file), deadline)
}

fn global_lock(root: &File, deadline: Instant) -> Result<File, ProfileError> {
    let global = regular(root.as_raw_fd(), OsStr::new(".global.lock"), false)?;
    marker(&global, b"", false)?;
    if !lock_until(&global, deadline)? {
        return Err(failure(
            "workspace registry still has an active cleanup owner",
        ));
    }
    same_entry(root, OsStr::new(".global.lock"), &global)?;
    marker(&global, b"", false)?;
    Ok(global)
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    // Open a new description, so readdir's offset never affects the retained fd.
    let copy = open(directory.as_raw_fd(), OsStr::new("."), DIRECTORY_FLAGS)?;
    use std::os::fd::IntoRawFd;
    let descriptor = copy.into_raw_fd();
    // SAFETY: descriptor refers to a live directory; successful fdopendir owns it.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        // SAFETY: failed fdopendir does not own the descriptor.
        unsafe {
            libc::close(descriptor);
        }
        return Err(io::Error::last_os_error());
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: this guard uniquely owns the successful fdopendir result.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let guard = Stream(stream);
    let mut result = vec![];
    loop {
        // POSIX distinguishes EOF from an iteration error through errno.
        // SAFETY: errno is thread-local writable integer storage.
        unsafe {
            *errno_pointer() = 0;
        }
        // SAFETY: readdir returns borrowed storage valid until the next call.
        let entry = unsafe { libc::readdir(guard.0) };
        if entry.is_null() {
            // SAFETY: errno_pointer returns this thread's errno storage.
            if unsafe { *errno_pointer() } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        // SAFETY: POSIX dirent.d_name is a terminated C string.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes != b"." && bytes != b".." {
            result.push(OsString::from_vec(bytes.to_vec()));
        }
    }
    result.sort();
    Ok(result)
}

#[cfg(target_os = "macos")]
unsafe fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __error returns thread-local errno storage.
    unsafe { libc::__error() }
}

#[cfg(not(target_os = "macos"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    // SAFETY: __errno_location returns thread-local errno storage on Linux.
    unsafe { libc::__errno_location() }
}

fn unlink(parent: &File, path: &OsStr, directory: bool) -> io::Result<()> {
    let path = name(path)?;
    // SAFETY: parent is retained, name is a live C string, no follow is performed.
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

fn stat(parent: &File, path: &OsStr) -> io::Result<libc::stat> {
    let path = name(path)?;
    let mut info = std::mem::MaybeUninit::uninit();
    // SAFETY: info is writable output storage, the path is terminated.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            path.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat initialized the structure on success.
    Ok(unsafe { info.assume_init() })
}

fn same_entry(parent: &File, path: &OsStr, opened: &File) -> Result<(), ProfileError> {
    let current = stat(parent, path)?;
    let actual = opened.metadata()?;
    if current.st_dev as u64 != actual.dev() || current.st_ino != actual.ino() {
        return Err(failure("workspace directory changed during cleanup"));
    }
    Ok(())
}

fn same_registry(path: &Path, root: &File) -> Result<(), ProfileError> {
    let opened = root.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if !current.is_dir() || current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Err(failure("workspace registry changed during analysis"));
    }
    Ok(())
}

fn verify_coordination(
    root: &File,
    registry: &Path,
    (global, owner): (&File, &File),
) -> Result<(), ProfileError> {
    same_registry(registry, root)?;
    same_entry(root, OsStr::new(".global.lock"), global)?;
    same_entry(root, OsStr::new(".owner"), owner)?;
    marker(global, b"", false)?;
    marker(owner, &owner_marker(), false)
}

fn tree(directory_file: &File, remove: bool, depth: usize) -> Result<(), ProfileError> {
    if depth > 128 {
        return Err(failure("workspace tree exceeds supported depth"));
    }
    for name in names(directory_file)? {
        let info = stat(directory_file, &name)?;
        if info.st_uid != uid() {
            return Err(failure("workspace payload has another owner"));
        }
        if info.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = directory(directory_file.as_raw_fd(), &name, false)?;
            tree(&child, remove, depth + 1)?;
            same_entry(directory_file, &name, &child)?;
            if remove {
                unlink(directory_file, &name, true)?;
            }
        } else if remove {
            unlink(directory_file, &name, false)?;
        }
    }
    Ok(())
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn mkdir(parent: &File, path: &str) -> io::Result<()> {
    let path = CString::new(path).expect("fixed workspace name");
    // SAFETY: parent is live; path is terminated; mode is a plain integer.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), path.as_ptr(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn rename(parent: &File, from: &str, to: &str) -> io::Result<()> {
    let from = CString::new(from).expect("workspace name");
    let to = CString::new(to).expect("workspace name");
    // SAFETY: both paths are live terminated strings under the retained parent.
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
    parent.sync_all()
}

fn owner_marker() -> Vec<u8> {
    format!("FAILFORGE_NATIVE_PROFILE_WORKSPACES_V1:{}\n", uid()).into_bytes()
}
fn campaign_marker(token: &str) -> Vec<u8> {
    format!("FAILFORGE_NATIVE_PROFILE_SNAPSHOT_V1:{}:{token}\n", uid()).into_bytes()
}

pub(super) struct Workspace {
    registry: PathBuf,
    root: File,
    campaign_name: String,
    lease: Option<File>,
    lease_identity: (u64, u64),
    pub path: PathBuf,
    closed: bool,
    cleanup_attempted: bool,
}

/// Identity-only handles do not hold a flock. They keep substituted objects
/// distinguishable while registry coordination is unavailable.
struct CleanupIdentity {
    campaign: File,
    owner: File,
    entries: Vec<(&'static str, File)>,
}

impl CleanupIdentity {
    fn capture(
        root: &File,
        campaign_name: &str,
        lease_identity: (u64, u64),
    ) -> Result<Self, ProfileError> {
        let campaign = directory(root.as_raw_fd(), OsStr::new(campaign_name), true)?;
        let lease = regular(campaign.as_raw_fd(), OsStr::new("lease"), false)?;
        let info = lease.metadata()?;
        if (info.dev(), info.ino()) != lease_identity {
            return Err(failure("workspace lease changed before cleanup"));
        }
        let owner = regular(root.as_raw_fd(), OsStr::new(".owner"), false)?;
        let metadata = regular(campaign.as_raw_fd(), OsStr::new("metadata"), false)?;
        let work = directory(campaign.as_raw_fd(), OsStr::new("work"), true)?;
        Ok(Self {
            campaign,
            owner,
            entries: vec![("lease", lease), ("metadata", metadata), ("work", work)],
        })
    }

    fn verify(&self, root: &File, campaign_name: &str) -> Result<(), ProfileError> {
        same_entry(root, OsStr::new(campaign_name), &self.campaign)?;
        same_entry(root, OsStr::new(".owner"), &self.owner)?;
        marker(&self.owner, &owner_marker(), false)?;
        for (name, file) in &self.entries {
            same_entry(&self.campaign, OsStr::new(name), file)?;
        }
        Ok(())
    }
}

/// A cleanup capability retained by a detached native owner after its calling
/// future was dropped. It contains directory identity, never a metadata path.
pub(super) struct Recovery {
    registry: PathBuf,
    root: File,
    campaign_name: String,
    lease_identity: (u64, u64),
}

impl Recovery {
    pub fn recover(self) -> Result<(), ProfileError> {
        let deadline = Instant::now() + CLEANUP_LOCK_BUDGET;
        let identity =
            match CleanupIdentity::capture(&self.root, &self.campaign_name, self.lease_identity) {
                Ok(identity) => Some(identity),
                Err(ProfileError::Io(error)) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
        let global = global_lock(&self.root, deadline)?;
        same_registry(&self.registry, &self.root)?;
        let owner = regular(self.root.as_raw_fd(), OsStr::new(".owner"), false)?;
        marker(&owner, &owner_marker(), false)?;
        // Another legitimate reaper may have finished while we waited. Only a
        // missing campaign is idempotent; missing/replaced controls are errors.
        match stat(&self.root, OsStr::new(&self.campaign_name)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(_) => (),
        }
        identity
            .ok_or_else(|| failure("workspace changed before recovery"))?
            .verify(&self.root, &self.campaign_name)?;
        reap(
            &self.root,
            &self.registry,
            &self.campaign_name,
            Some(deadline),
            (&global, &owner),
        )
    }
}

impl Workspace {
    pub fn new(protected: &[PathBuf]) -> Result<Self, ProfileError> {
        let registry = std::env::temp_dir()
            .canonicalize()?
            .join(format!("failforge-profile-workspaces-{}", uid()));
        Self::at(registry, protected)
    }

    fn at(registry: PathBuf, protected: &[PathBuf]) -> Result<Self, ProfileError> {
        let deadline = Instant::now() + CLEANUP_LOCK_BUDGET;
        if protected
            .iter()
            .any(|path| registry.starts_with(path) || path.starts_with(&registry))
        {
            return Err(failure(
                "temporary workspace registry must be outside analysis inputs",
            ));
        }
        match fs::DirBuilder::new().mode(0o700).create(&registry) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let root = directory(libc::AT_FDCWD, registry.as_os_str(), true)?;
        let entries = names(&root)?;
        if entries.iter().any(|entry| entry == ".owner")
            && !entries.iter().any(|entry| entry == ".global.lock")
        {
            return Err(failure("workspace registry global lock is missing"));
        }
        let global = match regular(root.as_raw_fd(), OsStr::new(".global.lock"), true) {
            Ok(file) => file,
            Err(ProfileError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                regular(root.as_raw_fd(), OsStr::new(".global.lock"), false)?
            }
            Err(error) => return Err(error),
        };
        marker(&global, b"", false)?;
        if !lock_until(&global, deadline)? {
            return Err(failure(
                "workspace registry still has an active cleanup owner",
            ));
        }
        same_entry(&root, OsStr::new(".global.lock"), &global)?;
        marker(&global, b"", false)?;
        same_registry(&registry, &root)?;
        let owner = owner_marker();
        match regular(root.as_raw_fd(), OsStr::new(".owner"), false) {
            Ok(file) => marker(&file, &owner, false)?,
            Err(ProfileError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                for entry in names(&root)? {
                    if entry == ".global.lock" {
                        continue;
                    }
                    let text = entry
                        .to_str()
                        .ok_or_else(|| failure("unrecognized workspace entry"))?;
                    if !text
                        .strip_prefix(".owner.pending-")
                        .is_some_and(valid_token)
                    {
                        return Err(failure("unrecognized uninitialized workspace registry"));
                    }
                    marker(&regular(root.as_raw_fd(), &entry, false)?, &owner, true)?;
                    unlink(&root, &entry, false)?;
                }
                let pending = format!(".owner.pending-{}", random_token()?);
                let file = regular(root.as_raw_fd(), OsStr::new(&pending), true)?;
                write_marker(&file, &owner)?;
                rename(&root, &pending, ".owner")?;
            }
            Err(error) => return Err(error),
        }
        let owner = regular(root.as_raw_fd(), OsStr::new(".owner"), false)?;
        marker(&owner, &owner_marker(), false)?;
        for entry in names(&root)? {
            if let Some(text) = entry.to_str() {
                let _ = reap(&root, &registry, text, None, (&global, &owner));
            }
        }
        verify_coordination(&root, &registry, (&global, &owner))?;
        let token = random_token()?;
        let pending = format!("pending-{token}");
        let campaign_name = format!("campaign-{token}");
        mkdir(&root, &pending)?;
        let campaign = directory(root.as_raw_fd(), OsStr::new(&pending), true)?;
        let lease = regular(campaign.as_raw_fd(), OsStr::new("lease"), true)?;
        if !try_lock(&lease)? {
            return Err(failure("new workspace lease is already locked"));
        }
        write_marker(&lease, WORKSPACE_LEASE_MAGIC)?;
        write_marker(
            &regular(campaign.as_raw_fd(), OsStr::new("metadata"), true)?,
            &campaign_marker(&token),
        )?;
        mkdir(&campaign, "work")?;
        campaign.sync_all()?;
        rename(&root, &pending, &campaign_name)?;
        same_registry(&registry, &root)?;
        let path = registry.join(&campaign_name).join("work");
        let lease_info = lease.metadata()?;
        drop(global);
        Ok(Self {
            registry,
            root,
            campaign_name,
            lease: Some(lease),
            lease_identity: (lease_info.dev(), lease_info.ino()),
            path,
            closed: false,
            cleanup_attempted: false,
        })
    }

    pub fn lease_fd(&self) -> RawFd {
        self.lease
            .as_ref()
            .expect("live workspace lease")
            .as_raw_fd()
    }

    pub fn recovery(&self) -> Result<Recovery, ProfileError> {
        Ok(Recovery {
            registry: self.registry.clone(),
            root: self.root.try_clone()?,
            campaign_name: self.campaign_name.clone(),
            lease_identity: self.lease_identity,
        })
    }

    pub fn close(&mut self) -> Result<(), ProfileError> {
        self.cleanup_attempted = true;
        self.close_until(Instant::now() + CLEANUP_LOCK_BUDGET)
    }

    fn close_until(&mut self, deadline: Instant) -> Result<(), ProfileError> {
        if self.closed {
            return Ok(());
        }
        let identity =
            CleanupIdentity::capture(&self.root, &self.campaign_name, self.lease_identity)?;
        let global = global_lock(&self.root, deadline)?;
        let owner = regular(self.root.as_raw_fd(), OsStr::new(".owner"), false)?;
        marker(&owner, &owner_marker(), false)?;
        same_registry(&self.registry, &self.root)?;
        identity.verify(&self.root, &self.campaign_name)?;
        self.lease.take(); // Never LOCK_UN the helper's inherited open description.
        // A concurrent fork can retain CLOEXEC descriptors until exec. Retry
        // only within the shared cleanup budget; a real owner remains protected.
        // This bounds lock waiting, not the filesystem work needed for deletion.
        reap(
            &self.root,
            &self.registry,
            &self.campaign_name,
            Some(deadline),
            (&global, &owner),
        )?;
        if self.registry.join(&self.campaign_name).try_exists()? {
            return Err(failure("applicability workspace cleanup did not complete"));
        }
        self.closed = true;
        Ok(())
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        // A failed explicit close has already spent its waiting budget. Drop
        // still attempts immediate cleanup, but cannot silently wait again.
        let deadline = Instant::now()
            + if self.cleanup_attempted {
                Duration::ZERO
            } else {
                CLEANUP_LOCK_BUDGET
            };
        let _ = self.close_until(deadline);
    }
}

fn valid_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn reap(
    root: &File,
    registry: &Path,
    name: &str,
    deadline: Option<Instant>,
    coordination: (&File, &File),
) -> Result<(), ProfileError> {
    let (pending, token) = if let Some(token) = name.strip_prefix("pending-") {
        (true, token)
    } else if let Some(token) = name.strip_prefix("campaign-") {
        (false, token)
    } else {
        return Err(failure("unrecognized workspace entry"));
    };
    if !valid_token(token) {
        return Err(failure("unrecognized workspace entry"));
    }
    let campaign = directory(root.as_raw_fd(), OsStr::new(name), true)?;
    let entries = names(&campaign)?;
    if entries
        .iter()
        .any(|entry| entry != "lease" && entry != "metadata" && entry != "work")
    {
        return Err(failure("unrecognized campaign entry"));
    }
    let lease = match regular(campaign.as_raw_fd(), OsStr::new("lease"), false) {
        Ok(file) => file,
        Err(ProfileError::Io(error)) if pending && error.kind() == io::ErrorKind::NotFound => {
            regular(campaign.as_raw_fd(), OsStr::new("lease"), true)?
        }
        Err(error) => return Err(error),
    };
    // Retain all directory/control identities before any waiting, so replacing
    // a workspace while its old lease drains cannot authorize deleting the new
    // object. None of the payload is inspected or changed while the lease is held.
    let metadata = entries
        .iter()
        .any(|entry| entry == "metadata")
        .then(|| regular(campaign.as_raw_fd(), OsStr::new("metadata"), false))
        .transpose()?;
    let work = entries
        .iter()
        .any(|entry| entry == "work")
        .then(|| directory(campaign.as_raw_fd(), OsStr::new("work"), true))
        .transpose()?;
    let acquired = match deadline {
        Some(deadline) => lock_until(&lease, deadline)?,
        None => try_lock(&lease)?,
    };
    if !acquired {
        return Err(failure("workspace still has an active cleanup owner"));
    }
    // Waiting on the campaign lease must not outlive our authorization: both
    // coordination files must still name the exact retained control objects.
    verify_coordination(root, registry, coordination)?;
    same_entry(root, OsStr::new(name), &campaign)?;
    same_entry(&campaign, OsStr::new("lease"), &lease)?;
    marker(&lease, WORKSPACE_LEASE_MAGIC, pending)?;
    if let Some(metadata) = metadata {
        same_entry(&campaign, OsStr::new("metadata"), &metadata)?;
        marker(&metadata, &campaign_marker(token), pending)?;
    } else if !pending {
        return Err(failure("workspace metadata is missing"));
    }
    if let Some(work) = work {
        same_entry(&campaign, OsStr::new("work"), &work)?;
        if pending && !names(&work)?.is_empty() {
            return Err(failure("uncommitted workspace is not empty"));
        }
        tree(&work, false, 0)?;
        tree(&work, true, 0)?;
        same_entry(&campaign, OsStr::new("work"), &work)?;
        work.sync_all()?;
    } else if !pending {
        return Err(failure("workspace work directory is missing"));
    }
    let cleanup = format!("pending-{token}");
    if !pending {
        if stat(root, OsStr::new(&cleanup)).is_ok() {
            return Err(failure("duplicate workspace token"));
        }
        rename(root, name, &cleanup)?;
    }
    remove_pending(&campaign, token)?;
    same_entry(root, OsStr::new(&cleanup), &campaign)?;
    unlink(root, OsStr::new(&cleanup), true)?;
    root.sync_all()?;
    Ok(())
}

fn remove_pending(campaign: &File, token: &str) -> Result<(), ProfileError> {
    let entries = names(campaign)?;
    if entries
        .iter()
        .any(|entry| entry != "lease" && entry != "metadata" && entry != "work")
    {
        return Err(failure("unrecognized campaign entry during cleanup"));
    }
    if entries.iter().any(|entry| entry == "work") {
        let work = directory(campaign.as_raw_fd(), OsStr::new("work"), true)?;
        if !names(&work)?.is_empty() {
            return Err(failure("workspace is not empty after cleanup"));
        }
        same_entry(campaign, OsStr::new("work"), &work)?;
        unlink(campaign, OsStr::new("work"), true)?;
    }
    for (name, expected) in [
        ("metadata", campaign_marker(token)),
        ("lease", WORKSPACE_LEASE_MAGIC.to_vec()),
    ] {
        if entries.iter().any(|entry| entry == name) {
            let file = regular(campaign.as_raw_fd(), OsStr::new(name), false)?;
            marker(&file, &expected, true)?;
            same_entry(campaign, OsStr::new(name), &file)?;
            unlink(campaign, OsStr::new(name), false)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn registry() -> PathBuf {
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-profile-registry-test-{}",
            random_token().unwrap()
        ))
    }

    #[test]
    fn active_leases_are_not_reaped_and_stale_payload_is_recovered() {
        let root = registry();
        let mut first = Workspace::at(root.clone(), &[]).unwrap();
        let first_path = first.path.clone();
        fs::write(first.path.join("source"), "data").unwrap();
        let mut second = Workspace::at(root.clone(), &[]).unwrap();
        assert!(first_path.exists());
        second.close().unwrap();
        // Simulate controller death without running Drop cleanup.
        let recovery = first.recovery().unwrap();
        first.lease.take();
        first.closed = true;
        let mut third = Workspace::at(root.clone(), &[]).unwrap();
        // Startup skips leased campaigns, including a transient descriptor
        // inherited by another test's pre-exec child. Deferred recovery waits.
        if first_path.exists() {
            assert_eq!(fs::read(first_path.join("source")).unwrap(), b"data");
        }
        recovery.recover().unwrap();
        assert!(!first_path.exists());
        third.close().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_unlinks_payload_alias_without_touching_target() {
        let root = registry();
        let target = registry();
        fs::create_dir(&target).unwrap();
        fs::write(target.join("valuable"), "keep").unwrap();
        let mut work = Workspace::at(root.clone(), &[]).unwrap();
        symlink(&target, work.path.join("alias")).unwrap();
        work.close().unwrap();
        assert_eq!(fs::read_to_string(target.join("valuable")).unwrap(), "keep");
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn unmarked_or_symlink_registry_is_never_reaped() {
        let root = registry();
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        fs::write(root.join("valuable"), "keep").unwrap();
        assert!(Workspace::at(root.clone(), &[]).is_err());
        assert!(root.join("valuable").exists());
        let alias = registry();
        symlink(&root, &alias).unwrap();
        assert!(Workspace::at(alias.clone(), &[]).is_err());
        fs::remove_file(alias).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_cleanup_preserves_new_unknown_entries_and_replaced_registry() {
        let root = registry();
        let mut work = Workspace::at(root.clone(), &[]).unwrap();
        let campaign_path = work.path.parent().unwrap();
        let campaign = directory(libc::AT_FDCWD, campaign_path.as_os_str(), true).unwrap();
        let token = work.campaign_name.strip_prefix("campaign-").unwrap();
        fs::write(campaign_path.join("valuable"), "keep").unwrap();
        assert!(remove_pending(&campaign, token).is_err());
        assert_eq!(
            fs::read_to_string(campaign_path.join("valuable")).unwrap(),
            "keep"
        );
        fs::remove_file(campaign_path.join("valuable")).unwrap();
        let moved = registry();
        fs::rename(&root, &moved).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        assert!(work.close().is_err());
        assert!(same_registry(&root, &work.root).is_err());
        work.closed = true;
        drop(work);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn registry_cannot_contain_or_be_contained_by_inputs() {
        let root = registry();
        assert!(Workspace::at(root.clone(), &[root.join("old-campaign/project")]).is_err());
        assert!(!root.exists());
        assert!(Workspace::at(root.clone(), &[root.parent().unwrap().to_path_buf()]).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn missing_or_nonempty_global_lock_never_recreates_coordination() {
        let root = registry();
        let mut work = Workspace::at(root.clone(), &[]).unwrap();
        work.close().unwrap();
        fs::remove_file(root.join(".global.lock")).unwrap();
        assert!(Workspace::at(root.clone(), &[]).is_err());
        assert!(!root.join(".global.lock").exists());
        let control = regular(work.root.as_raw_fd(), OsStr::new(".global.lock"), true).unwrap();
        write_marker(&control, b"unexpected").unwrap();
        assert!(Workspace::at(root.clone(), &[]).is_err());
        assert_eq!(fs::read(root.join(".global.lock")).unwrap(), b"unexpected");
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[path = "workspaces_fork_tests.rs"]
mod fork_tests;
