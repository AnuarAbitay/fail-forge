//! Retained Windows report layout with regular public summary mirrors.
use crate::campaign_publish::{Publisher, token};
use crate::campaign_publish_fs as files;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
struct Written {
    file: File,
    fingerprint: files::Fingerprint,
}
pub(super) struct Layout {
    output: PathBuf,
    directories: BTreeMap<String, File>,
    written: BTreeMap<String, Written>,
    journal: Option<File>,
    sequence: u64,
    run_id: String,
    started: Instant,
}
fn parent_name(path: &str) -> io::Result<(&str, &str)> {
    if path.contains(['\\', ':', '\0'])
        || path
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
            written: BTreeMap::new(),
            journal: None,
            sequence: 0,
            run_id: run_id.into(),
            started: Instant::now(),
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
        Ok(())
    }
    pub(super) fn publisher(&self, profile: bool, families: &[String]) -> io::Result<Publisher> {
        Publisher::new(&self.output, self.root()?, profile, families)
    }
    pub(super) fn verify(&self) -> io::Result<()> {
        if files::identity(&self.directories[""])? != files::identity(&files::walk(&self.output)?)?
        {
            return Err(files::invalid("campaign report root changed"));
        }
        for (path, file) in &self.directories {
            if path.is_empty() {
                continue;
            }
            let (parent, name) = parent_name(path)?;
            files::verify_file(&self.directories[parent], name, file)?;
        }
        for (path, expected) in &self.written {
            let (parent, name) = parent_name(path)?;
            files::verify_file(&self.directories[parent], name, &expected.file)?;
            if files::fingerprint(&expected.file)? != expected.fingerprint {
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
        files::verify_file(directory, &temporary, &file)?;
        files::transfer(
            &file,
            raw.len() as u64,
            &hash,
            &fingerprint,
            &mut io::sink(),
        )?;
        absent(directory, name)?;
        files::rename_file(directory, &file, name, false)?;
        self.written.insert(
            path.into(),
            Written {
                fingerprint: files::fingerprint(&file)?,
                file,
            },
        );
        hook("sync-directory")?;
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
                let value = &summary["families"][family]["summary"];
                self.write(
                    &format!(".profile-reports/pending/families/{family}/summary.json"),
                    value,
                )?;
                self.write(&format!("families/{family}/summary.json"), value)?;
            }
            self.write(".profile-reports/pending/summary.json", summary)?;
        }
        self.write("summary.json", summary)
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| files::invalid("invalid wall clock"))?;
        let mut utc = windows_sys::Win32::Foundation::SYSTEMTIME::default();
        unsafe {
            windows_sys::Win32::System::SystemInformation::GetSystemTime(&mut utc);
        }
        let utc = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}+00:00",
            utc.wYear,
            utc.wMonth,
            utc.wDay,
            utc.wHour,
            utc.wMinute,
            utc.wSecond,
            now.subsec_micros()
        );
        let system = u64::try_from(self.started.elapsed().as_nanos())
            .map_err(|_| files::invalid("monotonic clock overflow"))?;
        value.extend(json!({"version":1,"runId":self.run_id,"sequence":sequence,"kind":kind,"systemNs":system,"utc":utc}).as_object().unwrap().clone());
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        if self.journal.is_none() {
            self.journal = Some(files::create_file(&self.directories[""], "events.jsonl")?);
        }
        let journal = self.journal.as_mut().expect("journal opened");
        journal.write_all(&bytes)?;
        journal.flush()?;
        let fingerprint = files::fingerprint(journal)?;
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
    match files::regular(parent, name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(files::invalid(
            "refusing to overwrite existing report metadata",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            std::fs::create_dir(&path).unwrap();
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
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    #[test]
    fn faults_before_commit_never_publish_metadata() {
        for step in ["create", "write", "sync-file", "commit"] {
            let mut fixture = Fixture::new();
            assert!(
                fixture
                    .layout
                    .write_with_hook(
                        "manifest.json",
                        &json!({"value":1}),
                        &mut |current| if current == step {
                            Err(io::Error::other("injected"))
                        } else {
                            Ok(())
                        }
                    )
                    .is_err()
            );
            assert!(!fixture.path.join("manifest.json").exists());
        }
    }
    #[test]
    fn metadata_and_journal_corruption_are_detected() {
        for name in ["manifest.json", "events.jsonl"] {
            let mut fixture = Fixture::new();
            if name == "manifest.json" {
                fixture.layout.write(name, &json!({"value":1})).unwrap();
            } else {
                fixture.layout.record("initial", json!({})).unwrap();
            }
            std::fs::write(fixture.path.join(name), "corruption").unwrap();
            assert!(fixture.layout.record("later", json!({})).is_err());
        }
    }
    #[test]
    fn paths_and_reserved_journal_fields_cannot_escape_layout() {
        let mut fixture = Fixture::new();
        for path in [
            "../escape",
            "C:/escape",
            "one\\escape",
            "/escape",
            "unbound/child",
            "",
        ] {
            assert!(fixture.layout.write(path, &json!({})).is_err());
        }
        for key in ["version", "runId", "sequence", "kind", "systemNs", "utc"] {
            assert!(
                fixture
                    .layout
                    .record("sample", json!({key:"forged"}))
                    .is_err()
            );
        }
        fixture
            .layout
            .record("sample", json!({"text":"один"}))
            .unwrap();
        let value: Value =
            serde_json::from_slice(&std::fs::read(fixture.path.join("events.jsonl")).unwrap())
                .unwrap();
        assert_eq!(value["sequence"], 1);
    }
}
