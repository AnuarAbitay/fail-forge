use super::*;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);

#[test]
fn bound_allocation_retains_lease_through_cleanup_until_owner_drop() {
    let fixture = Fixture::new();
    let started = start(&fixture.root, &Cancellation::default()).unwrap();
    let close_caps = LifecycleCaps {
        root: started.caps.root.try_clone().unwrap(),
        campaign: started.caps.campaign.try_clone().unwrap(),
        work: started.caps.work.try_clone().unwrap(),
        lease: None,
    };
    let lease_path = fixture.root.join(&started.campaign_name).join(LEASE);
    let mut allocation = allocate_bound(
        &fixture.root,
        &started.campaign_name,
        started.caps,
        "attempt",
        &Cancellation::default(),
    )
    .unwrap();
    assert!(busy(&lease_path));
    assert!(allocation.name().starts_with("attempt-"));
    allocation.verify(&Cancellation::default()).unwrap();
    private_file(&allocation.path().join("payload"), b"data");
    allocation.remove(&Cancellation::default()).unwrap();
    allocation.remove(&Cancellation::default()).unwrap();
    assert!(busy(&lease_path));
    assert!(allocation.verify(&Cancellation::default()).is_err());
    drop(allocation);
    assert!(!busy(&lease_path));
    assert!(
        !close(
            &fixture.root,
            &started.campaign_name,
            close_caps,
            &Cancellation::default()
        )
        .unwrap()
        .cleanup_deferred
    );
}

#[test]
fn bound_allocation_distinguishes_removed_inode_from_renamed_empty_directory() {
    for renamed in [false, true] {
        let fixture = Fixture::new();
        let started = start(&fixture.root, &Cancellation::default()).unwrap();
        let mut allocation = allocate_bound(
            &fixture.root,
            &started.campaign_name,
            started.caps,
            "attempt",
            &Cancellation::default(),
        )
        .unwrap();
        let path = allocation.path().to_owned();
        let outside = fixture.base.join("renamed");
        if renamed {
            fs::rename(&path, &outside).unwrap();
            assert!(allocation.remove(&Cancellation::default()).is_err());
            assert!(outside.is_dir());
            fs::rename(outside, &path).unwrap();
        } else {
            fs::remove_dir(&path).unwrap();
        }
        allocation.remove(&Cancellation::default()).unwrap();
        allocation.remove(&Cancellation::default()).unwrap();
        assert!(!path.exists());
    }
}

#[test]
fn bound_allocation_cleanup_cancel_is_retryable_without_releasing_its_lease() {
    let fixture = Fixture::new();
    let started = start(&fixture.root, &Cancellation::default()).unwrap();
    let lease = fixture.root.join(&started.campaign_name).join(LEASE);
    let mut allocation = allocate_bound(
        &fixture.root,
        &started.campaign_name,
        started.caps,
        "attempt",
        &Cancellation::default(),
    )
    .unwrap();
    private_file(&allocation.path().join("a"), b"first");
    private_file(&allocation.path().join("b"), b"second");
    let cancel = Cancellation::default();
    let request = cancel.clone();
    TEST_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |stage| {
            if stage == Stage::WorkEntryRemoved {
                request.request(libc::SIGTERM);
            }
        }))
    });
    let result = allocation.remove(&cancel);
    TEST_HOOK.with(|slot| slot.borrow_mut().take());
    assert!(matches!(result, Err(InputError::Cancelled(libc::SIGTERM))));
    assert!(busy(&lease));
    assert!(allocation.path().is_dir());
    allocation.remove(&Cancellation::default()).unwrap();
    assert!(!allocation.path().exists());
}

struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-lifecycle-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        private_directory(&base);
        let root = base.join("registry with spaces Ж");
        Self { base, root }
    }

    fn pending(&self, number: u64) -> (PathBuf, String) {
        let token = format!("{number:032x}");
        let path = self.root.join(format!("pending-{token}"));
        private_directory(&path);
        (path, token)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only test-owned directories are made writable. Never follow payload
        // aliases, including the deliberate external-directory test fixtures.
        fn writable(path: &Path) {
            let Ok(metadata) = fs::symlink_metadata(path) else {
                return;
            };
            if !metadata.is_dir() {
                return;
            }
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        }
        writable(&self.base);
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn private_directory(path: &Path) {
    DirBuilder::new().mode(0o700).create(path).unwrap();
}

fn private_file(path: &Path, contents: &[u8]) -> File {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(contents).unwrap();
    file
}

fn expected_owner() -> Vec<u8> {
    format!(
        "{{\"kind\":\"failforge-workspace-registry\",\"version\":1,\"uid\":{}}}\n",
        // SAFETY: geteuid has no arguments or memory side effects.
        unsafe { libc::geteuid() }
    )
    .into_bytes()
}

fn expected_campaign(token: &str) -> Vec<u8> {
    format!(
        "{{\"kind\":\"failforge-campaign-workspace\",\"version\":1,\"uid\":{},\"token\":\"{token}\"}}\n",
        // SAFETY: geteuid has no arguments or memory side effects.
        unsafe { libc::geteuid() }
    )
    .into_bytes()
}

fn flock_exclusive(file: &File) {
    // SAFETY: the live descriptor is borrowed; flock has no pointer arguments.
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
}

fn busy(path: &Path) -> bool {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    // SAFETY: the live descriptor is borrowed; flock has no pointer arguments.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        false
    } else {
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        true
    }
}

fn fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated path; mode is a plain integer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

const EXPECTED_LEASE: &[u8] = b"FAILFORGE_WORKSPACE_LEASE_V1\n";

fn begin(root: &Path) -> StartResult {
    start(root, &Cancellation::default()).unwrap()
}

fn cloned(caps: &LifecycleCaps, lease: bool) -> LifecycleCaps {
    LifecycleCaps {
        root: caps.root.try_clone().unwrap(),
        campaign: caps.campaign.try_clone().unwrap(),
        work: caps.work.try_clone().unwrap(),
        lease: if lease {
            caps.lease.as_ref().map(|file| file.try_clone().unwrap())
        } else {
            None
        },
    }
}

#[test]
fn public_capability_duplicates_retain_the_same_lease_until_the_last_owner_closes() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    let caps = started.caps.try_clone().unwrap();
    caps.verify(
        &fixture.root,
        &started.campaign_name,
        &Cancellation::default(),
    )
    .unwrap();
    for file in [
        &caps.root,
        &caps.campaign,
        &caps.work,
        caps.lease.as_ref().unwrap(),
    ] {
        // SAFETY: each descriptor is owned by the live capability object.
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
    let campaign = started.campaign_name.clone();
    assert!(finish(started).cleanup_deferred);
    assert!(fixture.root.join(&campaign).is_dir());
    caps.verify(&fixture.root, &campaign, &Cancellation::default())
        .unwrap();
    let mut closing = caps.try_clone().unwrap();
    drop(caps);
    assert!(busy(&fixture.root.join(&campaign).join("lease")));
    closing.lease.take();
    assert!(
        !close(&fixture.root, &campaign, closing, &Cancellation::default())
            .unwrap()
            .cleanup_deferred
    );
    assert!(!fixture.root.join(campaign).exists());
}

#[test]
fn capability_verification_without_allocation_rejects_missing_lease_and_replaced_work() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    let mut caps = started.caps.try_clone().unwrap();
    caps.lease.take();
    failure_reason(
        caps.verify(
            &fixture.root,
            &started.campaign_name,
            &Cancellation::default(),
        ),
        "missing_workspace_lease",
    );
    let work = fixture.root.join(&started.campaign_name).join(WORK);
    let displaced = fixture.base.join("old-work");
    fs::rename(&work, &displaced).unwrap();
    private_directory(&work);
    failure_reason(
        started.caps.verify(
            &fixture.root,
            &started.campaign_name,
            &Cancellation::default(),
        ),
        "directory_changed",
    );
    fs::remove_dir(work).unwrap();
    fs::rename(
        displaced,
        fixture.root.join(&started.campaign_name).join(WORK),
    )
    .unwrap();
    drop(caps);
    assert!(!finish(started).cleanup_deferred);
}

fn finish(mut started: StartResult) -> CloseReport {
    started.caps.lease.take();
    close(
        &started.root_path,
        &started.campaign_name,
        started.caps,
        &Cancellation::default(),
    )
    .unwrap()
}

fn initialized(root: &Path) {
    assert!(!finish(begin(root)).cleanup_deferred);
}

fn abandoned(root: &Path) -> PathBuf {
    let started = begin(root);
    let path = root.join(&started.campaign_name);
    drop(started);
    path
}

fn skipped(started: &StartResult, path: &Path, reason: &str) {
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(
        started
            .skipped
            .iter()
            .any(|item| item.name == name && item.reason == reason),
        "expected {name} skipped as {reason}; observed {:?}",
        started
            .skipped
            .iter()
            .map(|item| (&item.name, &item.reason))
            .collect::<Vec<_>>()
    );
    assert!(!started.recovered.iter().any(|item| item == name));
}

fn failure_reason<T>(result: Result<T, InputError>, expected: &str) {
    let error = result
        .err()
        .expect("operation must reject the malformed workspace");
    assert!(
        matches!(&error, InputError::Invalid(reason) if reason == expected),
        "{error:?}"
    );
}

fn allocate_attempt(started: &StartResult) -> Allocation {
    allocate(
        &started.root_path,
        &started.campaign_name,
        cloned(&started.caps, true),
        "attempt",
        &Cancellation::default(),
    )
    .unwrap()
}

fn remove_allocation(started: &StartResult, allocation: &Allocation) -> Result<(), InputError> {
    remove(
        &started.root_path,
        &started.campaign_name,
        cloned(&started.caps, true),
        &allocation.name,
        &Cancellation::default(),
    )
}

#[test]
fn exact_legacy_markers_private_modes_and_cloexec_descriptors() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    assert_eq!(started.root_path, fixture.root);
    let campaign = fixture.root.join(&started.campaign_name);
    assert_eq!(
        fs::read(fixture.root.join(".owner")).unwrap(),
        expected_owner()
    );
    assert_eq!(fs::read(campaign.join("lease")).unwrap(), EXPECTED_LEASE);
    assert_eq!(
        fs::read(campaign.join("metadata.json")).unwrap(),
        expected_campaign(started.campaign_name.strip_prefix("campaign-").unwrap())
    );
    for path in [&fixture.root, &campaign, &campaign.join("work")] {
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, 0o700);
    }
    for path in [
        fixture.root.join(".global.lock"),
        fixture.root.join(".owner"),
        campaign.join("lease"),
        campaign.join("metadata.json"),
    ] {
        let metadata = fs::metadata(path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        // SAFETY: geteuid has no arguments or memory side effects.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
    for file in [
        &started.caps.root,
        &started.caps.campaign,
        &started.caps.work,
        started.caps.lease.as_ref().unwrap(),
    ] {
        // SAFETY: fcntl only inspects flags on the borrowed live descriptor.
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
    assert!(busy(&campaign.join("lease")));
    assert!(!finish(started).cleanup_deferred);
    assert!(!campaign.exists());
}

#[test]
fn root_path_keeps_an_existing_ancestor_alias_and_lexical_identity() {
    let fixture = Fixture::new();
    let actual = fixture.base.join("actual");
    private_directory(&actual);
    let alias = fixture.base.join("alias");
    symlink(&actual, &alias).unwrap();
    let requested = alias.join("registry");
    let started = begin(&requested);
    assert_eq!(started.root_path, requested);
    assert!(actual.join("registry/.owner").is_file());
    assert!(!finish(started).cleanup_deferred);
}

#[test]
fn temporary_allocations_are_distinct_and_only_valid_kinds_are_accepted() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    let first = allocate_attempt(&started);
    let second = allocate(
        &fixture.root,
        &started.campaign_name,
        cloned(&started.caps, true),
        "snapshot",
        &Cancellation::default(),
    )
    .unwrap();
    assert_ne!(first.path, second.path);
    assert!(first.name.starts_with("attempt-"));
    assert!(second.name.starts_with("snapshot-"));
    for allocation in [&first, &second] {
        assert_eq!(
            allocation.path.parent().unwrap(),
            fixture.root.join(&started.campaign_name).join("work")
        );
        assert_eq!(
            fs::metadata(&allocation.path).unwrap().mode() & 0o7777,
            0o700
        );
        assert_eq!(fs::read_dir(&allocation.path).unwrap().count(), 0);
    }
    for kind in ["../outside", "", "other", "/tmp", "attempt/../snapshot"] {
        failure_reason(
            allocate(
                &fixture.root,
                &started.campaign_name,
                cloned(&started.caps, true),
                kind,
                &Cancellation::default(),
            ),
            "invalid_workspace_kind",
        );
    }
    fs::write(first.path.join("payload"), b"test bytes").unwrap();
    remove_allocation(&started, &first).unwrap();
    remove_allocation(&started, &first).unwrap();
    assert!(!first.path.exists());
    assert!(second.path.exists());
    remove_allocation(&started, &second).unwrap();
    assert!(!finish(started).cleanup_deferred);
}

#[test]
fn active_campaigns_are_skipped_and_last_inherited_lease_controls_recovery() {
    let fixture = Fixture::new();
    let first = begin(&fixture.root);
    let campaign = fixture.root.join(&first.campaign_name);
    fs::write(campaign.join("work/keep"), b"active source").unwrap();
    let duplicate_one = first.caps.lease.as_ref().unwrap().try_clone().unwrap();
    let duplicate_two = first.caps.lease.as_ref().unwrap().try_clone().unwrap();
    let second = begin(&fixture.root);
    skipped(&second, &campaign, "active_lease");
    assert!(second.recovered.is_empty());
    assert!(!finish(second).cleanup_deferred);
    let report = finish(first);
    assert!(report.cleanup_deferred);
    assert!(report.skipped.iter().any(|item| item.name
        == campaign.file_name().unwrap().to_str().unwrap()
        && item.reason == "active_lease"));
    drop(duplicate_one);
    let third = begin(&fixture.root);
    skipped(&third, &campaign, "active_lease");
    assert_eq!(
        fs::read(campaign.join("work/keep")).unwrap(),
        b"active source"
    );
    assert!(busy(&campaign.join("lease")));
    assert!(!finish(third).cleanup_deferred);
    drop(duplicate_two);
    let recovered = begin(&fixture.root);
    assert!(
        recovered
            .recovered
            .iter()
            .any(|name| name == campaign.file_name().unwrap().to_str().unwrap())
    );
    assert!(!campaign.exists());
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn already_reaped_campaign_close_succeeds_without_touching_new_campaign() {
    let fixture = Fixture::new();
    let mut old = begin(&fixture.root);
    old.caps.lease.take();
    let replacement = begin(&fixture.root);
    assert!(replacement.recovered.contains(&old.campaign_name));
    let current = fixture.root.join(&replacement.campaign_name);
    assert!(!finish(old).cleanup_deferred);
    assert!(current.exists());
    assert!(!finish(replacement).cleanup_deferred);
}

#[test]
fn payload_symlinks_and_hardlinks_preserve_outside_bytes_and_modes() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("outside");
    private_directory(&outside);
    let sentinel = outside.join("keep");
    fs::write(&sentinel, b"must remain exact").unwrap();
    fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o500)).unwrap();
    let started = begin(&fixture.root);
    let allocation = allocate_attempt(&started);
    symlink(&outside, allocation.path.join("outside directory")).unwrap();
    symlink(&sentinel, allocation.path.join("outside file")).unwrap();
    fs::hard_link(&sentinel, allocation.path.join("hardlink")).unwrap();
    let readonly = allocation.path.join("readonly");
    private_directory(&readonly);
    fs::write(readonly.join("inside"), b"remove me").unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o500)).unwrap();
    remove_allocation(&started, &allocation).unwrap();
    assert!(!allocation.path.exists());
    assert!(!finish(started).cleanup_deferred);
    assert_eq!(fs::read(&sentinel).unwrap(), b"must remain exact");
    assert_eq!(fs::metadata(&outside).unwrap().mode() & 0o7777, 0o500);
    assert_eq!(fs::metadata(&sentinel).unwrap().mode() & 0o7777, 0o444);
    assert_eq!(fs::metadata(&sentinel).unwrap().nlink(), 1);
}

#[test]
fn abandoned_readonly_payload_is_recovered_but_unknown_readonly_payload_is_untouched() {
    let fixture = Fixture::new();
    let old = abandoned(&fixture.root);
    let readonly = old.join("work/readonly");
    private_directory(&readonly);
    fs::write(readonly.join("remove"), b"owned bytes").unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o500)).unwrap();
    let unknown = fixture.root.join("foreign-user-directory");
    private_directory(&unknown);
    fs::write(unknown.join("keep"), b"foreign bytes").unwrap();
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o500)).unwrap();
    let recovered = begin(&fixture.root);
    assert!(!old.exists());
    skipped(&recovered, &unknown, "unrecognized_registry_entry");
    assert_eq!(fs::read(unknown.join("keep")).unwrap(), b"foreign bytes");
    assert_eq!(fs::metadata(&unknown).unwrap().mode() & 0o7777, 0o500);
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn pending_creation_and_final_deletion_windows_recover_exact_known_prefixes() {
    let fixture = Fixture::new();
    initialized(&fixture.root);
    let mut paths = Vec::new();
    for stage in 0..10 {
        let (path, token) = fixture.pending(stage + 1);
        if (1..=6).contains(&stage) {
            private_file(
                &path.join("lease"),
                match stage {
                    1 => b"",
                    2 => &EXPECTED_LEASE[..8],
                    _ => EXPECTED_LEASE,
                },
            );
        }
        if (4..=6).contains(&stage) || stage == 8 {
            let bytes = expected_campaign(&token);
            private_file(
                &path.join("metadata.json"),
                if stage == 4 { &bytes[..12] } else { &bytes },
            );
        }
        if matches!(stage, 6..=8) {
            private_directory(&path.join("work"));
        }
        if stage == 9 {
            private_file(&path.join("lease"), EXPECTED_LEASE);
        }
        paths.push(path);
    }
    let recovered = begin(&fixture.root);
    let expected: Vec<String> = paths
        .iter()
        .map(|path| path.file_name().unwrap().to_str().unwrap().into())
        .collect();
    assert_eq!(recovered.recovered, expected);
    assert!(recovered.skipped.is_empty());
    assert!(paths.iter().all(|path| !path.exists()));
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn pending_live_lease_foreign_markers_and_nonempty_work_are_never_reaped() {
    let fixture = Fixture::new();
    initialized(&fixture.root);
    let (active, _) = fixture.pending(1);
    let lease = private_file(&active.join("lease"), b"");
    flock_exclusive(&lease);
    let (foreign, _) = fixture.pending(2);
    private_file(&foreign.join("lease"), EXPECTED_LEASE);
    private_file(&foreign.join("metadata.json"), b"{\"kind\":\"other-tool\"}");
    let (nonempty, _) = fixture.pending(3);
    private_directory(&nonempty.join("work"));
    fs::write(nonempty.join("work/keep"), b"must stay").unwrap();
    let recovered = begin(&fixture.root);
    skipped(&recovered, &active, "active_lease");
    skipped(&recovered, &foreign, "invalid_marker");
    skipped(&recovered, &nonempty, "nonempty_pending_workspace");
    assert_eq!(fs::read(nonempty.join("work/keep")).unwrap(), b"must stay");
    assert!(!finish(recovered).cleanup_deferred);
    drop(lease);
    let recovered = begin(&fixture.root);
    assert!(!active.exists());
    assert!(foreign.exists() && nonempty.exists());
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn root_initialization_windows_recover_without_foreign_marker_adoption() {
    for stage in 0..5 {
        let fixture = Fixture::new();
        private_directory(&fixture.root);
        if stage != 0 {
            private_file(&fixture.root.join(".global.lock"), b"");
        }
        if stage >= 2 {
            let expected = expected_owner();
            private_file(
                &fixture.root.join(format!(".owner.pending-{:032x}", 1)),
                match stage {
                    2 => b"",
                    3 => &expected[..15],
                    _ => &expected,
                },
            );
        }
        let started = begin(&fixture.root);
        assert_eq!(
            fs::read(fixture.root.join(".owner")).unwrap(),
            expected_owner()
        );
        assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".owner.pending-")
        }));
        assert!(!finish(started).cleanup_deferred);
    }
}

#[test]
fn foreign_uninitialized_registry_or_partial_owner_is_never_adopted() {
    for partial in [false, true] {
        let fixture = Fixture::new();
        private_directory(&fixture.root);
        let path = if partial {
            private_file(&fixture.root.join(".global.lock"), b"");
            fixture.root.join(format!(".owner.pending-{:032x}", 1))
        } else {
            fixture.root.join("keep")
        };
        private_file(&path, b"{\"kind\":\"other-tool\"}");
        failure_reason(
            start(&fixture.root, &Cancellation::default()),
            if partial {
                "invalid_marker"
            } else {
                "unrecognized_uninitialized_registry"
            },
        );
        assert_eq!(fs::read(&path).unwrap(), b"{\"kind\":\"other-tool\"}");
        assert!(!fixture.root.join(".owner").exists());
    }
}

#[test]
fn invalid_or_profile_root_markers_are_fatal_and_never_rewritten() {
    for bytes in [
        b"{\"kind\":\"another-tool\"}\n".as_slice(),
        b"FAILFORGE_NATIVE_PROFILE_WORKSPACES_V1:501\n",
        b"",
    ] {
        let fixture = Fixture::new();
        initialized(&fixture.root);
        let owner = fixture.root.join(".owner");
        fs::write(&owner, bytes).unwrap();
        failure_reason(
            start(&fixture.root, &Cancellation::default()),
            "invalid_marker",
        );
        assert_eq!(fs::read(&owner).unwrap(), bytes);
    }
}

#[test]
fn missing_committed_controls_or_foreign_metadata_leave_payload_untouched() {
    for (entry, expected) in [
        ("lease", "missing_campaign_lease"),
        ("metadata.json", "missing_campaign_metadata"),
        ("work", "missing_campaign_work_directory"),
        ("foreign", "invalid_marker"),
        ("extra", "unrecognized_campaign_entry"),
    ] {
        let fixture = Fixture::new();
        let campaign = abandoned(&fixture.root);
        if entry == "work" {
            fs::remove_dir(campaign.join("work")).unwrap();
        } else {
            fs::write(campaign.join("work/keep"), b"do not delete").unwrap();
            match entry {
                "foreign" => {
                    fs::write(campaign.join("metadata.json"), b"{\"kind\":\"other-tool\"}").unwrap()
                }
                "extra" => fs::write(campaign.join("foreign-entry"), b"unrecognized").unwrap(),
                _ => fs::remove_file(campaign.join(entry)).unwrap(),
            }
        }
        let recovered = begin(&fixture.root);
        skipped(&recovered, &campaign, expected);
        if entry != "work" {
            assert_eq!(
                fs::read(campaign.join("work/keep")).unwrap(),
                b"do not delete"
            );
        }
        assert!(!finish(recovered).cleanup_deferred);
    }
}

#[test]
fn final_registry_symlink_and_nonprivate_modes_are_rejected() {
    let fixture = Fixture::new();
    let outside = fixture.base.join("outside");
    private_directory(&outside);
    fs::write(outside.join("keep"), b"unchanged").unwrap();
    symlink(&outside, &fixture.root).unwrap();
    failure_reason(
        start(&fixture.root, &Cancellation::default()),
        "not_a_directory",
    );
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unchanged");
    fs::remove_file(&fixture.root).unwrap();
    private_directory(&fixture.root);
    for mode in [0o755, 0o770, 0o750, 0o1700] {
        fs::set_permissions(&fixture.root, fs::Permissions::from_mode(mode)).unwrap();
        failure_reason(
            start(&fixture.root, &Cancellation::default()),
            "invalid_directory_permissions",
        );
        assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    }
}

#[test]
fn recognized_campaign_symlink_and_invalid_control_modes_are_skipped() {
    let fixture = Fixture::new();
    initialized(&fixture.root);
    let outside = fixture.base.join("outside");
    private_directory(&outside);
    fs::write(outside.join("keep"), b"unchanged").unwrap();
    let alias = fixture.root.join(format!("campaign-{:032x}", 1));
    symlink(&outside, &alias).unwrap();
    let recovered = begin(&fixture.root);
    skipped(&recovered, &alias, "not_a_directory");
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unchanged");
    assert!(!finish(recovered).cleanup_deferred);
    for mode in [0o644, 0o400, 0o660, 0o1600] {
        let campaign = abandoned(&fixture.root);
        fs::set_permissions(campaign.join("lease"), fs::Permissions::from_mode(mode)).unwrap();
        let recovered = begin(&fixture.root);
        skipped(
            &recovered,
            &campaign,
            if mode == 0o400 {
                "filesystem_error"
            } else {
                "invalid_file_permissions"
            },
        );
        assert!(!finish(recovered).cleanup_deferred);
    }
}

#[test]
fn global_lock_aliases_and_hardlinked_campaign_lease_are_rejected() {
    for hardlink in [false, true] {
        let fixture = Fixture::new();
        initialized(&fixture.root);
        let lock = fixture.root.join(".global.lock");
        fs::remove_file(&lock).unwrap();
        let outside = fixture.base.join("outside lock");
        private_file(&outside, b"");
        if hardlink {
            fs::hard_link(&outside, &lock).unwrap();
        } else {
            symlink(&outside, &lock).unwrap();
        }
        assert!(start(&fixture.root, &Cancellation::default()).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"");
    }
    let fixture = Fixture::new();
    let campaign = abandoned(&fixture.root);
    let outside = fixture.base.join("hardlinked lease");
    fs::hard_link(campaign.join("lease"), &outside).unwrap();
    let recovered = begin(&fixture.root);
    skipped(&recovered, &campaign, "invalid_file_link_count");
    assert_eq!(fs::read(&outside).unwrap(), EXPECTED_LEASE);
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn owner_and_campaign_metadata_fifos_are_rejected_without_a_writer() {
    let fixture = Fixture::new();
    initialized(&fixture.root);
    let owner = fixture.root.join(".owner");
    fs::remove_file(&owner).unwrap();
    fifo(&owner);
    let root = fixture.root.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender
            .send(start(&root, &Cancellation::default()).err())
            .unwrap()
    });
    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("FIFO must not wait for a writer")
        .unwrap();
    failure_reason::<()>(Err(error), "not_a_regular_file");
    worker.join().unwrap();
    fs::remove_file(&owner).unwrap();
    private_file(&owner, &expected_owner());
    let campaign = abandoned(&fixture.root);
    fs::remove_file(campaign.join("metadata.json")).unwrap();
    fifo(&campaign.join("metadata.json"));
    let root = fixture.root.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || sender.send(begin(&root)).unwrap());
    let recovered = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("FIFO must not wait for a writer");
    worker.join().unwrap();
    skipped(&recovered, &campaign, "not_a_regular_file");
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn depth_limit_is_checked_before_any_payload_deletion_or_permission_change() {
    let fixture = Fixture::new();
    let campaign = abandoned(&fixture.root);
    let keep = campaign.join("work/a-keep");
    fs::write(&keep, b"keep before deep tree").unwrap();
    let readonly = campaign.join("work/b-readonly");
    private_directory(&readonly);
    fs::write(readonly.join("keep"), b"keep permissions").unwrap();
    fs::set_permissions(&readonly, fs::Permissions::from_mode(0o500)).unwrap();
    let mut leaf = campaign.join("work/z-deep");
    private_directory(&leaf);
    for _ in 0..130 {
        leaf.push("d");
        private_directory(&leaf);
    }
    fs::write(leaf.join("sentinel"), b"deep bytes").unwrap();
    let markers: Vec<_> = ["lease", "metadata.json"]
        .into_iter()
        .map(|name| (name, fs::read(campaign.join(name)).unwrap()))
        .collect();
    for _ in 0..2 {
        let recovered = begin(&fixture.root);
        skipped(&recovered, &campaign, "workspace_depth_limit");
        assert_eq!(fs::read(&keep).unwrap(), b"keep before deep tree");
        assert_eq!(fs::read(leaf.join("sentinel")).unwrap(), b"deep bytes");
        assert_eq!(fs::metadata(&readonly).unwrap().mode() & 0o7777, 0o500);
        for (name, bytes) in &markers {
            assert_eq!(&fs::read(campaign.join(name)).unwrap(), bytes);
        }
        assert!(!finish(recovered).cleanup_deferred);
    }
}

#[test]
fn concurrent_reapers_recover_abandoned_campaign_exactly_once() {
    let fixture = Fixture::new();
    let abandoned = abandoned(&fixture.root);
    fs::write(abandoned.join("work/sentinel"), b"remove once").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let root = fixture.root.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                begin(&root)
            })
        })
        .collect();
    barrier.wait();
    let started: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let name = abandoned.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        started
            .iter()
            .filter(|result| result.recovered.iter().any(|item| item == name))
            .count(),
        1
    );
    assert!(!abandoned.exists());
    for result in started {
        assert!(!finish(result).cleanup_deferred);
    }
}

#[test]
fn blocked_global_lock_observes_cancellation_without_waiting_for_owner_exit() {
    let fixture = Fixture::new();
    initialized(&fixture.root);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root.join(".global.lock"))
        .unwrap();
    flock_exclusive(&lock);
    let cancel = Cancellation::default();
    let observing = cancel.clone();
    let root = fixture.root.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || sender.send(start(&root, &observing).err()).unwrap());
    // Even if the thread has not reached flock yet, cancellation must remain
    // latched and the lock remains held until after its bounded completion.
    std::thread::sleep(Duration::from_millis(30));
    let since = Instant::now();
    cancel.request(libc::SIGTERM);
    let error = receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("global lock wait must be cancellable");
    assert!(matches!(error, Some(InputError::Cancelled(libc::SIGTERM))));
    assert!(since.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 2);
}

#[test]
fn ownership_validation_rejects_foreign_uid_without_changing_permissions() {
    let fixture = Fixture::new();
    fs::write(fixture.base.join("keep"), b"foreign owner payload").unwrap();
    fs::set_permissions(&fixture.base, fs::Permissions::from_mode(0o500)).unwrap();
    let file = File::open(&fixture.base).unwrap();
    failure_reason(
        remove_tree(
            &file,
            // SAFETY: geteuid has no arguments or memory side effects.
            unsafe { libc::geteuid() }.wrapping_add(1),
            0,
            &Cancellation::default(),
        ),
        "wrong_owner",
    );
    assert_eq!(fs::metadata(&fixture.base).unwrap().mode() & 0o7777, 0o500);
    assert_eq!(
        fs::read(fixture.base.join("keep")).unwrap(),
        b"foreign owner payload"
    );
}

#[test]
fn invalid_temporary_names_and_foreign_capabilities_cannot_authorize_deletion() {
    let fixture = Fixture::new();
    let first = begin(&fixture.root);
    let second = begin(&fixture.root);
    let allocation = allocate_attempt(&first);
    fs::write(allocation.path.join("keep"), b"preserve allocation").unwrap();
    for name in [
        "../outside",
        "",
        "attempt-1",
        "snapshot-../../outside",
        "work",
        "/tmp",
        "attempt-FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF",
    ] {
        failure_reason(
            remove(
                &fixture.root,
                &first.campaign_name,
                cloned(&first.caps, true),
                name,
                &Cancellation::default(),
            ),
            "invalid_temporary_workspace_name",
        );
    }
    for part in ["root", "campaign", "work", "lease", "missing-lease"] {
        let mut caps = cloned(&first.caps, true);
        let expected = match part {
            "root" => {
                caps.root = File::open(&fixture.base).unwrap();
                // The foreign directory has no global control file.
                None
            }
            "campaign" => {
                caps.campaign = second.caps.campaign.try_clone().unwrap();
                Some("directory_changed")
            }
            "work" => {
                caps.work = second.caps.work.try_clone().unwrap();
                Some("directory_changed")
            }
            "lease" => {
                caps.lease = second
                    .caps
                    .lease
                    .as_ref()
                    .map(|file| file.try_clone().unwrap());
                Some("file_changed")
            }
            _ => {
                caps.lease = None;
                Some("missing_workspace_lease")
            }
        };
        let result = remove(
            &fixture.root,
            &first.campaign_name,
            caps,
            &allocation.name,
            &Cancellation::default(),
        );
        if let Some(expected) = expected {
            failure_reason(result, expected);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(
            fs::read(allocation.path.join("keep")).unwrap(),
            b"preserve allocation"
        );
    }
    remove_allocation(&first, &allocation).unwrap();
    assert!(!finish(second).cleanup_deferred);
    assert!(!finish(first).cleanup_deferred);
}

#[test]
fn changed_campaign_or_work_inode_is_rejected_before_mutation() {
    for part in ["campaign", "work"] {
        let fixture = Fixture::new();
        let started = begin(&fixture.root);
        let original = if part == "campaign" {
            fixture.root.join(&started.campaign_name)
        } else {
            fixture.root.join(&started.campaign_name).join("work")
        };
        let renamed = original.with_file_name(format!("renamed-{part}"));
        fs::rename(&original, &renamed).unwrap();
        private_directory(&original);
        fs::write(original.join("foreign"), b"do not delete replacement").unwrap();
        failure_reason(
            allocate(
                &fixture.root,
                &started.campaign_name,
                cloned(&started.caps, true),
                "attempt",
                &Cancellation::default(),
            ),
            "directory_changed",
        );
        failure_reason(
            close(
                &fixture.root,
                &started.campaign_name,
                cloned(&started.caps, false),
                &Cancellation::default(),
            ),
            "directory_changed",
        );
        assert_eq!(
            fs::read(original.join("foreign")).unwrap(),
            b"do not delete replacement"
        );
        fs::remove_file(original.join("foreign")).unwrap();
        fs::remove_dir(&original).unwrap();
        fs::rename(&renamed, &original).unwrap();
        assert!(!finish(started).cleanup_deferred);
    }
}

#[test]
fn missing_campaign_name_does_not_hide_a_renamed_live_directory() {
    let fixture = Fixture::new();
    let mut started = begin(&fixture.root);
    let campaign = fixture.root.join(&started.campaign_name);
    fs::write(campaign.join("work/keep"), b"renamed live input").unwrap();
    let renamed = fixture.root.join("renamed-live-campaign");
    fs::rename(&campaign, &renamed).unwrap();
    started.caps.lease.take();
    failure_reason(
        close(
            &fixture.root,
            &started.campaign_name,
            cloned(&started.caps, false),
            &Cancellation::default(),
        ),
        "directory_changed",
    );
    assert_eq!(
        fs::read(renamed.join("work/keep")).unwrap(),
        b"renamed live input"
    );
    fs::rename(&renamed, &campaign).unwrap();
    assert!(!finish(started).cleanup_deferred);
}

#[test]
fn changed_registry_is_rejected_without_writing_replacement() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    let renamed = fixture.base.join("renamed registry");
    fs::rename(&fixture.root, &renamed).unwrap();
    private_directory(&fixture.root);
    fs::write(fixture.root.join("keep"), b"foreign registry").unwrap();
    failure_reason(
        allocate(
            &fixture.root,
            &started.campaign_name,
            cloned(&started.caps, true),
            "attempt",
            &Cancellation::default(),
        ),
        "registry_changed",
    );
    failure_reason(
        close(
            &fixture.root,
            &started.campaign_name,
            cloned(&started.caps, false),
            &Cancellation::default(),
        ),
        "registry_changed",
    );
    assert_eq!(
        fs::read(fixture.root.join("keep")).unwrap(),
        b"foreign registry"
    );
    fs::remove_file(fixture.root.join("keep")).unwrap();
    fs::remove_dir(&fixture.root).unwrap();
    fs::rename(&renamed, &fixture.root).unwrap();
    assert!(!finish(started).cleanup_deferred);
}

#[test]
fn close_rejects_its_own_live_lease_and_invalid_campaign_names() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    failure_reason(
        close(
            &fixture.root,
            &started.campaign_name,
            cloned(&started.caps, true),
            &Cancellation::default(),
        ),
        "workspace_lease_not_released",
    );
    for name in [
        "../outside",
        "campaign-",
        "pending-00000000000000000000000000000001",
        "/tmp",
        "campaign-0000000000000000000000000000000G",
    ] {
        failure_reason(
            close(
                &fixture.root,
                name,
                cloned(&started.caps, false),
                &Cancellation::default(),
            ),
            "invalid_campaign_name",
        );
    }
    assert!(busy(
        &fixture.root.join(&started.campaign_name).join("lease")
    ));
    assert!(!finish(started).cleanup_deferred);
}

#[test]
#[cfg(target_os = "linux")]
fn non_unicode_registry_names_are_preserved_in_python_surrogateescape_order() {
    use std::os::unix::ffi::OsStringExt;
    let fixture = Fixture::new();
    initialized(&fixture.root);
    let names = [
        std::ffi::OsString::from("a"),
        std::ffi::OsString::from("Ж"),
        std::ffi::OsString::from_vec(vec![0x80]),
        std::ffi::OsString::from_vec(vec![0xff]),
    ];
    for name in &names {
        private_directory(&fixture.root.join(name));
    }
    let recovered = begin(&fixture.root);
    assert_eq!(recovered.skipped.len(), 4);
    for (item, expected) in recovered.skipped.iter().zip(&names) {
        let actual = item
            .name_bytes
            .clone()
            .unwrap_or_else(|| item.name.as_bytes().to_vec());
        assert_eq!(actual, expected.as_os_str().as_bytes());
        assert_eq!(item.reason, "unrecognized_registry_entry");
        assert!(fixture.root.join(expected).is_dir());
    }
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn surrogateescape_name_keys_match_python_for_invalid_and_truncated_utf8() {
    use std::os::unix::ffi::OsStrExt;
    for (bytes, expected) in [
        (b"a\xffz".as_slice(), vec![0x61, 0xdcff, 0x7a]),
        (b"\xe2(\xa1".as_slice(), vec![0xdce2, 0x28, 0xdca1]),
        (b"\xf0\x9f\x92".as_slice(), vec![0xdcf0, 0xdc9f, 0xdc92]),
        (b"\xc0\xaf".as_slice(), vec![0xdcc0, 0xdcaf]),
        ("Ж😀".as_bytes(), vec![0x416, 0x1f600]),
    ] {
        assert_eq!(
            super::fs::python_name_key(std::ffi::OsStr::from_bytes(bytes)),
            expected
        );
    }
    let mut names = [
        b"\xff".as_slice(),
        "😀".as_bytes(),
        b"a",
        b"\x80",
        "Ж".as_bytes(),
    ];
    names.sort_by_key(|bytes| super::fs::python_name_key(std::ffi::OsStr::from_bytes(bytes)));
    assert_eq!(
        names,
        [
            b"a".as_slice(),
            "Ж".as_bytes(),
            b"\x80",
            b"\xff",
            "😀".as_bytes()
        ]
    );
}

struct HookGuard;
impl HookGuard {
    fn set(hook: impl FnMut(Stage) + 'static) -> Self {
        TEST_HOOK.with(|current| {
            assert!(current.borrow().is_none(), "test hook must not be nested");
            *current.borrow_mut() = Some(Box::new(hook));
        });
        Self
    }
}
impl Drop for HookGuard {
    fn drop(&mut self) {
        TEST_HOOK.with(|current| *current.borrow_mut() = None);
    }
}

#[test]
fn interrupted_recovery_preserves_a_recoverable_disk_state_at_every_commit_boundary() {
    for phase in 0..5 {
        let fixture = Fixture::new();
        let campaign = abandoned(&fixture.root);
        fs::write(campaign.join("work/a"), b"first").unwrap();
        fs::write(campaign.join("work/b"), b"second").unwrap();
        let mut marker_count = 0;
        let hook = HookGuard::set(move |stage| {
            let crash = match (phase, stage) {
                (0, Stage::WorkEntryRemoved)
                | (1, Stage::BeforeCampaignRename)
                | (2, Stage::CampaignRenamed) => true,
                (3 | 4, Stage::MarkerRemoved) => {
                    marker_count += 1;
                    marker_count == if phase == 3 { 1 } else { 2 }
                }
                _ => false,
            };
            assert!(!crash, "injected interrupted recovery");
        });
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| begin(&fixture.root)));
        drop(hook);
        assert!(result.is_err());
        let pending =
            campaign.with_file_name(campaign.file_name().unwrap().to_str().unwrap().replacen(
                "campaign-",
                "pending-",
                1,
            ));
        if phase < 2 {
            assert!(campaign.is_dir());
            assert!(campaign.join("lease").is_file());
            assert!(campaign.join("metadata.json").is_file());
            if phase == 0 {
                assert_eq!(fs::read(campaign.join("work/b")).unwrap(), b"second");
            }
        } else {
            assert!(pending.is_dir());
            assert!(!campaign.exists());
        }
        let recovered = begin(&fixture.root);
        let expected = if phase < 2 { &campaign } else { &pending };
        assert!(
            recovered
                .recovered
                .iter()
                .any(|name| name == expected.file_name().unwrap().to_str().unwrap())
        );
        assert!(!campaign.exists() && !pending.exists());
        assert!(!finish(recovered).cleanup_deferred);
    }
}

#[test]
fn cancelled_campaign_and_allocation_commit_points_still_return_cleanup_receipts() {
    let fixture = Fixture::new();
    let cancelled = Cancellation::default();
    cancelled.request(libc::SIGTERM);
    assert!(matches!(
        start(&fixture.root, &cancelled),
        Err(InputError::Cancelled(libc::SIGTERM))
    ));
    assert!(!fixture.root.exists());
    let cancel = Cancellation::default();
    let observer = cancel.clone();
    let hook = HookGuard::set(move |stage| {
        if matches!(stage, Stage::CampaignCreated) {
            observer.request(libc::SIGTERM);
        }
    });
    let started = start(&fixture.root, &cancel).unwrap();
    drop(hook);
    assert_eq!(cancel.signum(), Some(libc::SIGTERM));
    let cancel = Cancellation::default();
    let observer = cancel.clone();
    let hook = HookGuard::set(move |stage| {
        if matches!(stage, Stage::AllocationCreated) {
            observer.request(libc::SIGINT);
        }
    });
    let allocation = allocate(
        &fixture.root,
        &started.campaign_name,
        cloned(&started.caps, true),
        "attempt",
        &cancel,
    )
    .unwrap();
    drop(hook);
    assert!(allocation.path.is_dir());
    assert_eq!(cancel.signum(), Some(libc::SIGINT));
    remove_allocation(&started, &allocation).unwrap();
    assert!(!finish(started).cleanup_deferred);
}

#[test]
fn cancellation_mid_payload_deletion_keeps_committed_markers_for_next_start() {
    let fixture = Fixture::new();
    let campaign = abandoned(&fixture.root);
    fs::write(campaign.join("work/a"), b"first").unwrap();
    fs::write(campaign.join("work/b"), b"second").unwrap();
    let cancel = Cancellation::default();
    let observer = cancel.clone();
    let hook = HookGuard::set(move |stage| {
        if matches!(stage, Stage::WorkEntryRemoved) {
            observer.request(libc::SIGTERM);
        }
    });
    assert!(matches!(
        start(&fixture.root, &cancel),
        Err(InputError::Cancelled(libc::SIGTERM))
    ));
    drop(hook);
    assert!(!campaign.join("work/a").exists());
    assert_eq!(fs::read(campaign.join("work/b")).unwrap(), b"second");
    assert_eq!(fs::read(campaign.join("lease")).unwrap(), EXPECTED_LEASE);
    assert!(campaign.join("metadata.json").is_file());
    let recovered = begin(&fixture.root);
    assert!(!campaign.exists());
    assert!(!finish(recovered).cleanup_deferred);
}

#[test]
fn renamed_empty_campaign_outside_registry_is_not_reported_as_already_reaped() {
    let fixture = Fixture::new();
    let mut started = begin(&fixture.root);
    let campaign = fixture.root.join(&started.campaign_name);
    started.caps.lease.take();
    for name in ["lease", "metadata.json"] {
        fs::remove_file(campaign.join(name)).unwrap();
    }
    fs::remove_dir(campaign.join("work")).unwrap();
    let moved = fixture.base.join("moved empty campaign");
    fs::rename(&campaign, &moved).unwrap();
    assert!(!directory_unlinked(&started.caps.campaign).unwrap());
    failure_reason(
        close(
            &fixture.root,
            &started.campaign_name,
            started.caps,
            &Cancellation::default(),
        ),
        "directory_changed",
    );
    assert!(moved.is_dir());
}

#[test]
fn unlinked_directory_capability_is_distinguished_from_replacement_at_stale_path() {
    let fixture = Fixture::new();
    let path = fixture.base.join("original directory");
    private_directory(&path);
    let original = File::open(&path).unwrap();
    assert!(!directory_unlinked(&original).unwrap());
    fs::remove_dir(&path).unwrap();
    assert!(directory_unlinked(&original).unwrap());
    private_directory(&path);
    fs::write(path.join("keep"), b"replacement stays").unwrap();
    assert!(directory_unlinked(&original).unwrap());
    assert_eq!(fs::read(path.join("keep")).unwrap(), b"replacement stays");
}

#[test]
fn own_campaign_invalid_mode_and_missing_work_return_deferred_legacy_reasons() {
    for kind in ["mode", "missing-work"] {
        let fixture = Fixture::new();
        let started = begin(&fixture.root);
        let name = started.campaign_name.clone();
        let campaign = fixture.root.join(&name);
        if kind == "mode" {
            fs::set_permissions(&campaign, fs::Permissions::from_mode(0o750)).unwrap();
        } else {
            fs::remove_dir(campaign.join("work")).unwrap();
        }
        let report = finish(started);
        let expected = if kind == "mode" {
            "invalid_directory_permissions"
        } else {
            "missing_campaign_work_directory"
        };
        assert!(report.cleanup_deferred);
        assert!(
            report
                .skipped
                .iter()
                .any(|item| item.name == name && item.reason == expected)
        );
        assert!(campaign.join("lease").is_file());
        assert!(campaign.join("metadata.json").is_file());
    }
}

#[test]
fn reopening_the_same_lease_inode_does_not_substitute_its_locked_open_description() {
    let fixture = Fixture::new();
    let started = begin(&fixture.root);
    let allocation = allocate_attempt(&started);
    fs::write(
        allocation.path.join("keep"),
        b"original lease protects this",
    )
    .unwrap();
    let path = fixture.root.join(&started.campaign_name).join("lease");
    for removing in [false, true] {
        let mut caps = cloned(&started.caps, true);
        caps.lease = Some(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        );
        if removing {
            failure_reason(
                remove(
                    &fixture.root,
                    &started.campaign_name,
                    caps,
                    &allocation.name,
                    &Cancellation::default(),
                ),
                "invalid_workspace_lease",
            );
        } else {
            failure_reason(
                allocate(
                    &fixture.root,
                    &started.campaign_name,
                    caps,
                    "attempt",
                    &Cancellation::default(),
                ),
                "invalid_workspace_lease",
            );
        }
        assert_eq!(
            fs::read(allocation.path.join("keep")).unwrap(),
            b"original lease protects this"
        );
        assert_eq!(
            fs::read_dir(allocation.path.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        assert!(busy(&path));
    }
    remove_allocation(&started, &allocation).unwrap();
    assert!(!finish(started).cleanup_deferred);
}
