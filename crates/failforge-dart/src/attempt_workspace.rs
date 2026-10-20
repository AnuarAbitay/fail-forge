//! Fresh attempt preparation with explicit cleanup after native process drain.
//!
//! The caller owns process ordering: no Drop implementation removes a workspace
//! whose Runner may still have active children. Failed preparation remains owned
//! and must pass through the same explicit cleanup path as a completed attempt.
use std::path::{Path, PathBuf};

use failforge_workspace::lifecycle::{BoundAllocation, LifecycleCaps, allocate_bound};
use failforge_workspace::{
    Cancellation, Hashes, PreparedProject, copy_project, inspect_project, source_hashes,
    verify_source,
};

use crate::namespace::NamespaceError;
use crate::namespace::fs::Root;
use crate::{Mutant, apply_mutant, sha256_hex};

const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Copied,
    PatchWritten,
}
#[cfg(test)]
type Hook = Box<dyn FnMut(Stage) -> Result<(), NamespaceError>>;
#[cfg(test)]
thread_local! { static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn stage(value: Stage) -> Result<(), NamespaceError> {
    HOOK.with(|slot| match slot.borrow_mut().as_mut() {
        Some(hook) => hook(value),
        None => Ok(()),
    })
}

fn invalid(message: impl Into<String>) -> NamespaceError {
    NamespaceError::Invalid(message.into())
}

fn require(value: bool, message: &'static str) -> Result<(), NamespaceError> {
    if value { Ok(()) } else { Err(invalid(message)) }
}

struct Original {
    root: Root,
    hashes: Hashes,
}

pub struct AttemptWorkspace {
    allocation: BoundAllocation,
    copy_path: PathBuf,
    original: Option<Original>,
    copied: Option<Root>,
    execution: Option<Hashes>,
    patched: bool,
}

impl AttemptWorkspace {
    pub fn allocate(
        root: &Path,
        campaign: &str,
        caps: LifecycleCaps,
        cancel: &Cancellation,
    ) -> Result<Self, NamespaceError> {
        let allocation = allocate_bound(root, campaign, caps, "attempt", cancel)?;
        let copy_path = allocation.path().join("package");
        Ok(Self {
            allocation,
            copy_path,
            original: None,
            copied: None,
            execution: None,
            patched: false,
        })
    }

    pub fn copy_path(&self) -> &Path {
        &self.copy_path
    }

    /// Protect the actual new allocation, allowing the source snapshot to be a
    /// sibling inside the same registry. Runtime SDK/adapter roots can be passed
    /// here by the owner before copying, in addition to the checked dependencies.
    pub fn protect_inputs(&self, roots: &[PathBuf]) -> Result<(), NamespaceError> {
        let token = Cancellation::default();
        self.allocation.verify(&token)?;
        let allocation = self.allocation.path().canonicalize()?;
        for root in roots {
            require(
                root.is_absolute(),
                "attempt protected roots must be absolute",
            )?;
            let root = root.canonicalize()?;
            require(
                !allocation.starts_with(&root) && !root.starts_with(&allocation),
                "attempt allocation overlaps protected inputs",
            )?;
        }
        self.allocation.verify(&token)?;
        Ok(())
    }

    pub fn copy(
        &mut self,
        expected: &PreparedProject,
        cancel: &Cancellation,
    ) -> Result<PreparedProject, NamespaceError> {
        cancel.check()?;
        self.allocation.verify(cancel)?;
        require(
            self.original.is_none() && self.copied.is_none(),
            "attempt copy was already prepared",
        )?;
        let root = Root::open(&expected.root)?;
        let hashes = source_hashes(&root.path, cancel)?;
        root.verify()?;
        self.original = Some(Original { root, hashes });
        let result = (|| {
            let actual = inspect_project(&expected.root, cancel)?;
            require(
                actual == *expected,
                "attempt prepared project differs from inspected inputs",
            )?;
            let protected: Vec<_> = std::iter::once(actual.root.clone())
                .chain(actual.packages.values().cloned())
                .collect();
            self.protect_inputs(&protected)?;
            self.verify_original()?;
            let copied = copy_project(&actual.root, &self.copy_path, cancel)?;
            #[cfg(test)]
            stage(Stage::Copied)?;
            self.allocation.verify(cancel)?;
            self.copied = Some(Root::open(&self.copy_path)?);
            Ok(copied)
        })();
        // Source drift is never hidden by a copy error or latched cancellation.
        self.verify_original()?;
        result
    }

    pub fn apply_patch(
        &mut self,
        mutant: &Mutant,
        expected_patch_hash: &str,
        cancel: &Cancellation,
    ) -> Result<(), NamespaceError> {
        cancel.check()?;
        self.allocation.verify(cancel)?;
        require(
            !self.patched && self.execution.is_none(),
            "attempt mutation was already applied or sealed",
        )?;
        require(
            mutant.file.ends_with(".dart"),
            "attempt mutation target must be a Dart source",
        )?;
        let original = self
            .original
            .as_ref()
            .ok_or_else(|| invalid("attempt source was not prepared"))?;
        let copied = self
            .copied
            .as_ref()
            .ok_or_else(|| invalid("attempt copy was not prepared"))?;
        require(
            original.hashes.get(Path::new(&mutant.file)) == Some(&mutant.source_hash),
            "attempt mutation source is outside the original binding",
        )?;
        let (source, _) = original.root.read(&mutant.file, MAX_SOURCE_BYTES, false)?;
        let patched = apply_mutant(&source, mutant).map_err(|error| invalid(error.to_string()))?;
        require(
            patched.len() <= MAX_SOURCE_BYTES,
            "attempt patched source exceeds its byte limit",
        )?;
        require(
            sha256_hex(&patched) == expected_patch_hash,
            "attempt patch hash differs from the validated patch",
        )?;
        let (copy_raw, mut target) = copied.read(&mutant.file, MAX_SOURCE_BYTES, true)?;
        require(
            copy_raw == source,
            "attempt copied mutation source differs from original",
        )?;
        self.verify_original()?;
        cancel.check()?;
        let generation = target.generation();
        let result = (|| {
            target.replace(copied, &patched)?;
            #[cfg(test)]
            stage(Stage::PatchWritten)?;
            self.verify_original()?;
            self.allocation.verify(&Cancellation::default())?;
            cancel.check()?;
            Ok(())
        })();
        if let Err(primary) = result {
            if target.generation() != generation {
                if let Err(rollback) = target.restore(copied, &patched, &copy_raw) {
                    return Err(invalid(format!(
                        "attempt patch rollback failed after {primary}: {rollback}"
                    )));
                }
            }
            return Err(primary);
        }
        self.patched = true;
        Ok(())
    }

    pub fn seal(&mut self, cancel: &Cancellation) -> Result<Hashes, NamespaceError> {
        cancel.check()?;
        self.allocation.verify(cancel)?;
        require(
            self.execution.is_none(),
            "attempt execution inputs were already sealed",
        )?;
        let copied = self
            .copied
            .as_ref()
            .ok_or_else(|| invalid("attempt copy was not prepared"))?;
        copied.verify()?;
        self.verify_original()?;
        let hashes = source_hashes(&self.copy_path, cancel)?;
        copied.verify()?;
        self.execution = Some(hashes.clone());
        Ok(hashes)
    }

    pub fn verify_execution_inputs(&self) -> Result<(), NamespaceError> {
        let token = Cancellation::default();
        self.allocation.verify(&token)?;
        let copied = self
            .copied
            .as_ref()
            .ok_or_else(|| invalid("attempt copy was not prepared"))?;
        let hashes = self
            .execution
            .as_ref()
            .ok_or_else(|| invalid("attempt execution inputs were not sealed"))?;
        copied.verify()?;
        verify_source(&self.copy_path, hashes, &token)?;
        copied.verify()?;
        Ok(())
    }

    pub fn verify_original(&self) -> Result<(), NamespaceError> {
        if let Some(original) = &self.original {
            original.root.verify()?;
            verify_source(
                &original.root.path,
                &original.hashes,
                &Cancellation::default(),
            )?;
            original.root.verify()?;
        }
        Ok(())
    }

    /// Call only after Runner shutdown. Cancellation cannot skip cleanup and a
    /// changed allocation inode is reported without deleting its replacement.
    pub fn cleanup(&mut self) -> Result<(), NamespaceError> {
        self.allocation.remove(&Cancellation::default())?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "attempt_workspace_tests.rs"]
mod tests;
