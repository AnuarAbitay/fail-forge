//! Windows campaign registry: handle-relative traversal, protected control ACLs,
//! shared kernel leases for live owners, and exclusive stale-campaign recovery.
use crate::windows_fs as win;
use crate::{Cancellation, InputError, failure};
use failforge_runner::{WORKSPACE_LEASE_LOCK_OFFSET, WORKSPACE_LEASE_MAGIC};
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::fs::FileExt;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const LEASE_MAGIC: &[u8] = WORKSPACE_LEASE_MAGIC;
const GLOBAL: &str = ".global.lock";
const OWNER: &str = ".owner";
const LEASE: &str = "lease";
const METADATA: &str = "metadata.json";
const WORK: &str = "work";
const GLOBAL_LOCK_BUDGET: Duration = Duration::from_secs(2);
const GLOBAL_LOCK_RETRY: Duration = Duration::from_millis(10);

/// A proven shared kernel lease. All controller clones retain its original
/// handle; separate worker processes acquire their own shared runner lease.
#[derive(Clone, Debug)]
pub struct WindowsLease(Arc<File>);
impl WindowsLease {
    fn acquire(file: File) -> Result<Self, InputError> {
        require(
            win::try_lock_at(&file, true, WORKSPACE_LEASE_LOCK_OFFSET)?,
            "invalid_workspace_lease",
        )?;
        Ok(Self(Arc::new(file)))
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }
}
impl std::ops::Deref for WindowsLease {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl AsRawHandle for WindowsLease {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.as_raw_handle()
    }
}
impl AsHandle for WindowsLease {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}
#[derive(Debug)]
pub struct LifecycleCaps {
    pub root: File,
    pub campaign: File,
    pub work: File,
    pub lease: Option<WindowsLease>,
}
impl LifecycleCaps {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            root: self.root.try_clone()?,
            campaign: self.campaign.try_clone()?,
            work: self.work.try_clone()?,
            lease: self.lease.clone(),
        })
    }
    pub fn verify(
        &self,
        root: &Path,
        campaign: &str,
        cancel: &Cancellation,
    ) -> Result<(), InputError> {
        drop(validate_caps(root, campaign, self, cancel)?);
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Skip {
    pub name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_bytes: Option<Vec<u8>>,
}
#[derive(Debug)]
pub struct StartResult {
    pub caps: LifecycleCaps,
    pub root_path: PathBuf,
    pub campaign_name: String,
    pub recovered: Vec<String>,
    pub skipped: Vec<Skip>,
}
#[derive(Debug, Serialize)]
pub struct Allocation {
    pub name: String,
    pub path: PathBuf,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseReport {
    pub cleanup_deferred: bool,
    pub skipped: Vec<Skip>,
}
fn require(condition: bool, reason: &'static str) -> Result<(), InputError> {
    if condition {
        Ok(())
    } else {
        Err(failure(reason))
    }
}
fn marker(kind: &str, token: Option<&str>) -> Result<Vec<u8>, InputError> {
    // This Windows protocol records the full account SID; numeric Unix UID
    // values are not account identities on Windows.
    let value = match token {
        Some(token) => {
            serde_json::json!({"kind":kind,"version":1,"sid":win::current_sid()?,"token":token})
        }
        None => serde_json::json!({"kind":kind,"version":1,"sid":win::current_sid()?}),
    };
    let mut bytes = serde_json::to_vec(&value).map_err(|_| failure("invalid_marker"))?;
    bytes.push(b'\n');
    Ok(bytes)
}
fn token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn campaign_parts(value: &str) -> Option<(bool, &str)> {
    let (pending, tail) = if let Some(tail) = value.strip_prefix("campaign-") {
        (false, tail)
    } else {
        (true, value.strip_prefix("pending-")?)
    };
    token(tail).then_some((pending, tail))
}
fn temporary_name(value: &str) -> bool {
    value
        .strip_prefix("snapshot-")
        .or_else(|| value.strip_prefix("attempt-"))
        .is_some_and(token)
}
fn regular(parent: &File, name: &str, writable: bool, create: bool) -> Result<File, InputError> {
    let file = win::regular_at(parent, OsStr::new(name), writable, create)?;
    win::require_private(&file, false)?;
    Ok(file)
}
fn directory(parent: &File, name: &str) -> Result<File, InputError> {
    let file = win::directory_at(parent, OsStr::new(name))?;
    win::require_private(&file, true)?;
    Ok(file)
}
fn read_marker(file: &File, expected: &[u8], partial: bool) -> Result<(), InputError> {
    let length = file.metadata()?.len();
    require(length <= expected.len() as u64, "invalid_marker")?;
    let mut bytes = vec![0; expected.len()];
    let count = file.seek_read(&mut bytes, 0)?;
    bytes.truncate(count);
    require(
        if partial {
            expected.starts_with(&bytes)
        } else {
            expected == bytes
        },
        "invalid_marker",
    )
}
fn write_marker(mut file: &File, bytes: &[u8]) -> Result<(), InputError> {
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn owner(root: &File) -> Result<(), InputError> {
    read_marker(
        &regular(root, OWNER, false, false)?,
        &marker("failforge-workspace-registry", None)?,
        false,
    )
}
fn same_root(path: &Path, file: &File) -> Result<(), InputError> {
    require(
        win::identity(&win::directory(path)?)? == win::identity(file)?,
        "registry_changed",
    )
}
fn global_lock(path: &Path, root: &File, cancel: &Cancellation) -> Result<File, InputError> {
    cancel.check()?;
    let guard = regular(root, GLOBAL, true, false)?;
    read_marker(&guard, b"", false)?;
    let deadline = Instant::now() + GLOBAL_LOCK_BUDGET;
    while !win::try_lock(&guard)? {
        cancel.check()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        require(!remaining.is_zero(), "workspace_registry_busy")?;
        std::thread::sleep(remaining.min(GLOBAL_LOCK_RETRY));
    }
    cancel.check()?;
    win::require_private(root, true)?;
    same_root(path, root)?;
    Ok(guard)
}
fn initialize(path: &Path, cancel: &Cancellation) -> Result<File, InputError> {
    cancel.check()?;
    let parent = win::directory(
        path.parent()
            .ok_or_else(|| failure("workspace_root_has_no_parent"))?,
    )?;
    let name = path
        .file_name()
        .ok_or_else(|| failure("workspace_root_has_no_name"))?;
    let root = match win::create_directory_at(&parent, name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            win::directory_at(&parent, name)?
        }
        Err(error) => return Err(error.into()),
    };
    win::require_private(&root, true)?;
    let entries = win::names(&root)?;
    if !entries.iter().any(|name| name == GLOBAL) {
        require(
            !entries.iter().any(|name| name == OWNER),
            "missing_global_lock",
        )?;
        match regular(&root, GLOBAL, true, true) {
            Ok(_) => (),
            Err(InputError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error),
        }
    }
    let _guard = global_lock(path, &root, cancel)?;
    if win::names(&root)?.iter().any(|name| name == OWNER) {
        owner(&root)?;
        return Ok(root);
    }
    let expected = marker("failforge-workspace-registry", None)?;
    for entry in win::names(&root)? {
        cancel.check()?;
        if entry == GLOBAL {
            continue;
        }
        require(
            entry
                .to_str()
                .and_then(|name| name.strip_prefix(".owner.pending-"))
                .is_some_and(token),
            "unrecognized_uninitialized_registry",
        )?;
        let pending = win::regular_at(&root, &entry, false, false)?;
        win::require_private(&pending, false)?;
        read_marker(&pending, &expected, true)?;
        win::same_entry(&root, &entry, &pending)?;
        win::remove_handle(&pending)?;
    }
    let pending = format!(".owner.pending-{}", win::random_token()?);
    let marker_file = regular(&root, &pending, true, true)?;
    write_marker(&marker_file, &expected)?;
    win::rename_handle(&marker_file, &root, OsStr::new(OWNER), false)?;
    Ok(root)
}
fn validate_tree(root: &File, depth: usize, cancel: &Cancellation) -> Result<(), InputError> {
    cancel.check()?;
    require(depth <= 128, "workspace_depth_limit")?;
    win::require_owned(root, false)?;
    for name in win::names(root)? {
        cancel.check()?;
        if let Ok(child) = win::directory_at(root, &name) {
            validate_tree(&child, depth + 1, cancel)?;
            win::same_entry(root, &name, &child)?;
        } else {
            let file = win::regular_at(root, &name, false, false)?;
            win::require_owned(&file, false)?;
            win::same_entry(root, &name, &file)?;
        }
    }
    Ok(())
}
fn remove_tree(root: &File, depth: usize, cancel: &Cancellation) -> Result<(), InputError> {
    cancel.check()?;
    require(depth <= 128, "workspace_depth_limit")?;
    win::require_owned(root, false)?;
    for name in win::names(root)? {
        cancel.check()?;
        if let Ok(child) = win::directory_at(root, &name) {
            remove_tree(&child, depth + 1, cancel)?;
            win::same_entry(root, &name, &child)?;
            win::remove_handle(&child)?;
        } else {
            let file = win::regular_at(root, &name, false, false)?;
            win::require_owned(&file, false)?;
            win::same_entry(root, &name, &file)?;
            win::remove_handle(&file)?;
        }
    }
    Ok(())
}
fn known_entries(entries: &[OsString]) -> Result<(), InputError> {
    require(
        entries
            .iter()
            .all(|name| name == LEASE || name == METADATA || name == WORK),
        "unrecognized_campaign_entry",
    )
}
fn reap(root: &File, name: &str, cancel: &Cancellation) -> Result<(), InputError> {
    cancel.check()?;
    let (pending, token) =
        campaign_parts(name).ok_or_else(|| failure("unrecognized_registry_entry"))?;
    let campaign = directory(root, name)?;
    let entries = win::names(&campaign)?;
    known_entries(&entries)?;
    let missing_lease = !entries.iter().any(|name| name == LEASE);
    require(!missing_lease || pending, "missing_campaign_lease")?;
    let lease = regular(&campaign, LEASE, true, missing_lease)?;
    require(
        win::try_lock_at(&lease, false, WORKSPACE_LEASE_LOCK_OFFSET)?,
        "active_lease",
    )?;
    read_marker(&lease, LEASE_MAGIC, pending)?;
    if entries.iter().any(|name| name == METADATA) {
        read_marker(
            &regular(&campaign, METADATA, false, false)?,
            &marker("failforge-campaign-workspace", Some(token))?,
            pending,
        )?;
    } else {
        require(pending, "missing_campaign_metadata")?;
    }
    if entries.iter().any(|name| name == WORK) {
        let work = directory(&campaign, WORK)?;
        if pending {
            require(win::names(&work)?.is_empty(), "nonempty_pending_workspace")?;
        }
        validate_tree(&work, 0, cancel)?;
        if !pending {
            remove_tree(&work, 0, cancel)?;
        }
    } else {
        require(pending, "missing_campaign_work_directory")?;
    }
    let cleanup_name = if pending {
        name.to_owned()
    } else {
        let next = format!("pending-{token}");
        win::same_entry(root, OsStr::new(name), &campaign)?;
        win::rename_handle(&campaign, root, OsStr::new(&next), false)?;
        next
    };
    if entries.iter().any(|name| name == WORK) {
        let work = directory(&campaign, WORK)?;
        require(win::names(&work)?.is_empty(), "nonempty_pending_workspace")?;
        win::same_entry(&campaign, OsStr::new(WORK), &work)?;
        win::remove_handle(&work)?;
    }
    for (child, expected) in [
        (
            METADATA,
            marker("failforge-campaign-workspace", Some(token))?,
        ),
        (LEASE, LEASE_MAGIC.to_vec()),
    ] {
        if child == LEASE || entries.iter().any(|name| name == child) {
            let file = regular(&campaign, child, false, false)?;
            read_marker(&file, &expected, pending)?;
            win::same_entry(&campaign, OsStr::new(child), &file)?;
            win::remove_handle(&file)?;
        }
    }
    win::same_entry(root, OsStr::new(&cleanup_name), &campaign)?;
    win::remove_handle(&campaign)?;
    Ok(())
}
fn try_reap(root: &File, name: &str, cancel: &Cancellation) -> Result<Option<String>, InputError> {
    match reap(root, name, cancel) {
        Ok(()) => Ok(None),
        Err(InputError::Invalid(reason)) => Ok(Some(reason)),
        Err(InputError::Io(_)) => Ok(Some("filesystem_error".into())),
        Err(error @ InputError::Cancelled(_)) => Err(error),
    }
}
pub fn start(root_path: &Path, cancel: &Cancellation) -> Result<StartResult, InputError> {
    require(root_path.is_absolute(), "workspace_root_not_absolute")?;
    let root = initialize(root_path, cancel)?;
    let _guard = global_lock(root_path, &root, cancel)?;
    owner(&root)?;
    let mut recovered = vec![];
    let mut skipped = vec![];
    for entry in win::names(&root)? {
        cancel.check()?;
        if entry == GLOBAL || entry == OWNER {
            continue;
        }
        let Some(name) = entry.to_str() else {
            skipped.push(Skip {
                name: entry.to_string_lossy().into(),
                reason: "unrecognized_registry_entry".into(),
                name_bytes: None,
            });
            continue;
        };
        match try_reap(&root, name, cancel)? {
            None => recovered.push(name.into()),
            Some(reason) => skipped.push(Skip {
                name: name.into(),
                reason,
                name_bytes: None,
            }),
        }
    }
    cancel.check()?;
    let token = win::random_token()?;
    let pending = format!("pending-{token}");
    let name = format!("campaign-{token}");
    let campaign = win::create_directory_at(&root, OsStr::new(&pending))?;
    let lease = regular(&campaign, LEASE, true, true)?;
    write_marker(&lease, LEASE_MAGIC)?;
    let lease = WindowsLease::acquire(lease)?;
    let metadata = regular(&campaign, METADATA, true, true)?;
    write_marker(
        &metadata,
        &marker("failforge-campaign-workspace", Some(&token))?,
    )?;
    let work = win::create_directory_at(&campaign, OsStr::new(WORK))?;
    win::rename_handle(&campaign, &root, OsStr::new(&name), false)?;
    drop(_guard);
    Ok(StartResult {
        caps: LifecycleCaps {
            root,
            campaign,
            work,
            lease: Some(lease),
        },
        root_path: root_path.into(),
        campaign_name: name,
        recovered,
        skipped,
    })
}
fn validate_caps(
    root_path: &Path,
    campaign_name: &str,
    caps: &LifecycleCaps,
    cancel: &Cancellation,
) -> Result<File, InputError> {
    require(root_path.is_absolute(), "workspace_root_not_absolute")?;
    let (pending, token) =
        campaign_parts(campaign_name).ok_or_else(|| failure("invalid_campaign_name"))?;
    require(!pending, "invalid_campaign_name")?;
    let guard = global_lock(root_path, &caps.root, cancel)?;
    owner(&caps.root)?;
    win::require_private(&caps.campaign, true)?;
    win::require_private(&caps.work, true)?;
    win::same_entry(&caps.root, OsStr::new(campaign_name), &caps.campaign)?;
    win::same_entry(&caps.campaign, OsStr::new(WORK), &caps.work)?;
    known_entries(&win::names(&caps.campaign)?)?;
    read_marker(
        &regular(&caps.campaign, METADATA, false, false)?,
        &marker("failforge-campaign-workspace", Some(token))?,
        false,
    )?;
    let lease = caps
        .lease
        .as_ref()
        .ok_or_else(|| failure("missing_workspace_lease"))?;
    win::require_private(lease, false)?;
    win::same_entry(&caps.campaign, OsStr::new(LEASE), lease)?;
    read_marker(lease, LEASE_MAGIC, false)?;
    // WindowsLease can only be constructed by successfully acquiring the shared
    // lock, and Arc retains that exact owning handle throughout this operation.
    Ok(guard)
}
#[derive(Debug)]
pub struct BoundAllocation {
    root_path: PathBuf,
    campaign_name: String,
    caps: LifecycleCaps,
    directory: File,
    allocation: Allocation,
    removed: bool,
}
pub fn allocate_bound(
    root_path: &Path,
    campaign_name: &str,
    caps: LifecycleCaps,
    kind: &str,
    cancel: &Cancellation,
) -> Result<BoundAllocation, InputError> {
    require(
        matches!(kind, "snapshot" | "attempt"),
        "invalid_workspace_kind",
    )?;
    let guard = validate_caps(root_path, campaign_name, &caps, cancel)?;
    cancel.check()?;
    let name = format!("{kind}-{}", win::random_token()?);
    let directory = win::create_directory_at(&caps.work, OsStr::new(&name))?;
    let allocation = Allocation {
        path: root_path.join(campaign_name).join(WORK).join(&name),
        name,
    };
    drop(guard);
    Ok(BoundAllocation {
        root_path: root_path.into(),
        campaign_name: campaign_name.into(),
        caps,
        directory,
        allocation,
        removed: false,
    })
}
impl BoundAllocation {
    pub fn path(&self) -> &Path {
        &self.allocation.path
    }
    pub fn name(&self) -> &str {
        &self.allocation.name
    }
    pub fn verify(&self, cancel: &Cancellation) -> Result<(), InputError> {
        require(!self.removed, "workspace_allocation_already_removed")?;
        let _guard = validate_caps(&self.root_path, &self.campaign_name, &self.caps, cancel)?;
        win::require_private(&self.directory, true)?;
        win::same_entry(&self.caps.work, OsStr::new(self.name()), &self.directory)?;
        Ok(())
    }
    pub fn remove(&mut self, cancel: &Cancellation) -> Result<(), InputError> {
        if self.removed {
            return Ok(());
        }
        self.verify(cancel)?;
        validate_tree(&self.directory, 0, cancel)?;
        remove_tree(&self.directory, 0, cancel)?;
        win::same_entry(&self.caps.work, OsStr::new(self.name()), &self.directory)?;
        win::remove_handle(&self.directory)?;
        self.removed = true;
        Ok(())
    }
}
pub fn allocate(
    root_path: &Path,
    campaign_name: &str,
    caps: LifecycleCaps,
    kind: &str,
    cancel: &Cancellation,
) -> Result<Allocation, InputError> {
    Ok(allocate_bound(root_path, campaign_name, caps, kind, cancel)?.allocation)
}
pub fn remove(
    root_path: &Path,
    campaign_name: &str,
    caps: LifecycleCaps,
    name: &str,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    require(temporary_name(name), "invalid_temporary_workspace_name")?;
    let guard = validate_caps(root_path, campaign_name, &caps, cancel)?;
    let child = match directory(&caps.work, name) {
        Err(InputError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    drop(guard);
    validate_tree(&child, 0, cancel)?;
    remove_tree(&child, 0, cancel)?;
    win::same_entry(&caps.work, OsStr::new(name), &child)?;
    win::remove_handle(&child)?;
    Ok(())
}
pub fn close(
    root_path: &Path,
    campaign_name: &str,
    caps: LifecycleCaps,
    cancel: &Cancellation,
) -> Result<CloseReport, InputError> {
    require(caps.lease.is_none(), "workspace_lease_not_released")?;
    require(root_path.is_absolute(), "workspace_root_not_absolute")?;
    require(
        campaign_parts(campaign_name).is_some_and(|(pending, _)| !pending),
        "invalid_campaign_name",
    )?;
    let _guard = global_lock(root_path, &caps.root, cancel)?;
    owner(&caps.root)?;
    match win::same_entry(&caps.root, OsStr::new(campaign_name), &caps.campaign) {
        Err(error) if error.kind() == io::ErrorKind::NotFound && win::unlinked(&caps.campaign)? => {
            return Ok(CloseReport {
                cleanup_deferred: false,
                skipped: vec![],
            });
        }
        other => other?,
    }
    match try_reap(&caps.root, campaign_name, cancel)? {
        None => Ok(CloseReport {
            cleanup_deferred: false,
            skipped: vec![],
        }),
        Some(reason) => Ok(CloseReport {
            cleanup_deferred: true,
            skipped: vec![Skip {
                name: campaign_name.into(),
                reason,
                name_bytes: None,
            }],
        }),
    }
}
#[cfg(test)]
#[path = "lifecycle_windows_tests.rs"]
mod tests;
