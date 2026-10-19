//! Descriptor-owned campaign workspaces compatible with the original V1 registry.
//!
//! Control markers are a disk protocol: their names, exact bytes, and bounded
//! pending states deliberately differ from the native profile registry. A lease
//! must be passed as the same open file description (inheritance or SCM_RIGHTS),
//! never reopened by pathname. No operation explicitly unlocks a lease.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{Cancellation, InputError, failure};

#[path = "lifecycle_fs.rs"]
mod fs;
use fs::*;

pub const LEASE_MAGIC: &[u8] = b"FAILFORGE_WORKSPACE_LEASE_V1\n";
const GLOBAL: &str = ".global.lock";
const OWNER: &str = ".owner";
const LEASE: &str = "lease";
const METADATA: &str = "metadata.json";
const WORK: &str = "work";
const MAX_TREE_DEPTH: usize = 128;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    CampaignCreated,
    AllocationCreated,
    WorkEntryRemoved,
    BeforeCampaignRename,
    CampaignRenamed,
    MarkerRemoved,
}

#[cfg(test)]
type TestHook = Box<dyn FnMut(Stage)>;

#[cfg(test)]
thread_local! {
    static TEST_HOOK: std::cell::RefCell<Option<TestHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn stage(value: Stage) {
    TEST_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow_mut().as_mut() {
            callback(value);
        }
    });
}

/// Owned RPC duplicates. Dropping these preserves any other reference to the
/// inherited lease's open file description and therefore preserves its flock.
#[derive(Debug)]
pub struct LifecycleCaps {
    pub root: File,
    pub campaign: File,
    pub work: File,
    pub lease: Option<File>,
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

fn marker(kind: &str, uid: libc::uid_t, token: Option<&str>) -> Vec<u8> {
    // Field order and final LF match the Python V1 marker byte for byte. All
    // arguments are internal constants, an integer, and validated hex tokens.
    match token {
        Some(token) => {
            format!("{{\"kind\":\"{kind}\",\"version\":1,\"uid\":{uid},\"token\":\"{token}\"}}\n")
        }
        None => format!("{{\"kind\":\"{kind}\",\"version\":1,\"uid\":{uid}}}\n"),
    }
    .into_bytes()
}

fn token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

fn owner(root: &File, uid: libc::uid_t) -> Result<(), InputError> {
    read_marker(
        &regular(root, OsStr::new(OWNER), uid, false, false)?,
        &marker("failforge-workspace-registry", uid, None),
        false,
    )
}

fn global_lock(
    root_path: &Path,
    root: &File,
    uid: libc::uid_t,
    cancel: &Cancellation,
) -> Result<File, InputError> {
    cancel.check()?;
    let guard = regular(root, OsStr::new(GLOBAL), uid, true, false)?;
    read_marker(&guard, b"", false)?;
    // Polling LOCK_NB keeps a cancelled helper responsive while another process
    // owns the global lock. No shared description is ever explicitly unlocked.
    while !try_lock(&guard)? {
        cancel.check()?;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cancel.check()?;
    directory_info(&info(root)?, uid, true)?;
    same_at(
        libc::AT_FDCWD,
        root_path.as_os_str(),
        root,
        "registry_changed",
    )?;
    Ok(guard)
}

fn initialize(
    root_path: &Path,
    uid: libc::uid_t,
    cancel: &Cancellation,
) -> Result<File, InputError> {
    cancel.check()?;
    match mkdir_at(libc::AT_FDCWD, root_path.as_os_str()) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let root = directory(libc::AT_FDCWD, root_path.as_os_str(), uid, true)?;
    let entries = names(&root)?;
    if !entries.iter().any(|name| name == GLOBAL) {
        require(
            !entries.iter().any(|name| name == OWNER),
            "missing_global_lock",
        )?;
        match regular(&root, OsStr::new(GLOBAL), uid, true, true) {
            Ok(_) => {}
            Err(InputError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let _guard = global_lock(root_path, &root, uid, cancel)?;
    let expected = marker("failforge-workspace-registry", uid, None);
    if names(&root)?.iter().any(|name| name == OWNER) {
        owner(&root, uid)?;
        return Ok(root);
    }
    for entry in names(&root)? {
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
        let pending = regular(&root, &entry, uid, false, false)?;
        read_marker(&pending, &expected, true)?;
        same_at(root.as_raw_fd(), &entry, &pending, "file_changed")?;
        unlink(&root, &entry, false)?;
    }
    cancel.check()?;
    let pending_name = format!(".owner.pending-{}", random_token()?);
    let pending = regular(&root, OsStr::new(&pending_name), uid, true, true)?;
    write_marker(&pending, &expected)?;
    rename(&root, &pending_name, OWNER)?;
    root.sync_all()?;
    Ok(root)
}

fn validate_tree(
    root: &File,
    uid: libc::uid_t,
    depth: usize,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    cancel.check()?;
    require(depth <= MAX_TREE_DEPTH, "workspace_depth_limit")?;
    for name in names(root)? {
        cancel.check()?;
        let entry = stat_at(root.as_raw_fd(), &name)?;
        owned(&entry, uid)?;
        if entry.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = directory(root.as_raw_fd(), &name, uid, false)?;
            validate_tree(&child, uid, depth + 1, cancel)?;
        }
    }
    Ok(())
}

fn remove_tree(
    root: &File,
    uid: libc::uid_t,
    depth: usize,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    cancel.check()?;
    require(depth <= MAX_TREE_DEPTH, "workspace_depth_limit")?;
    let entry = info(root)?;
    directory_info(&entry, uid, false)?;
    if entry.st_mode & libc::S_IWUSR == 0 {
        chmod(root, (entry.st_mode & 0o7777) | libc::S_IWUSR)?;
    }
    for name in names(root)? {
        cancel.check()?;
        let entry = stat_at(root.as_raw_fd(), &name)?;
        owned(&entry, uid)?;
        if entry.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = directory(root.as_raw_fd(), &name, uid, false)?;
            remove_tree(&child, uid, depth + 1, cancel)?;
            same_at(root.as_raw_fd(), &name, &child, "directory_changed")?;
            unlink(root, &name, true)?;
        } else {
            unlink(root, &name, false)?;
        }
        #[cfg(test)]
        stage(Stage::WorkEntryRemoved);
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

fn remove_pending(
    campaign: &File,
    uid: libc::uid_t,
    token: &str,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    let entries = names(campaign)?;
    known_entries(&entries)?;
    if entries.iter().any(|name| name == WORK) {
        cancel.check()?;
        let work = directory(campaign.as_raw_fd(), OsStr::new(WORK), uid, true)?;
        require(names(&work)?.is_empty(), "nonempty_pending_workspace")?;
        same_at(
            campaign.as_raw_fd(),
            OsStr::new(WORK),
            &work,
            "directory_changed",
        )?;
        unlink(campaign, OsStr::new(WORK), true)?;
    }
    for (name, expected) in [
        (
            METADATA,
            marker("failforge-campaign-workspace", uid, Some(token)),
        ),
        (LEASE, LEASE_MAGIC.to_vec()),
    ] {
        cancel.check()?;
        if !entries.iter().any(|entry| entry == name) {
            continue;
        }
        let child = regular(campaign, OsStr::new(name), uid, false, false)?;
        read_marker(&child, &expected, true)?;
        same_at(
            campaign.as_raw_fd(),
            OsStr::new(name),
            &child,
            "file_changed",
        )?;
        unlink(campaign, OsStr::new(name), false)?;
        #[cfg(test)]
        stage(Stage::MarkerRemoved);
    }
    Ok(())
}

fn reap(
    root: &File,
    name: &str,
    uid: libc::uid_t,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    cancel.check()?;
    let (pending, token) =
        campaign_parts(name).ok_or_else(|| failure("unrecognized_registry_entry"))?;
    let campaign = directory(root.as_raw_fd(), OsStr::new(name), uid, true)?;
    let entries = names(&campaign)?;
    known_entries(&entries)?;
    let missing_lease = !entries.iter().any(|entry| entry == LEASE);
    require(!missing_lease || pending, "missing_campaign_lease")?;
    let lease = regular(&campaign, OsStr::new(LEASE), uid, true, missing_lease)?;
    require(try_lock(&lease)?, "active_lease")?;
    read_marker(&lease, LEASE_MAGIC, pending)?;
    if entries.iter().any(|entry| entry == METADATA) {
        read_marker(
            &regular(&campaign, OsStr::new(METADATA), uid, false, false)?,
            &marker("failforge-campaign-workspace", uid, Some(token)),
            pending,
        )?;
    } else {
        require(pending, "missing_campaign_metadata")?;
    }
    if entries.iter().any(|entry| entry == WORK) {
        let work = directory(campaign.as_raw_fd(), OsStr::new(WORK), uid, true)?;
        if pending {
            require(names(&work)?.is_empty(), "nonempty_pending_workspace")?;
        }
        validate_tree(&work, uid, 0, cancel)?;
        if !pending {
            remove_tree(&work, uid, 0, cancel)?;
            require(
                names(&work)?.is_empty(),
                "workspace_not_empty_after_cleanup",
            )?;
            work.sync_all()?;
        }
    } else {
        require(pending, "missing_campaign_work_directory")?;
    }
    validate_tree(&campaign, uid, 0, cancel)?;
    let cleanup_name = if pending {
        name.to_owned()
    } else {
        cancel.check()?;
        let next = format!("pending-{token}");
        require(
            !names(root)?.iter().any(|entry| entry == next.as_str()),
            "duplicate_campaign_token",
        )?;
        same_at(
            root.as_raw_fd(),
            OsStr::new(name),
            &campaign,
            "directory_changed",
        )?;
        #[cfg(test)]
        stage(Stage::BeforeCampaignRename);
        rename(root, name, &next)?;
        root.sync_all()?;
        #[cfg(test)]
        stage(Stage::CampaignRenamed);
        next
    };
    remove_pending(&campaign, uid, token, cancel)?;
    same_at(
        root.as_raw_fd(),
        OsStr::new(&cleanup_name),
        &campaign,
        "directory_changed",
    )?;
    unlink(root, OsStr::new(&cleanup_name), true)?;
    root.sync_all()?;
    Ok(())
}

fn try_reap(
    root: &File,
    name: &str,
    uid: libc::uid_t,
    cancel: &Cancellation,
) -> Result<Option<String>, InputError> {
    match reap(root, name, uid, cancel) {
        Ok(()) => Ok(None),
        Err(InputError::Invalid(reason)) => Ok(Some(reason)),
        Err(InputError::Io(_)) => Ok(Some("filesystem_error".into())),
        Err(error @ InputError::Cancelled(_)) => Err(error),
    }
}

fn create(
    root: File,
    root_path: &Path,
    uid: libc::uid_t,
    cancel: &Cancellation,
) -> Result<StartResult, InputError> {
    cancel.check()?;
    let token = random_token()?;
    let pending = format!("pending-{token}");
    let name = format!("campaign-{token}");
    mkdir_at(root.as_raw_fd(), OsStr::new(&pending))?;
    let campaign = directory(root.as_raw_fd(), OsStr::new(&pending), uid, true)?;
    let lease = regular(&campaign, OsStr::new(LEASE), uid, true, true)?;
    require(try_lock(&lease)?, "active_lease")?;
    write_marker(&lease, LEASE_MAGIC)?;
    let metadata = regular(&campaign, OsStr::new(METADATA), uid, true, true)?;
    write_marker(
        &metadata,
        &marker("failforge-campaign-workspace", uid, Some(&token)),
    )?;
    mkdir_at(campaign.as_raw_fd(), OsStr::new(WORK))?;
    let work = directory(campaign.as_raw_fd(), OsStr::new(WORK), uid, true)?;
    campaign.sync_all()?;
    rename(&root, &pending, &name)?;
    root.sync_all()?;
    #[cfg(test)]
    stage(Stage::CampaignCreated);
    // Commit point: returning the capabilities is mandatory even if cancellation
    // arrives now, so the controller can register and close this campaign.
    Ok(StartResult {
        caps: LifecycleCaps {
            root,
            campaign,
            work,
            lease: Some(lease),
        },
        root_path: root_path.into(),
        campaign_name: name,
        recovered: vec![],
        skipped: vec![],
    })
}

/// Recover stale campaigns and create a leased campaign. The root is absolute
/// and echoed without canonicalization; identity is established using inodes.
pub fn start(root_path: &Path, cancel: &Cancellation) -> Result<StartResult, InputError> {
    require(root_path.is_absolute(), "workspace_root_not_absolute")?;
    let run = || {
        let uid = uid();
        let root = initialize(root_path, uid, cancel)?;
        let _guard = global_lock(root_path, &root, uid, cancel)?;
        owner(&root, uid)?;
        let mut recovered = vec![];
        let mut skipped = vec![];
        for entry in names(&root)? {
            cancel.check()?;
            if entry == GLOBAL || entry == OWNER {
                continue;
            }
            let Some(name) = entry.to_str() else {
                let bytes: String = entry
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                skipped.push(Skip {
                    name: format!("<non-utf8:{bytes}>"),
                    reason: "unrecognized_registry_entry".into(),
                    name_bytes: Some(entry.as_bytes().to_vec()),
                });
                continue;
            };
            match try_reap(&root, name, uid, cancel)? {
                None => recovered.push(name.to_owned()),
                Some(reason) => skipped.push(Skip {
                    name: name.to_owned(),
                    reason,
                    name_bytes: None,
                }),
            }
        }
        let mut result = create(root, root_path, uid, cancel)?;
        result.recovered = recovered;
        result.skipped = skipped;
        Ok(result)
    };
    match run() {
        Err(InputError::Io(_)) => Err(failure("workspace_registry_unavailable")),
        other => other,
    }
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
    let uid = uid();
    let guard = global_lock(root_path, &caps.root, uid, cancel)?;
    owner(&caps.root, uid)?;
    directory_info(&info(&caps.campaign)?, uid, true)?;
    directory_info(&info(&caps.work)?, uid, true)?;
    same_at(
        caps.root.as_raw_fd(),
        OsStr::new(campaign_name),
        &caps.campaign,
        "directory_changed",
    )?;
    same_at(
        caps.campaign.as_raw_fd(),
        OsStr::new(WORK),
        &caps.work,
        "directory_changed",
    )?;
    known_entries(&names(&caps.campaign)?)?;
    read_marker(
        &regular(&caps.campaign, OsStr::new(METADATA), uid, false, false)?,
        &marker("failforge-campaign-workspace", uid, Some(token)),
        false,
    )?;
    let lease = caps
        .lease
        .as_ref()
        .ok_or_else(|| failure("missing_workspace_lease"))?;
    regular_info(&info(lease)?, uid)?;
    same_at(
        caps.campaign.as_raw_fd(),
        OsStr::new(LEASE),
        lease,
        "file_changed",
    )?;
    read_marker(lease, LEASE_MAGIC, false)?;
    require(try_lock(lease)?, "invalid_workspace_lease")?;
    Ok(guard)
}

/// Allocate an empty snapshot or attempt under the retained work descriptor.
pub fn allocate(
    root_path: &Path,
    campaign_name: &str,
    caps: LifecycleCaps,
    kind: &str,
    cancel: &Cancellation,
) -> Result<Allocation, InputError> {
    require(
        matches!(kind, "snapshot" | "attempt"),
        "invalid_workspace_kind",
    )?;
    let _guard = validate_caps(root_path, campaign_name, &caps, cancel)?;
    cancel.check()?;
    let name = format!("{kind}-{}", random_token()?);
    mkdir_at(caps.work.as_raw_fd(), OsStr::new(&name))?;
    #[cfg(test)]
    stage(Stage::AllocationCreated);
    // Do not observe late cancellation after committing an allocation: its name
    // must reach the caller so its context manager can clean it up.
    Ok(Allocation {
        path: root_path.join(campaign_name).join(WORK).join(&name),
        name,
    })
}

/// Remove one known temporary allocation, without following payload aliases.
pub fn remove(
    root_path: &Path,
    campaign_name: &str,
    caps: LifecycleCaps,
    name: &str,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    require(temporary_name(name), "invalid_temporary_workspace_name")?;
    let guard = validate_caps(root_path, campaign_name, &caps, cancel)?;
    let child = match directory(caps.work.as_raw_fd(), OsStr::new(name), uid(), true) {
        Err(InputError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    // The inherited lease protects this campaign throughout removal. Holding
    // the registry lock during a large payload cleanup would serialize workers.
    drop(guard);
    validate_tree(&child, uid(), 0, cancel)?;
    remove_tree(&child, uid(), 0, cancel)?;
    same_at(
        caps.work.as_raw_fd(),
        OsStr::new(name),
        &child,
        "directory_changed",
    )?;
    unlink(&caps.work, OsStr::new(name), true)?;
    Ok(())
}

/// Close after the controller has dropped its lease. Other native owners keep
/// the same flock alive, yielding a deferred report instead of unsafe deletion.
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
    let run = || {
        let uid = uid();
        let _guard = global_lock(root_path, &caps.root, uid, cancel)?;
        owner(&caps.root, uid)?;
        // Retained capabilities must still be our directory inodes. Protocol
        // mode validation belongs to the reaper so malformed campaign modes
        // keep the legacy deferred-cleanup report rather than becoming fatal.
        directory_info(&info(&caps.campaign)?, uid, false)?;
        directory_info(&info(&caps.work)?, uid, false)?;
        match same_at(
            caps.root.as_raw_fd(),
            OsStr::new(campaign_name),
            &caps.campaign,
            "directory_changed",
        ) {
            Err(InputError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                require(
                    names(&caps.campaign)?.is_empty() && names(&caps.work)?.is_empty(),
                    "directory_changed",
                )?;
                require(directory_unlinked(&caps.campaign)?, "directory_changed")?;
                return Ok(CloseReport {
                    cleanup_deferred: false,
                    skipped: vec![],
                });
            }
            other => other?,
        }
        match same_at(
            caps.campaign.as_raw_fd(),
            OsStr::new(WORK),
            &caps.work,
            "directory_changed",
        ) {
            // A missing committed control entry is a reaper skip reason. A
            // replacement inode still fails before anything can be removed.
            Err(InputError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
            other => other?,
        }
        match try_reap(&caps.root, campaign_name, uid, cancel)? {
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
    };
    match run() {
        Err(InputError::Io(_)) => Err(failure("workspace_cleanup_unavailable")),
        other => other,
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
