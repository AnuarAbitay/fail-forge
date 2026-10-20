//! An allocation whose directory identity stays bound until explicit cleanup.
use super::*;

/// Retains the campaign lease and the allocated directory inode. There is no
/// destructive Drop: a process owner must drain its children before removal.
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
    let _guard = validate_caps(root_path, campaign_name, &caps, cancel)?;
    cancel.check()?;
    let name = format!("{kind}-{}", random_token()?);
    mkdir_at(caps.work.as_raw_fd(), OsStr::new(&name))?;
    let directory = directory(caps.work.as_raw_fd(), OsStr::new(&name), uid(), true)?;
    let allocation = Allocation {
        path: root_path.join(campaign_name).join(WORK).join(&name),
        name,
    };
    #[cfg(test)]
    stage(Stage::AllocationCreated);
    // An allocation that committed must reach its caller even on cancellation.
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
        directory_info(&info(&self.directory)?, uid(), true)?;
        same_at(
            self.caps.work.as_raw_fd(),
            OsStr::new(self.name()),
            &self.directory,
            "workspace_allocation_changed",
        )
    }

    /// Remove only this retained inode, after the caller's native children have
    /// fully stopped. A failed cleanup remains retryable and retains its lease.
    pub fn remove(&mut self, cancel: &Cancellation) -> Result<(), InputError> {
        if self.removed {
            return Ok(());
        }
        let guard = validate_caps(&self.root_path, &self.campaign_name, &self.caps, cancel)?;
        directory_info(&info(&self.directory)?, uid(), true)?;
        match same_at(
            self.caps.work.as_raw_fd(),
            OsStr::new(self.name()),
            &self.directory,
            "workspace_allocation_changed",
        ) {
            Err(InputError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                require(
                    names(&self.directory)?.is_empty() && directory_unlinked(&self.directory)?,
                    "workspace_allocation_changed",
                )?;
                self.removed = true;
                return Ok(());
            }
            other => other?,
        }
        // The live lease protects the campaign without serializing other
        // workers behind the recursive payload walk.
        drop(guard);
        validate_tree(&self.directory, uid(), 0, cancel)?;
        remove_tree(&self.directory, uid(), 0, cancel)?;
        same_at(
            self.caps.work.as_raw_fd(),
            OsStr::new(self.name()),
            &self.directory,
            "workspace_allocation_changed",
        )?;
        unlink(&self.caps.work, OsStr::new(self.name()), true)?;
        self.removed = true;
        Ok(())
    }

    pub(super) fn into_unbound(self) -> Allocation {
        self.allocation
    }
}
