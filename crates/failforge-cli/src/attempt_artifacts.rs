//! Descriptor-anchored creation of a fresh attempt report and its parents.
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use failforge_dart::namespace::NamespaceError;

use crate::dart_namespace::invalid;
use crate::namespace_planner::{ReportDirectory, create_report_directory};

pub(crate) fn create(
    path: &Path,
    protected: &[PathBuf],
) -> Result<ReportDirectory, NamespaceError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("invalid attempt report path"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("missing attempt report parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("missing attempt report name"))?;
    // Resolve pre-existing platform aliases once (e.g. macOS /tmp). Every new
    // descendant is then opened relative to a retained directory, without links.
    let mut ancestor = parent;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| invalid("invalid report parent"))?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| invalid("invalid report parent"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut current = ancestor.canonicalize()?;
    let output = missing
        .iter()
        .rev()
        .fold(current.clone(), |path, name| path.join(name))
        .join(name);
    if protected
        .iter()
        .any(|root| output.starts_with(root) || root.starts_with(&output))
    {
        return Err(invalid("attempt report overlaps protected inputs"));
    }
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&current)?;
    for name in missing.into_iter().rev() {
        let name_c = CString::new(name.as_bytes()).map_err(|_| invalid("invalid report parent"))?;
        verify_directory(&directory, &current)?;
        // SAFETY: a single validated component is relative to a retained dir FD.
        if unsafe { libc::mkdirat(directory.as_raw_fd(), name_c.as_ptr(), 0o700) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        // SAFETY: the returned unique descriptor cannot follow a replacement link.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returns a unique live descriptor.
        let next = unsafe { File::from_raw_fd(fd) };
        verify_directory(&directory, &current)?;
        current.push(name);
        verify_directory(&next, &current)?;
        directory = next;
    }
    verify_directory(&directory, &current)?;
    // Revalidate the caller-visible parent so a replaced ancestor cannot move
    // creation to a foreign directory between this walk and report binding.
    if parent.canonicalize()? != current {
        return Err(invalid("attempt report parent changed"));
    }
    // Preserve the caller's spelling as part of the binding. Managed child
    // paths use the verified canonical accessor, while verify still detects a
    // retargeted pre-existing alias in the original request.
    let report = create_report_directory(path, protected)?;
    verify_directory(&directory, &current)?;
    Ok(report)
}

fn verify_directory(file: &File, path: &Path) -> Result<(), NamespaceError> {
    let held = file.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !named.is_dir() || held.dev() != named.dev() || held.ino() != named.ino() {
        return Err(invalid("attempt report parent changed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "ff-attempt-artifacts-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nested_creation_is_private_fresh_and_anchored() {
        let root = Temp::new();
        let path = root.path().join("mutants/id/attempt");
        let report = create(&path, &[]).unwrap();
        report
            .write_json("result.json", &serde_json::json!({"outcome":"SURVIVED"}))
            .unwrap();
        assert_eq!(
            fs::metadata(path.join("result.json")).unwrap().mode() & 0o777,
            0o600
        );
        assert!(create(&path, &[]).is_err());
        let moved = root.path().join("moved");
        fs::rename(path.parent().unwrap(), &moved).unwrap();
        fs::create_dir_all(&path).unwrap();
        assert!(report.write_private("other", b"secret").is_err());
        assert!(!path.join("other").exists());
        assert!(!moved.join("attempt/other").exists());
    }

    #[test]
    fn protected_aliases_are_rejected_before_any_directory_creation() {
        let root = Temp::new();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        let alias = root.path().join("alias");
        symlink(&source, &alias).unwrap();
        assert!(
            create(
                &alias.join("nested/attempt"),
                &[source.canonicalize().unwrap()]
            )
            .is_err()
        );
        assert!(!source.join("nested").exists());
        assert!(create(&root.path().join("a/../report"), &[]).is_err());
    }
}
