use super::*;
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "failforge-lifecycle-windows-{}",
            win::random_token().unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn active_shared_lease_defers_cleanup_and_stale_campaign_is_recovered() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let cancel = Cancellation::default();
    let mut first = start(&root, &cancel).unwrap();
    let mut close_caps = first.caps.try_clone().unwrap();
    close_caps.lease.take();
    assert!(
        close(&root, &first.campaign_name, close_caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
    let mut second = start(&root, &cancel).unwrap();
    assert!(
        second
            .skipped
            .iter()
            .any(|skip| skip.name == first.campaign_name && skip.reason == "active_lease")
    );
    first.caps.lease.take();
    let third = start(&root, &cancel).unwrap();
    assert!(third.recovered.contains(&first.campaign_name));
    second.caps.lease.take();
    assert!(
        !close(&root, &second.campaign_name, second.caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
    let mut caps = third.caps;
    caps.lease.take();
    assert!(
        !close(&root, &third.campaign_name, caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
}
#[test]
fn allocated_directory_is_bound_and_removal_is_idempotent() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let cancel = Cancellation::default();
    let mut campaign = start(&root, &cancel).unwrap();
    let mut allocation = allocate_bound(
        &root,
        &campaign.campaign_name,
        campaign.caps.try_clone().unwrap(),
        "attempt",
        &cancel,
    )
    .unwrap();
    std::fs::create_dir(allocation.path().join("nested")).unwrap();
    std::fs::write(allocation.path().join("nested/payload"), "data").unwrap();
    allocation.verify(&cancel).unwrap();
    allocation.remove(&cancel).unwrap();
    allocation.remove(&cancel).unwrap();
    assert!(!allocation.path().exists());
    drop(allocation);
    campaign.caps.lease.take();
    assert!(
        !close(&root, &campaign.campaign_name, campaign.caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
}
#[test]
fn replaced_allocation_is_not_removed() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let cancel = Cancellation::default();
    let campaign = start(&root, &cancel).unwrap();
    let mut allocation = allocate_bound(
        &root,
        &campaign.campaign_name,
        campaign.caps.try_clone().unwrap(),
        "attempt",
        &cancel,
    )
    .unwrap();
    let moved = allocation.path().with_extension("moved");
    std::fs::rename(allocation.path(), &moved).unwrap();
    std::fs::create_dir(allocation.path()).unwrap();
    std::fs::write(allocation.path().join("keep"), "foreign replacement").unwrap();
    assert!(allocation.remove(&cancel).is_err());
    assert_eq!(
        std::fs::read(allocation.path().join("keep")).unwrap(),
        b"foreign replacement"
    );
}
#[test]
fn hostile_registry_entries_and_hardlinked_control_files_are_retained() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let cancel = Cancellation::default();
    let mut first = start(&root, &cancel).unwrap();
    std::fs::write(root.join("foreign"), "keep").unwrap();
    let lease = root.join(&first.campaign_name).join(LEASE);
    std::fs::hard_link(&lease, f.0.join("external-lease")).unwrap();
    first.caps.lease.take();
    let second = start(&root, &cancel).unwrap();
    assert!(second.skipped.iter().any(|skip| skip.name == "foreign"));
    assert!(
        second
            .skipped
            .iter()
            .any(|skip| skip.name == first.campaign_name)
    );
    assert_eq!(std::fs::read(root.join("foreign")).unwrap(), b"keep");
    assert!(lease.exists());
}
#[test]
fn source_lease_is_shared_with_independent_runner_owner() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let cancel = Cancellation::default();
    let mut campaign = start(&root, &cancel).unwrap();
    let owner = failforge_runner::WorkspaceLease::from_borrowed(
        campaign.caps.lease.as_ref().unwrap().as_handle(),
    )
    .unwrap();
    campaign.caps.lease.take();
    let mut observer = start(&root, &cancel).unwrap();
    assert!(
        observer
            .skipped
            .iter()
            .any(|skip| skip.name == campaign.campaign_name && skip.reason == "active_lease")
    );
    drop(owner);
    assert!(
        !close(&root, &campaign.campaign_name, campaign.caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
    observer.caps.lease.take();
    assert!(
        !close(&root, &observer.campaign_name, observer.caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
}

#[test]
fn contended_registry_lock_bounds_recovery_without_removing_another_owner() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let cancel = Cancellation::default();
    let mut campaign = start(&root, &cancel).unwrap();
    campaign.caps.lease.take();
    let guard = regular(&campaign.caps.root, GLOBAL, true, false).unwrap();
    assert!(win::try_lock(&guard).unwrap());

    let began = std::time::Instant::now();
    let error = start(&root, &cancel).unwrap_err();
    assert!(error.to_string().contains("workspace_registry_busy"));
    assert!(began.elapsed() >= std::time::Duration::from_secs(2));
    assert!(began.elapsed() < std::time::Duration::from_secs(5));
    assert!(root.join(&campaign.campaign_name).exists());
    // A failed attempt must not release the lock belonging to the other handle.
    let contender = regular(&campaign.caps.root, GLOBAL, true, false).unwrap();
    assert!(!win::try_lock(&contender).unwrap());
    drop(guard);
    drop(contender);

    let mut recovered = start(&root, &cancel).unwrap();
    assert!(recovered.recovered.contains(&campaign.campaign_name));
    recovered.caps.lease.take();
    assert!(
        !close(&root, &recovered.campaign_name, recovered.caps, &cancel)
            .unwrap()
            .cleanup_deferred
    );
}

#[test]
fn contended_registry_lock_still_observes_cancellation() {
    let f = Fixture::new();
    let root = f.0.join("registry");
    let campaign = start(&root, &Cancellation::default()).unwrap();
    let guard = regular(&campaign.caps.root, GLOBAL, true, false).unwrap();
    assert!(win::try_lock(&guard).unwrap());
    let cancel = Cancellation::default();
    let request = cancel.clone();
    let sender = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        request.request(libc::SIGINT);
    });
    let began = std::time::Instant::now();
    assert!(matches!(
        start(&root, &cancel),
        Err(InputError::Cancelled(libc::SIGINT))
    ));
    assert!(began.elapsed() < std::time::Duration::from_secs(1));
    sender.join().unwrap();
}

#[test]
fn cleanup_recovers_after_each_committed_control_transition() {
    for (pending, interrupted) in [
        (false, "marked"),
        (false, "work_removed"),
        (false, "lease_removed"),
        (false, "cleanup_removed"),
        (true, "work_removed"),
        (true, "metadata_removed"),
        (true, "lease_removed"),
    ] {
        let f = Fixture::new();
        let root = f.0.join("registry");
        let cancel = Cancellation::default();
        let mut campaign = start(&root, &cancel).unwrap();
        let mut name = campaign.campaign_name.clone();
        let work = root.join(&name).join(WORK);
        if !pending {
            std::fs::create_dir(work.join("nested")).unwrap();
            std::fs::write(work.join("nested").join("payload"), "owned payload").unwrap();
        }
        campaign.caps.lease.take();
        drop(campaign.caps);
        let directory = win::directory(&root).unwrap();
        let guard = global_lock(&root, &directory, &cancel).unwrap();
        if pending {
            let held = win::directory_at(&directory, OsStr::new(&name)).unwrap();
            let next = name.replacen("campaign-", "pending-", 1);
            win::rename_handle(&held, &directory, OsStr::new(&next), false).unwrap();
            name = next;
        }
        let result = reap_with_hook(&directory, &name, &cancel, &mut |stage| {
            if stage == interrupted {
                Err(failure("simulated_cleanup_crash"))
            } else {
                Ok(())
            }
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("simulated_cleanup_crash")
        );
        drop(guard);

        let mut recovered = start(&root, &cancel).unwrap();
        assert!(
            recovered.recovered.contains(&name),
            "{interrupted}: {:?}",
            recovered.skipped
        );
        assert!(!root.join(&name).exists(), "{interrupted}");
        recovered.caps.lease.take();
        assert!(
            !close(&root, &recovered.campaign_name, recovered.caps, &cancel)
                .unwrap()
                .cleanup_deferred
        );
    }
}

#[test]
fn missing_controls_and_invalid_cleanup_marker_never_authorize_payload_removal() {
    for corrupt_marker in [false, true] {
        let f = Fixture::new();
        let root = f.0.join("registry");
        let cancel = Cancellation::default();
        let mut campaign = start(&root, &cancel).unwrap();
        let name = campaign.campaign_name.clone();
        let payload = root.join(&name).join(WORK).join("keep");
        std::fs::write(&payload, "retained payload").unwrap();
        campaign.caps.lease.take();
        if corrupt_marker {
            let metadata = regular(&campaign.caps.campaign, METADATA, true, false).unwrap();
            win::rename_handle(
                &metadata,
                &campaign.caps.campaign,
                OsStr::new(CLEANUP),
                false,
            )
            .unwrap();
            metadata.set_len(0).unwrap();
            write_marker(&metadata, b"invalid cleanup proof").unwrap();
        } else {
            let lease = regular(&campaign.caps.campaign, LEASE, true, false).unwrap();
            win::remove_handle(&lease).unwrap();
        }
        drop(campaign.caps);
        let observer = start(&root, &cancel).unwrap();
        assert!(observer.skipped.iter().any(|skip| skip.name == name));
        assert_eq!(std::fs::read(&payload).unwrap(), b"retained payload");
    }
}
