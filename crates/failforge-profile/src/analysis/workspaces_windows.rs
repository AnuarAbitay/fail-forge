//! Profile workspaces use the shared native Windows registry/lease protocol.
use super::failure;
use crate::ProfileError;
use failforge_workspace::{Cancellation, lifecycle as native, windows_fs as win};
use std::os::windows::io::AsHandle;
use std::path::{Path, PathBuf};

pub(super) struct Workspace {
    registry: PathBuf,
    campaign_name: String,
    caps: Option<native::LifecycleCaps>,
    pub path: PathBuf,
}
impl Workspace {
    pub fn new(protected: &[PathBuf]) -> Result<Self, ProfileError> {
        let registry = std::env::temp_dir().canonicalize()?.join(format!(
            "failforge-profile-workspaces-{}",
            win::current_sid()?
        ));
        Self::at(&registry, protected)
    }
    fn at(registry: &Path, protected: &[PathBuf]) -> Result<Self, ProfileError> {
        if protected
            .iter()
            .any(|path| registry.starts_with(path) || path.starts_with(registry))
        {
            return Err(failure(
                "temporary workspace registry must be outside analysis inputs",
            ));
        }
        let start = native::start(registry, &Cancellation::default())?;
        Ok(Self {
            path: start.root_path.join(&start.campaign_name).join("work"),
            registry: start.root_path,
            campaign_name: start.campaign_name,
            caps: Some(start.caps),
        })
    }
    pub fn native_lease(&self) -> Result<failforge_runner::WorkspaceLease, ProfileError> {
        let lease = self
            .caps
            .as_ref()
            .and_then(|caps| caps.lease.as_ref())
            .ok_or_else(|| failure("workspace lease already closed"))?;
        Ok(failforge_runner::WorkspaceLease::from_borrowed(
            lease.as_handle(),
        )?)
    }
    pub fn recovery(&self) -> Result<Recovery, ProfileError> {
        let mut caps = self
            .caps
            .as_ref()
            .ok_or_else(|| failure("workspace already closed"))?
            .try_clone()?;
        // A recovery capability identifies objects but must not hold a lease,
        // otherwise it would itself prevent proving every native owner exited.
        caps.lease.take();
        Ok(Recovery {
            registry: self.registry.clone(),
            campaign_name: self.campaign_name.clone(),
            caps,
        })
    }
    pub fn close(&mut self) -> Result<(), ProfileError> {
        let Some(mut caps) = self.caps.take() else {
            return Ok(());
        };
        caps.lease.take();
        let report = native::close(
            &self.registry,
            &self.campaign_name,
            caps,
            &Cancellation::default(),
        )?;
        if report.cleanup_deferred {
            return Err(failure(
                "applicability workspace still has an active or unverified owner",
            ));
        }
        Ok(())
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub(super) struct Recovery {
    registry: PathBuf,
    campaign_name: String,
    caps: native::LifecycleCaps,
}
impl Recovery {
    pub fn recover(self) -> Result<(), ProfileError> {
        match native::close(
            &self.registry,
            &self.campaign_name,
            self.caps,
            &Cancellation::default(),
        ) {
            Ok(report) if !report.cleanup_deferred => Ok(()),
            Ok(_) => Err(failure("native profile workspace recovery deferred")),
            Err(failforge_workspace::InputError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_registry_isolated_and_explicit_close_removes_payload() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-profile-test-{}",
            win::random_token().unwrap()
        ));
        let mut workspace = Workspace::at(&root, &[]).unwrap();
        let path = workspace.path.clone();
        std::fs::write(path.join("payload"), b"profile snapshot").unwrap();
        workspace.close().unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn native_shared_lease_defers_cleanup_until_owner_exits() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-profile-test-{}",
            win::random_token().unwrap()
        ));
        let mut workspace = Workspace::at(&root, &[]).unwrap();
        let owner = workspace.native_lease().unwrap();
        let recovery = workspace.recovery().unwrap();
        let path = workspace.path.clone();
        assert!(workspace.close().is_err());
        assert!(path.is_dir());
        drop(owner);
        recovery.recover().unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
