//! Atomic progress checkpoints anchored to a retained Windows directory handle.
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use failforge_workspace::windows_fs as win;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::campaign_publish_fs as files;

const MAX_PROGRESS_BYTES: usize = 64 * 1024 * 1024;

struct Existing {
    file: File,
    fingerprint: files::Fingerprint,
    hash: String,
}

fn digest(file: &File) -> io::Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(files::read(file, MAX_PROGRESS_BYTES)?)
    ))
}

fn existing(root: &File) -> io::Result<Option<Existing>> {
    let file = match files::regular(root, "progress.json") {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let before = files::staged(&file, file.metadata()?.len())?;
    files::verify_file(root, "progress.json", &file)?;
    let hash = digest(&file)?;
    if files::fingerprint(&file)? != before {
        return Err(files::invalid("progress file changed"));
    }
    Ok(Some(Existing {
        file,
        fingerprint: before,
        hash,
    }))
}

pub(crate) struct Writer {
    path: PathBuf,
    root: File,
    current: Option<Existing>,
}
impl Writer {
    pub(crate) fn new(path: &Path, root: File) -> io::Result<Self> {
        files::path_text(path)?;
        win::require_private(&root, true)?;
        let mut result = Self {
            path: path.into(),
            root,
            current: None,
        };
        result.verify_root()?;
        result.current = existing(&result.root)?;
        Ok(result)
    }

    fn verify_root(&self) -> io::Result<()> {
        let named = files::walk(&self.path)?;
        if files::identity(&named)? != files::identity(&self.root)? {
            return Err(files::invalid("progress output directory changed"));
        }
        Ok(())
    }

    fn verify_current(&self) -> io::Result<()> {
        self.verify_root()?;
        if let Some(current) = &self.current {
            files::verify_file(&self.root, "progress.json", &current.file)?;
            if files::fingerprint(&current.file)? != current.fingerprint
                || digest(&current.file)? != current.hash
            {
                return Err(files::invalid("progress checkpoint changed"));
            }
        } else {
            match files::regular(&self.root, "progress.json") {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(files::invalid("unexpected progress checkpoint")),
            }
        }
        Ok(())
    }

    pub(crate) fn write(&mut self, value: &Value) -> io::Result<()> {
        self.write_with_hook(value, &mut |_| Ok(()))
    }

    pub(crate) fn write_with_hook(
        &mut self,
        value: &Value,
        hook: &mut impl FnMut(&str) -> io::Result<()>,
    ) -> io::Result<()> {
        self.verify_current()?;
        let mut encoded = serde_json::to_vec(value).map_err(io::Error::other)?;
        encoded.push(b'\n');
        if encoded.len() > MAX_PROGRESS_BYTES {
            return Err(files::invalid("progress checkpoint exceeds its byte limit"));
        }
        let bytes = encoded.len() as u64;
        let hash = format!("{:x}", Sha256::digest(&encoded));
        let name = format!(".progress-{}.tmp", win::random_token()?);
        hook("create-stage")?;
        let mut file = files::create_file(&self.root, &name)?;
        // A failed or uncertain stage is retained; names never authorize cleanup.
        hook("write-stage")?;
        file.write_all(&encoded)?;
        hook("sync-file")?;
        file.sync_all()?;
        let before = files::staged(&file, bytes)?;
        if digest(&file)? != hash {
            return Err(files::invalid("staged progress bytes changed"));
        }
        hook("verify-stage")?;
        self.verify_current()?;
        files::verify_file(&self.root, &name, &file)?;
        if files::staged(&file, bytes)? != before {
            return Err(files::invalid("staged progress changed"));
        }
        hook("sync-root")?;
        files::sync_directory(&self.root)?;
        hook("commit")?;
        self.verify_current()?;
        files::verify_file(&self.root, &name, &file)?;
        if files::staged(&file, bytes)? != before || digest(&file)? != hash {
            return Err(files::invalid("staged progress changed"));
        }
        files::rename_file(&self.root, &file, "progress.json", self.current.is_some())?;
        self.current = Some(Existing {
            fingerprint: files::staged(&file, bytes)?,
            file,
            hash,
        });
        hook("sync-commit")?;
        files::sync_directory(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsStr;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("ff-progress-{}", win::random_token().unwrap()));
            let parent = win::directory(path.parent().unwrap()).unwrap();
            drop(win::create_directory_at(&parent, path.file_name().unwrap()).unwrap());
            Self(path)
        }
        fn writer(&self) -> Writer {
            Writer::new(&self.0, win::directory(&self.0).unwrap()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fresh_and_existing_checkpoints_preserve_complete_json() {
        let fixture = Fixture::new();
        let mut first = fixture.writer();
        first
            .write(&json!({"complete":false,"completedMutants":1}))
            .unwrap();
        drop(first);
        let mut resumed = fixture.writer();
        resumed
            .write(&json!({"complete":false,"completedMutants":2}))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(
                &std::fs::read(fixture.0.join("progress.json")).unwrap()
            )
            .unwrap()["completedMutants"],
            2
        );
    }

    #[test]
    fn in_place_tampering_is_rejected_before_replacement() {
        let fixture = Fixture::new();
        let mut writer = fixture.writer();
        writer.write(&json!({"n":1})).unwrap();
        std::fs::write(fixture.0.join("progress.json"), b"{\"n\":9}\n").unwrap();
        assert!(writer.write(&json!({"n":2})).is_err());
        assert_eq!(
            std::fs::read(fixture.0.join("progress.json")).unwrap(),
            b"{\"n\":9}\n"
        );
    }

    #[test]
    fn stage_byte_changes_cannot_become_a_valid_checkpoint() {
        let fixture = Fixture::new();
        let mut writer = fixture.writer();
        let result = writer.write_with_hook(&json!({"n":1}), &mut |event| {
            if event == "verify-stage" {
                let root = win::directory(&fixture.0)?;
                let name = win::names(&root)?
                    .into_iter()
                    .find(|name| name.to_string_lossy().starts_with(".progress-"))
                    .unwrap();
                let mut file = win::regular_at(&root, OsStr::new(&name), true, false)?;
                file.write_all(b"{\"n\":9}\n")?;
                file.sync_all()?;
            }
            Ok(())
        });
        assert!(result.is_err());
        assert!(!fixture.0.join("progress.json").exists());
    }
}
