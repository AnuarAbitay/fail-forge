//! Create only ordinary private Windows directories relative to held handles.
use crate::dart_namespace::invalid;
use crate::namespace_planner::{ReportDirectory, create_report_directory};
use failforge_dart::namespace::NamespaceError;
use failforge_workspace::windows_fs as win;
use std::fs;
use std::path::{Component, Path, PathBuf};

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
    let mut ancestor = parent;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| invalid("invalid report parent"))?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| invalid("invalid report parent"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut current = ancestor.canonicalize()?;
    let mut directory = win::directory(&current)?;
    let output = missing
        .iter()
        .rev()
        .fold(current.clone(), |path, name| path.join(name))
        .join(path.file_name().unwrap());
    // No report directory or intermediate parent may be created inside an input.
    if crate::namespace_planner::protected_overlap(&current, protected)?
        || protected.iter().any(|root| root.starts_with(&output))
    {
        return Err(invalid("attempt report overlaps protected inputs"));
    }
    for name in missing.into_iter().rev() {
        verify(&directory, &current)?;
        let next = match win::create_directory_at(&directory, &name) {
            Ok(next) => next,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                win::directory_at(&directory, &name)?
            }
            Err(error) => return Err(error.into()),
        };
        verify(&directory, &current)?;
        current.push(name);
        verify(&next, &current)?;
        directory = next;
    }
    verify(&directory, &current)?;
    if parent.canonicalize()? != current {
        return Err(invalid("attempt report parent changed"));
    }
    let report = create_report_directory(path, protected)?;
    verify(&directory, &current)?;
    Ok(report)
}
fn verify(file: &std::fs::File, path: &Path) -> Result<(), NamespaceError> {
    if win::identity(&win::directory(path)?)? != win::identity(file)? {
        return Err(invalid("attempt report parent changed"));
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_native_reports_are_private_and_never_reused() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-attempt-test-{}",
            win::random_token().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("mutants/id/attempt");
        let report = create(&path, &[]).unwrap();
        report.write_private("result.json", b"complete").unwrap();
        win::require_private(&win::directory(&path).unwrap(), true).unwrap();
        assert!(create(&path, &[]).is_err());
        // PathBuf::join normalizes traversal beneath verbatim Windows roots.
        // Preserve the actual external spelling so validation sees the input.
        let mut traversal = root.as_os_str().to_os_string();
        traversal.push(r"\a\..\bad");
        assert!(create(std::path::Path::new(&traversal), &[]).is_err());
        assert!(
            create(
                &root.join("protected/reports/attempt"),
                std::slice::from_ref(&root)
            )
            .is_err()
        );
        assert!(!root.join("protected").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
