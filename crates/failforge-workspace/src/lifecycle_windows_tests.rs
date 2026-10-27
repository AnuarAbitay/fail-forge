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
