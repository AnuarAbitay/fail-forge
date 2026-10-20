//! Descriptor-relative metadata, initial report layout and ordered journal.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::campaign_publish::{Publisher, token};
use crate::campaign_publish_fs as files;

struct Alias {
    device: u64,
    inode: u64,
    target: String,
}

struct Written {
    file: File,
    fingerprint: files::Fingerprint,
}

/// Every traversed directory remains open for the lifetime of the campaign.
pub(super) struct Layout {
    output: PathBuf,
    directories: BTreeMap<String, File>,
    aliases: BTreeMap<String, Alias>,
    written: BTreeMap<String, Written>,
    journal: Option<File>,
    sequence: u64,
    run_id: String,
}

fn parent_name(path: &str) -> io::Result<(&str, &str)> {
    if path
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(files::invalid("invalid relative report filename"));
    }
    Ok(path.rsplit_once('/').unwrap_or(("", path)))
}

impl Layout {
    pub(super) fn new(
        output: &Path,
        root: File,
        run_id: &str,
        families: &[String],
        profile: bool,
    ) -> io::Result<Self> {
        files::path_text(output)?;
        let mut layout = Self {
            output: output.into(),
            directories: BTreeMap::from([(String::new(), root)]),
            aliases: BTreeMap::new(),
            written: BTreeMap::new(),
            journal: None,
            sequence: 0,
            run_id: run_id.into(),
        };
        layout.verify()?;
        if profile {
            layout.bind_directory("families", false)?;
            for family in families {
                layout.bind_directory(&format!("families/{family}"), false)?;
            }
        }
        Ok(layout)
    }

    pub(super) fn root(&self) -> io::Result<File> {
        self.verify()?;
        self.directories[""].try_clone()
    }

    fn bind_directory(&mut self, path: &str, create: bool) -> io::Result<()> {
        self.verify()?;
        let (parent, name) = parent_name(path)?;
        let parent = self
            .directories
            .get(parent)
            .ok_or_else(|| files::invalid("unbound report parent"))?;
        let directory = if create {
            files::create_directory(parent, name)?
        } else {
            files::directory(parent, name)?
        };
        self.directories.insert(path.into(), directory);
        self.verify()
    }

    fn alias(&mut self, path: &str, target: &str) -> io::Result<()> {
        self.verify()?;
        let (parent, name) = parent_name(path)?;
        let parent = self
            .directories
            .get(parent)
            .ok_or_else(|| files::invalid("unbound report alias parent"))?;
        files::symlink(parent, name, target)?;
        let info = files::stat(parent, name)?;
        if info.st_mode & libc::S_IFMT != libc::S_IFLNK || files::read_link(parent, name)? != target
        {
            return Err(files::invalid("new report alias changed"));
        }
        self.aliases.insert(
            path.into(),
            Alias {
                device: info.st_dev as u64,
                inode: info.st_ino,
                target: target.into(),
            },
        );
        self.verify()
    }

    pub(super) fn profile(&mut self, families: &[String]) -> io::Result<()> {
        for path in [
            ".profile-reports",
            ".profile-reports/pending",
            ".profile-reports/pending/families",
        ] {
            self.bind_directory(path, true)?;
        }
        for family in families {
            self.bind_directory(&format!(".profile-reports/pending/families/{family}"), true)?;
        }
        self.alias(".profile-reports-current", ".profile-reports/pending")?;
        self.alias("summary.json", ".profile-reports-current/summary.json")?;
        for family in families {
            self.alias(
                &format!("families/{family}/summary.json"),
                &format!("../../.profile-reports-current/families/{family}/summary.json"),
            )?;
        }
        Ok(())
    }

    pub(super) fn publisher(&self, profile: bool, families: &[String]) -> io::Result<Publisher> {
        Publisher::new(&self.output, self.root()?, profile, families)
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        let held = self.directories[""].metadata()?;
        let named = files::walk(&self.output)?.metadata()?;
        if held.dev() != named.dev() || held.ino() != named.ino() {
            return Err(files::invalid("campaign report root changed"));
        }
        for (path, file) in &self.directories {
            if path.is_empty() {
                continue;
            }
            let (parent, name) = parent_name(path)?;
            if !files::same(
                &files::stat(&self.directories[parent], name)?,
                file,
                libc::S_IFDIR,
            )? {
                return Err(files::invalid("campaign report directory changed"));
            }
        }
        for (path, alias) in &self.aliases {
            let (parent, name) = parent_name(path)?;
            let directory = &self.directories[parent];
            let named = files::stat(directory, name)?;
            if named.st_mode & libc::S_IFMT != libc::S_IFLNK
                || named.st_dev as u64 != alias.device
                || named.st_ino != alias.inode
                || files::read_link(directory, name)? != alias.target
            {
                return Err(files::invalid("campaign report alias changed"));
            }
        }
        for (path, expected) in &self.written {
            let (parent, name) = parent_name(path)?;
            let named = files::stat(&self.directories[parent], name)?;
            if !files::same(&named, &expected.file, libc::S_IFREG)? {
                return Err(files::invalid("campaign report file changed"));
            }
            if files::Fingerprint::from(expected.file.metadata()?) != expected.fingerprint {
                return Err(files::invalid("campaign report contents changed"));
            }
        }
        Ok(())
    }

    pub(super) fn write(&mut self, path: &str, value: &Value) -> io::Result<()> {
        self.write_with_hook(path, value, &mut |_| Ok(()))
    }

    fn write_with_hook(
        &mut self,
        path: &str,
        value: &Value,
        hook: &mut impl FnMut(&str) -> io::Result<()>,
    ) -> io::Result<()> {
        self.verify()?;
        let (parent, name) = parent_name(path)?;
        let directory = self
            .directories
            .get(parent)
            .ok_or_else(|| files::invalid("unbound report file parent"))?;
        absent(directory, name)?;
        let mut raw = serde_json::to_vec_pretty(value)?;
        raw.push(b'\n');
        let hash = format!("{:x}", Sha256::digest(&raw));
        let temporary = format!(".metadata-{}.tmp", token()?);
        hook("create")?;
        self.verify()?;
        let mut file = files::create_file(directory, &temporary)?;
        hook("write")?;
        file.write_all(&raw)?;
        hook("sync-file")?;
        file.sync_all()?;
        let fingerprint = files::staged(&file, raw.len() as u64)?;
        files::transfer(
            &file,
            raw.len() as u64,
            &hash,
            &fingerprint,
            &mut io::sink(),
        )?;
        hook("commit")?;
        self.verify()?;
        if !files::same(&files::stat(directory, &temporary)?, &file, libc::S_IFREG)? {
            return Err(files::invalid("staged report metadata changed"));
        }
        files::transfer(
            &file,
            raw.len() as u64,
            &hash,
            &fingerprint,
            &mut io::sink(),
        )?;
        absent(directory, name)?;
        files::rename(directory, &temporary, name)?;
        self.written.insert(
            path.into(),
            Written {
                fingerprint: file.metadata()?.into(),
                file,
            },
        );
        hook("sync-directory")?;
        directory.sync_all()?;
        self.verify()
    }

    pub(super) fn initial(
        &mut self,
        summary: &Value,
        profile: bool,
        families: &[String],
    ) -> io::Result<()> {
        if profile {
            for family in families {
                self.write(
                    &format!(".profile-reports/pending/families/{family}/summary.json"),
                    &summary["families"][family]["summary"],
                )?;
            }
            self.write(".profile-reports/pending/summary.json", summary)
        } else {
            self.write("summary.json", summary)
        }
    }

    pub(super) fn record(&mut self, kind: &str, fields: Value) -> io::Result<()> {
        self.verify()?;
        let mut value = fields
            .as_object()
            .cloned()
            .ok_or_else(|| files::invalid("journal fields must be an object"))?;
        if ["version", "runId", "sequence", "kind", "systemNs", "utc"]
            .iter()
            .any(|key| value.contains_key(*key))
        {
            return Err(files::invalid("journal metadata must not be overridden"));
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| files::invalid("journal sequence overflow"))?;
        let (system, utc) = clock()?;
        value.extend(
            json!({"version":1,"runId":self.run_id,"sequence":sequence,
            "kind":kind,"systemNs":system,"utc":utc})
            .as_object()
            .unwrap()
            .clone(),
        );
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        if self.journal.is_none() {
            self.journal = Some(files::create_file(&self.directories[""], "events.jsonl")?);
        }
        let journal = self.journal.as_mut().expect("journal opened");
        journal.write_all(&bytes)?;
        journal.flush()?;
        let fingerprint = journal.metadata()?.into();
        if let Some(written) = self.written.get_mut("events.jsonl") {
            written.fingerprint = fingerprint;
        } else {
            self.written.insert(
                "events.jsonl".into(),
                Written {
                    file: journal.try_clone()?,
                    fingerprint,
                },
            );
        }
        self.sequence = sequence;
        self.verify()
    }
}

fn absent(parent: &File, name: &str) -> io::Result<()> {
    match files::stat(parent, name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(files::invalid(
            "refusing to overwrite existing report metadata",
        )),
    }
}

fn clock() -> io::Result<(u64, String)> {
    let mut stamp = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime initializes the provided timespec on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, stamp.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful clock_gettime initialized stamp.
    let stamp = unsafe { stamp.assume_init() };
    let system = u64::try_from(stamp.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|value| {
            u64::try_from(stamp.tv_nsec)
                .ok()
                .and_then(|nanos| value.checked_add(nanos))
        })
        .ok_or_else(|| files::invalid("invalid monotonic clock"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| files::invalid("invalid wall clock"))?;
    let seconds =
        libc::time_t::try_from(now.as_secs()).map_err(|_| files::invalid("wall clock overflow"))?;
    let mut utc = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: gmtime_r writes only to the supplied tm and does not share static state.
    if unsafe { libc::gmtime_r(&seconds, utc.as_mut_ptr()) }.is_null() {
        return Err(files::invalid("cannot format wall clock"));
    }
    // SAFETY: successful gmtime_r initialized utc.
    let utc = unsafe { utc.assume_init() };
    Ok((
        system,
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00",
            utc.tm_year + 1900,
            utc.tm_mon + 1,
            utc.tm_mday,
            utc.tm_hour,
            utc.tm_min,
            utc.tm_sec,
            now.subsec_micros()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{FileExt, symlink};

    struct Fixture {
        path: PathBuf,
        layout: Layout,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("ff-layout-{}", token().unwrap()));
            fs::create_dir(&path).unwrap();
            let layout = Layout::new(
                &path,
                files::walk(&path).unwrap(),
                "0123456789abcdef0123456789abcdef",
                &["classic".into()],
                false,
            )
            .unwrap();
            Self { path, layout }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn metadata_faults_never_publish_before_commit() {
        for step in ["create", "write", "sync-file", "commit"] {
            let mut fixture = Fixture::new();
            let error = fixture
                .layout
                .write_with_hook("manifest.json", &json!({"x":1}), &mut |current| {
                    if current == step {
                        Err(io::Error::other("injected"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert_eq!(error.to_string(), "injected");
            assert!(!fixture.path.join("manifest.json").exists());
        }
    }
    #[test]
    fn same_length_stage_content_substitution_is_rejected() {
        let mut fixture = Fixture::new();
        let path = fixture.path.clone();
        let error = fixture
            .layout
            .write_with_hook("manifest.json", &json!({"value":"original"}), &mut |step| {
                if step == "sync-file" {
                    let staged = fs::read_dir(&path)?
                        .map(|entry| entry.unwrap().path())
                        .find(|entry| {
                            entry
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .starts_with(".metadata-")
                        })
                        .unwrap();
                    let file = fs::OpenOptions::new().write(true).open(staged)?;
                    file.write_all_at(b"X", 0)?;
                }
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("hash"));
        assert!(!fixture.path.join("manifest.json").exists());
    }
    #[test]
    fn substituted_stage_name_is_not_committed_or_removed() {
        let mut fixture = Fixture::new();
        let path = fixture.path.clone();
        assert!(
            fixture
                .layout
                .write_with_hook("manifest.json", &json!({}), &mut |step| {
                    if step == "commit" {
                        let staged = fs::read_dir(&path)?
                            .map(|entry| entry.unwrap().path())
                            .find(|entry| {
                                entry
                                    .file_name()
                                    .unwrap()
                                    .to_string_lossy()
                                    .starts_with(".metadata-")
                            })
                            .unwrap();
                        fs::rename(&staged, path.join("held-stage"))?;
                        symlink("foreign", &staged)?;
                    }
                    Ok(())
                })
                .is_err()
        );
        assert!(!fixture.path.join("manifest.json").exists());
        assert!(
            fs::read_dir(&path).unwrap().any(|entry| entry
                .unwrap()
                .file_type()
                .unwrap()
                .is_symlink())
        );
    }
    #[test]
    fn late_foreign_target_is_preserved() {
        let mut fixture = Fixture::new();
        let path = fixture.path.clone();
        assert!(
            fixture
                .layout
                .write_with_hook("manifest.json", &json!({}), &mut |step| {
                    if step == "commit" {
                        fs::write(path.join("manifest.json"), "foreign")?;
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(path.join("manifest.json")).unwrap(),
            "foreign"
        );
    }
    #[test]
    fn directory_sync_failure_preserves_valid_metadata_and_latches_error() {
        let mut fixture = Fixture::new();
        assert!(
            fixture
                .layout
                .write_with_hook("manifest.json", &json!({"value":1}), &mut |step| {
                    if step == "sync-directory" {
                        Err(io::Error::other("injected"))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(fixture.path.join("manifest.json")).unwrap())
                .unwrap(),
            json!({"value":1})
        );
        fixture.layout.verify().unwrap();
    }
    #[test]
    fn existing_metadata_contents_and_journal_are_guarded() {
        for name in ["manifest.json", "events.jsonl"] {
            let mut fixture = Fixture::new();
            if name == "manifest.json" {
                fixture.layout.write(name, &json!({"x":1})).unwrap();
            } else {
                fixture
                    .layout
                    .record("stage_started", json!({"stage":"snapshot"}))
                    .unwrap();
            }
            fs::write(fixture.path.join(name), "corruption").unwrap();
            assert!(fixture.layout.record("later", json!({})).is_err());
            assert_eq!(
                fs::read_to_string(fixture.path.join(name)).unwrap(),
                "corruption"
            );
        }
    }
    #[test]
    fn journal_envelope_cannot_be_replaced_by_fields() {
        let mut fixture = Fixture::new();
        for key in ["version", "runId", "sequence", "kind", "systemNs", "utc"] {
            assert!(
                fixture
                    .layout
                    .record("sample", json!({key:"forged"}))
                    .is_err()
            );
        }
        assert!(!fixture.path.join("events.jsonl").exists());
        fixture
            .layout
            .record("sample", json!({"text":"один"}))
            .unwrap();
        let value: Value =
            serde_json::from_slice(&fs::read(fixture.path.join("events.jsonl")).unwrap()).unwrap();
        assert_eq!(value["sequence"], 1);
    }
    #[test]
    fn metadata_paths_cannot_escape_bound_layout() {
        let mut fixture = Fixture::new();
        for path in [
            "/outside",
            "../outside",
            "one/../outside",
            "unbound/child",
            "",
            "one//two",
        ] {
            assert!(fixture.layout.write(path, &json!({})).is_err());
        }
        assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 0);
    }
}
