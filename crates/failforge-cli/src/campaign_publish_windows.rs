//! Native Windows publication. Family summaries are regular mirrors; the root
//! summary is the authoritative final commit. Precommit failures roll mirrors
//! back through retained handles; staged generations remain available to audit.
use crate::campaign_publish_fs as files;
use failforge_workspace::windows_fs as win;
use files::invalid;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
const FAMILIES: [&str; 3] = ["classic", "failure_mode", "language_specific"];
pub(crate) fn token() -> io::Result<String> {
    win::random_token()
}
fn hex(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) struct Publisher {
    output: PathBuf,
    root: File,
    profile: bool,
    families: Vec<String>,
    directories: BTreeMap<String, File>,
}
impl Publisher {
    pub(crate) fn new(
        output: &Path,
        root: File,
        profile: bool,
        families: &[String],
    ) -> io::Result<Self> {
        files::path_text(output)?;
        if families.is_empty()
            || families.len() > 3
            || !profile && families.len() != 1
            || families
                .iter()
                .any(|name| !FAMILIES.contains(&name.as_str()))
            || families.iter().collect::<BTreeSet<_>>().len() != families.len()
        {
            return Err(invalid("invalid publication families"));
        }
        win::require_owned(&root, false)?;
        let mut selected = families.to_vec();
        selected.sort_by_key(|name| FAMILIES.iter().position(|family| *family == name));
        let mut directories = BTreeMap::new();
        if profile {
            directories.insert(
                ".profile-reports".into(),
                files::directory(&root, ".profile-reports")?,
            );
            let family_root = files::directory(&root, "families")?;
            for family in &selected {
                directories.insert(
                    format!("families/{family}"),
                    files::directory(&family_root, family)?,
                );
            }
            directories.insert("families".into(), family_root);
        }
        let publisher = Self {
            output: output.into(),
            root,
            profile,
            families: selected,
            directories,
        };
        publisher.verify()?;
        Ok(publisher)
    }
    fn verify(&self) -> io::Result<()> {
        if files::identity(&files::walk(&self.output)?)? != files::identity(&self.root)? {
            return Err(invalid("publication root binding changed"));
        }
        for (path, directory) in &self.directories {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            let root = if parent.is_empty() {
                &self.root
            } else {
                &self.directories[parent]
            };
            files::verify_file(root, name, directory)?;
        }
        Ok(())
    }
    fn lock(&self) -> io::Result<File> {
        let file = match files::create_file(&self.root, ".report-publish.lock") {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                win::regular_at(&self.root, OsStr::new(".report-publish.lock"), true, false)?
            }
            Err(error) => return Err(error),
        };
        win::require_private(&file, false)?;
        if !win::try_lock(&file)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another report publisher is active",
            ));
        }
        files::verify_file(&self.root, ".report-publish.lock", &file)?;
        Ok(file)
    }
    pub(crate) fn publish(&self, summary: &Value, progress: Option<&Value>) -> io::Result<()> {
        self.publish_with_hook(summary, progress, &mut |_| Ok(()))
    }
    fn publish_with_hook(
        &self,
        summary: &Value,
        progress: Option<&Value>,
        hook: &mut impl FnMut(&str) -> io::Result<()>,
    ) -> io::Result<()> {
        let complete = summary["complete"]
            .as_bool()
            .ok_or_else(|| invalid("missing report completion"))?;
        let run_id = summary["runId"]
            .as_str()
            .filter(|id| hex(id))
            .ok_or_else(|| invalid("missing report identity"))?;
        if summary["output"] != self.output.to_string_lossy().as_ref()
            || self.profile && summary["kind"] != "profile"
            || !self.profile && summary["kind"] == "profile" && complete
        {
            return Err(invalid("publication summary identity changed"));
        }
        match progress {
            Some(value) => {
                crate::campaign_progress::validate_final(value, run_id, &self.output, complete)?
            }
            None if complete => {
                return Err(invalid("complete publication requires final progress"));
            }
            None => (),
        }
        self.verify()?;
        let _guard = self.lock()?;
        let mut values = vec![(String::new(), summary)];
        if self.profile {
            for family in &self.families {
                let value = &summary["families"][family]["summary"];
                if !value.is_object() || value["family"] != *family || value["runId"] != run_id {
                    return Err(invalid("missing selected family report or identity"));
                }
                values.push((format!("families/{family}"), value));
            }
        }
        if let Some(progress) = progress {
            hook("progress:prepare")?;
            self.verify()?;
            let mut writer =
                crate::campaign_progress::Writer::new(&self.output, self.root.try_clone()?)?;
            writer.write_with_hook(progress, &mut |step| hook(&format!("progress:{step}")))?;
            self.verify()?;
        }
        let publication_token = token()?;
        let mut staged = vec![];
        for (parent, value) in values {
            let directory = if parent.is_empty() {
                &self.root
            } else {
                &self.directories[&parent]
            };
            let mut bytes = serde_json::to_vec_pretty(value)?;
            bytes.push(b'\n');
            let name = format!(".summary-{publication_token}.tmp");
            hook(&format!("create:{parent}/summary.json"))?;
            self.verify()?;
            let mut file = files::create_file(directory, &name)?;
            hook(&format!("write:{parent}/summary.json"))?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            let fingerprint = files::staged(&file, bytes.len() as u64)?;
            files::transfer(
                &file,
                bytes.len() as u64,
                &hash,
                &fingerprint,
                &mut io::sink(),
            )?;
            let old = match files::regular(directory, "summary.json") {
                Ok(old) => {
                    win::require_private(&old, false)?;
                    let old_fingerprint = files::fingerprint(&old)?;
                    let old_bytes = files::read(&old, 512 * 1024 * 1024)?;
                    if files::fingerprint(&old)? != old_fingerprint {
                        return Err(invalid(
                            "publication target changed while retaining its rollback bytes",
                        ));
                    }
                    let backup_name = format!(".summary-backup-{publication_token}.tmp");
                    let mut backup = files::create_file(directory, &backup_name)?;
                    backup.write_all(&old_bytes)?;
                    backup.flush()?;
                    backup.sync_all()?;
                    let backup_hash = format!("{:x}", Sha256::digest(&old_bytes));
                    let backup_fingerprint = files::staged(&backup, old_bytes.len() as u64)?;
                    files::transfer(
                        &backup,
                        old_bytes.len() as u64,
                        &backup_hash,
                        &backup_fingerprint,
                        &mut io::sink(),
                    )?;
                    Some(Backup {
                        original: old,
                        original_fingerprint: old_fingerprint,
                        file: backup,
                        name: backup_name,
                        bytes: old_bytes.len() as u64,
                        hash: backup_hash,
                        fingerprint: backup_fingerprint,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
            staged.push(Stage {
                parent,
                name,
                file,
                bytes: bytes.len() as u64,
                hash,
                fingerprint,
                old,
            });
        }
        if self.profile {
            // Preserve a complete, private generation, independent of the flat
            // public mirrors. Its directory name is tied to this publication.
            let storage = &self.directories[".profile-reports"];
            let generation = files::create_directory(
                storage,
                &format!(
                    "{}-{publication_token}",
                    if complete { "final" } else { "partial" }
                ),
            )?;
            let families = files::create_directory(&generation, "families")?;
            for stage in &staged {
                let parent = if stage.parent.is_empty() {
                    generation.try_clone()?
                } else {
                    files::create_directory(
                        &families,
                        stage.parent.split('/').nth(1).expect("validated family"),
                    )?
                };
                let mut output = files::create_file(&parent, "summary.json")?;
                files::transfer(
                    &stage.file,
                    stage.bytes,
                    &stage.hash,
                    &stage.fingerprint,
                    &mut output,
                )?;
                output.flush()?;
                output.sync_all()?;
            }
        }
        let verify = |stage: &Stage, committed: bool| -> io::Result<()> {
            self.verify()?;
            let parent = if stage.parent.is_empty() {
                &self.root
            } else {
                &self.directories[&stage.parent]
            };
            files::verify_file(
                parent,
                if committed {
                    "summary.json"
                } else {
                    &stage.name
                },
                &stage.file,
            )?;
            let current = files::staged(&stage.file, stage.bytes)?;
            if !committed && current != stage.fingerprint {
                return Err(invalid("staged publication bytes changed"));
            }
            files::transfer(
                &stage.file,
                stage.bytes,
                &stage.hash,
                &current,
                &mut io::sink(),
            )?;
            if !committed {
                match &stage.old {
                    Some(old) => {
                        files::verify_file(parent, "summary.json", &old.original)?;
                        if files::fingerprint(&old.original)? != old.original_fingerprint {
                            return Err(invalid("publication target changed"));
                        }
                        files::transfer(
                            &old.original,
                            old.bytes,
                            &old.hash,
                            &old.original_fingerprint,
                            &mut io::sink(),
                        )?;
                    }
                    None => match files::regular(parent, "summary.json") {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        Err(error) => return Err(error),
                        Ok(_) => return Err(invalid("publication target appeared")),
                    },
                }
            }
            Ok(())
        };
        for stage in &staged {
            verify(stage, false)?;
        }
        let mut committed = vec![];
        let prepare = (|| {
            for (index, stage) in staged.iter().enumerate().skip(1) {
                hook(&format!("commit:{}", stage.parent))?;
                verify(stage, false)?;
                let parent = &self.directories[&stage.parent];
                files::rename_file(parent, &stage.file, "summary.json", true)?;
                committed.push(index);
            }
            hook("commit")?;
            self.verify()?;
            for index in &committed {
                verify(&staged[*index], true)?;
            }
            verify(&staged[0], false)?;
            Ok(())
        })();
        let prepare = prepare
            .and_then(|()| files::rename_file(&self.root, &staged[0].file, "summary.json", true));
        if let Err(error) = prepare {
            for index in committed.into_iter().rev() {
                let stage = &staged[index];
                let parent = &self.directories[&stage.parent];
                files::verify_file(parent, "summary.json", &stage.file)?;
                match &stage.old {
                    Some(old) => {
                        files::verify_file(parent, &old.name, &old.file)?;
                        files::transfer(
                            &old.file,
                            old.bytes,
                            &old.hash,
                            &old.fingerprint,
                            &mut io::sink(),
                        )?;
                        files::rename_file(parent, &old.file, "summary.json", true)?;
                    }
                    None => win::remove_handle(&stage.file)?,
                }
            }
            return Err(error);
        }
        // Successful root rename is the final fallible operation. No callbacks,
        // verification or cleanup may turn a committed report into an error.
        Ok(())
    }
}
struct Backup {
    original: File,
    original_fingerprint: files::Fingerprint,
    file: File,
    name: String,
    bytes: u64,
    hash: String,
    fingerprint: files::Fingerprint,
}
struct Stage {
    parent: String,
    name: String,
    file: File,
    bytes: u64,
    hash: String,
    fingerprint: files::Fingerprint,
    old: Option<Backup>,
}

#[cfg(test)]
#[path = "campaign_publish_windows_tests.rs"]
mod tests;
