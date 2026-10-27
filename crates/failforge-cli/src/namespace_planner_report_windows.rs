//! Private Windows report artifacts anchored to retained directory handles.
use crate::dart_namespace::invalid;
use failforge_dart::namespace::NamespaceError;
use failforge_workspace::windows_fs as win;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) struct ReportDirectory {
    path: PathBuf,
    parent_path: PathBuf,
    parent: File,
    name: OsString,
    file: File,
}
impl ReportDirectory {
    pub(crate) fn verify_file(&self, name: &str, file: &File) -> Result<(), NamespaceError> {
        validate_name(OsStr::new(name))?;
        self.verify()?;
        win::same_entry(&self.file, OsStr::new(name), file)?;
        win::require_private(file, false)?;
        Ok(())
    }
    pub(crate) fn canonical_path(&self) -> Result<PathBuf, NamespaceError> {
        self.verify()?;
        Ok(self.parent_path.join(&self.name))
    }
    pub(crate) fn verify(&self) -> Result<(), NamespaceError> {
        if win::identity(&win::directory(&self.parent_path)?)? != win::identity(&self.parent)?
            || win::identity(&win::directory(&self.path)?)? != win::identity(&self.file)?
        {
            return Err(invalid("namespace report directory changed"));
        }
        win::same_entry(&self.parent, &self.name, &self.file)?;
        win::require_private(&self.file, true)?;
        Ok(())
    }
    pub(super) fn remove_owned(&self, name: &OsStr, file: &File) -> Result<(), NamespaceError> {
        validate_name(name)?;
        match win::same_entry(&self.file, name, file) {
            Ok(()) => win::remove_at(&self.file, name, false)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    pub(crate) fn write_private(&self, name: &str, raw: &[u8]) -> Result<File, NamespaceError> {
        validate_name(OsStr::new(name))?;
        self.verify()?;
        let mut file = win::regular_at(&self.file, OsStr::new(name), true, true)?;
        let result = (|| {
            file.write_all(raw)?;
            file.sync_all()?;
            self.verify_file(name, &file)
        })();
        if let Err(error) = result {
            let _ = self.remove_owned(OsStr::new(name), &file);
            return Err(error);
        }
        Ok(file)
    }
    pub(crate) fn write_json(&self, name: &str, value: &Value) -> Result<File, NamespaceError> {
        let mut raw =
            serde_json::to_vec_pretty(value).map_err(|_| invalid("invalid namespace receipt"))?;
        raw.push(b'\n');
        self.write_private(name, &raw)
    }
}
fn validate_name(name: &OsStr) -> Result<(), NamespaceError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .as_encoded_bytes()
            .iter()
            .any(|byte| matches!(byte, 0 | b'/' | b'\\' | b':'))
    {
        return Err(invalid("invalid namespace artifact name"));
    }
    Ok(())
}
/// Identity comparison avoids drive-letter case and alternative spelling bypasses.
pub(crate) fn protected_overlap(
    parent: &Path,
    protected: &[PathBuf],
) -> Result<bool, NamespaceError> {
    let ancestors: Vec<_> = parent
        .ancestors()
        .map(|path| win::directory(path).and_then(|file| win::identity(&file)))
        .collect::<Result<_, _>>()?;
    for root in protected {
        let file = if root.is_dir() {
            win::directory(root)?
        } else {
            win::regular(root, false)?
        };
        if ancestors.contains(&win::identity(&file)?) {
            return Ok(true);
        }
    }
    Ok(false)
}
pub(crate) fn create_report_directory(
    directory: &Path,
    protected: &[PathBuf],
) -> Result<ReportDirectory, NamespaceError> {
    let requested_parent = directory
        .parent()
        .ok_or_else(|| invalid("invalid namespace report directory"))?;
    let parent_path = requested_parent.canonicalize()?;
    let name = directory
        .file_name()
        .ok_or_else(|| invalid("invalid namespace report directory"))?
        .to_owned();
    validate_name(&name)?;
    let parent = win::directory(&parent_path)?;
    if protected_overlap(&parent_path, protected)? {
        return Err(invalid(
            "namespace report directory overlaps protected inputs",
        ));
    }
    if requested_parent.canonicalize()? != parent_path
        || win::identity(&win::directory(&parent_path)?)? != win::identity(&parent)?
    {
        return Err(invalid("namespace report parent changed"));
    }
    let file = win::create_directory_at(&parent, &name)?;
    let report = ReportDirectory {
        path: directory.into(),
        parent_path,
        parent,
        name,
        file,
    };
    report.verify()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("failforge-report-{}", win::random_token().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn artifacts_are_fresh_private_and_identity_bound() {
        let temp = Temp::new();
        let path = temp.0.join("report");
        let report = create_report_directory(&path, &[]).unwrap();
        let file = report.write_private("plan.json", b"whole bytes").unwrap();
        win::require_private(&file, false).unwrap();
        report.verify_file("plan.json", &file).unwrap();
        assert!(report.write_private("plan.json", b"overwrite").is_err());
        for invalid in ["../outside", "x\\outside", "name:stream", ".", ".."] {
            assert!(report.write_private(invalid, b"x").is_err());
        }
        std::fs::rename(&path, temp.0.join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(report.write_private("after", b"secret").is_err());
        assert!(!path.join("after").exists());
        assert!(!temp.0.join("moved/after").exists());
    }
    #[test]
    fn protected_input_overlap_is_rejected_before_creation() {
        let temp = Temp::new();
        assert!(
            create_report_directory(&temp.0.join("report"), std::slice::from_ref(&temp.0)).is_err()
        );
        assert!(!temp.0.join("report").exists());
    }
}
