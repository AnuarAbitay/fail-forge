//! Read-only admission of archived discovery and baseline evidence.
//! Execution, isolation, confirmation and publication stay in the campaign owner.
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use failforge_dart::namespace::NamespaceError;
use failforge_dart::{ReporterMode, analyze_catalogue, classify_dart_capture, sha256_hex};
use failforge_workspace::{Cancellation, Hashes, PreparedProject, parse_json, source_hashes};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::dart_namespace::invalid;
use crate::family_execution::DirectoryGuard;

const MAX_DOCUMENT: usize = 64 * 1024 * 1024;
const MAX_CAPTURE: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReplayPaths {
    pub transcript: PathBuf,
    pub baseline: PathBuf,
    pub framework: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl Fingerprint {
    fn of(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Debug)]
struct BoundFile {
    path: PathBuf,
    file: File,
    fingerprint: Fingerprint,
    hash: String,
}
impl BoundFile {
    fn read(path: &Path, limit: usize) -> Result<(Self, Vec<u8>), NamespaceError> {
        if !path.is_absolute() {
            return Err(invalid("replay evidence paths must be absolute"));
        }
        // Resolve ancestors while retaining a no-follow check on the final leaf.
        let parent = path
            .parent()
            .ok_or_else(|| invalid("invalid replay file"))?
            .canonicalize()?;
        let path = parent.join(
            path.file_name()
                .ok_or_else(|| invalid("invalid replay file"))?,
        );
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > limit as u64 {
            return Err(invalid("replay evidence must be a bounded regular file"));
        }
        let fingerprint = Fingerprint::of(&metadata);
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > limit || Fingerprint::of(&file.metadata()?) != fingerprint {
            return Err(invalid("replay evidence changed while reading"));
        }
        let bound = Self {
            path,
            file,
            fingerprint,
            hash: sha256_hex(&bytes),
        };
        bound.verify()?;
        Ok((bound, bytes))
    }

    fn verify_identity(&self) -> Result<(), NamespaceError> {
        let named = fs::symlink_metadata(&self.path)?;
        if !named.is_file()
            || Fingerprint::of(&named) != self.fingerprint
            || Fingerprint::of(&self.file.metadata()?) != self.fingerprint
        {
            return Err(invalid("retained replay file identity changed"));
        }
        Ok(())
    }

    fn verify(&self) -> Result<(), NamespaceError> {
        self.verify_identity()?;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut offset = 0;
        loop {
            let count = self.file.read_at(&mut buffer, offset)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
            offset += count as u64;
            if offset > self.fingerprint.len {
                return Err(invalid("retained replay file grew"));
            }
        }
        if offset != self.fingerprint.len || format!("{:x}", digest.finalize()) != self.hash {
            return Err(invalid("retained replay file contents changed"));
        }
        self.verify_identity()?;
        Ok(())
    }
}

pub(crate) struct LoadedReplay {
    pub discovery: Value,
    pub expected: Vec<[String; 2]>,
    project: PathBuf,
    project_guard: DirectoryGuard,
    project_hashes: Hashes,
    framework: PathBuf,
    framework_guard: DirectoryGuard,
    framework_hashes: Hashes,
    files: Vec<BoundFile>,
    binding: Value,
}
impl std::fmt::Debug for LoadedReplay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedReplay")
            .field("project", &self.project)
            .field("framework", &self.framework)
            .field("expected", &self.expected.len())
            .field("files", &self.files.len())
            .finish()
    }
}

fn relative(value: &str) -> Result<&Path, NamespaceError> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains(['\0', '\\'])
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || path.to_str() != Some(value)
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(invalid("replay evidence needs canonical relative paths"));
    }
    Ok(path)
}

fn library(hashes: &Hashes) -> Hashes {
    hashes
        .iter()
        .filter(|(path, _)| path.starts_with("lib"))
        .map(|(path, hash)| (path.clone(), hash.clone()))
        .collect()
}

fn read_json(path: &Path, files: &mut Vec<BoundFile>) -> Result<Value, NamespaceError> {
    let (file, bytes) = BoundFile::read(path, MAX_DOCUMENT)?;
    let value = parse_json(&bytes)?;
    if !value.is_object() {
        return Err(invalid("replay evidence JSON must be an object"));
    }
    files.push(file);
    Ok(value)
}

impl LoadedReplay {
    pub(crate) fn load(
        paths: &ReplayPaths,
        project: &PreparedProject,
        cancel: &Cancellation,
    ) -> Result<Self, NamespaceError> {
        cancel.check()?;
        let project_guard = DirectoryGuard::open(&project.root)?;
        let framework_guard = DirectoryGuard::open(&paths.framework)?;
        let framework = framework_guard.canonical_path()?;
        let project_hashes = source_hashes(&project.root, cancel)?;
        let framework_hashes = source_hashes(&framework, cancel)?;
        let project_lib = library(&project_hashes);
        if project_lib.is_empty() || project_lib != library(&framework_hashes) {
            return Err(invalid(
                "replay framework is not the exact pristine subject library",
            ));
        }
        let mut files = Vec::new();
        let baseline = read_json(&paths.baseline, &mut files)?;
        let baseline_path = files[0].path.clone();
        let directory = baseline_path
            .parent()
            .ok_or_else(|| invalid("missing baseline directory"))?;
        let provenance = read_json(&directory.join("provenance.json"), &mut files)?;
        let dependencies = read_json(&directory.join("dependency-result.json"), &mut files)?;
        let source = provenance["sourceFileHashes"]
            .as_object()
            .filter(|source| !source.is_empty())
            .ok_or_else(|| invalid("baseline has no source provenance"))?;
        for (name, hash) in source {
            let path = relative(name)?;
            if !hash.as_str().is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }) || project_hashes.get(path).map(String::as_str) != hash.as_str()
            {
                return Err(invalid(
                    "project differs from historical baseline provenance",
                ));
            }
        }
        // The archive may omit generated metadata, but must cover every author
        // library/test/configuration source whose execution could change.
        for name in project_hashes.keys().filter(|name| {
            name.starts_with("lib")
                || name.starts_with("test")
                || matches!(
                    name.to_str(),
                    Some("pubspec.yaml" | "dart_test.yaml" | "analysis_options.yaml")
                )
        }) {
            if !source.contains_key(
                name.to_str()
                    .ok_or_else(|| invalid("non-Unicode replay source"))?,
            ) {
                return Err(invalid("baseline provenance omits an author source"));
            }
        }
        for (name, field) in [
            ("pubspec.lock", "generatedLockSha256"),
            (
                ".dart_tool/package_config.json",
                "generatedPackageConfigSha256",
            ),
        ] {
            if project_hashes.get(Path::new(name)).map(String::as_str)
                != dependencies[field].as_str()
                || dependencies[field].as_str().is_none()
            {
                return Err(invalid("baseline dependency bindings changed"));
            }
        }
        let raw_path = directory.join(relative(
            baseline["outputSource"]
                .as_str()
                .ok_or_else(|| invalid("baseline has no reporter capture"))?,
        )?);
        let (raw_file, raw) = BoundFile::read(&raw_path, MAX_CAPTURE)?;
        files.push(raw_file);
        let cli_exit = baseline["runnerExitCode"]
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| invalid("baseline has invalid runner exit code"))?;
        let receipt = baseline["runnerSummary"]
            .as_str()
            .ok_or_else(|| invalid("baseline has no native runner receipt"))?;
        let mode = if project.flutter {
            ReporterMode::Flutter
        } else {
            ReporterMode::Dart
        };
        let evidence = classify_dart_capture(&raw, cli_exit, receipt.as_bytes(), mode);
        if evidence["outcome"] != "SURVIVED"
            || evidence["tests"]["passed"]
                .as_u64()
                .is_none_or(|count| count == 0)
        {
            return Err(invalid(
                "replay requires a complete passing historical baseline",
            ));
        }
        let expected: Vec<[String; 2]> =
            serde_json::from_value(baseline["completedTestIdentities"].clone())
                .map_err(|_| invalid("baseline has invalid author identities"))?;
        let cwd = match baseline.get("workingDirectory") {
            None => project.root.clone(),
            Some(Value::String(path)) if Path::new(path).is_absolute() && !path.contains('\0') => {
                PathBuf::from(path)
            }
            _ => return Err(invalid("baseline workingDirectory must be absolute")),
        };
        let catalogue = analyze_catalogue(&raw, &cwd, mode)
            .map_err(|_| invalid("baseline reporter catalogue is invalid"))?;
        if expected.is_empty() || expected != catalogue.identities {
            return Err(invalid(
                "baseline author identities differ from authenticated reporter",
            ));
        }
        let (transcript_file, transcript_raw) = BoundFile::read(&paths.transcript, MAX_DOCUMENT)?;
        let transcript = failforge_dart::discovery::load_retained_transcript(
            &transcript_raw,
            &project.root,
            "classic",
        )
        .map_err(|error| invalid(&error.to_string()))?;
        let transcript_hash = transcript_file.hash.clone();
        files.push(transcript_file);
        let binding = json!({
            "version":1,"staticValidationReused":true,"transcript":paths.transcript,
            "transcriptSha256":transcript_hash,"baseline":baseline_path,
            "baselineEvidence":evidence,"expectedTestIdentities":expected,
            "framework":framework,"originalFileHashes":project_hashes,
            "frameworkFileHashes":framework_hashes,"candidates":transcript.candidates.len(),
            "operators":transcript.steps["handshake"]["operators"],
            "operatorTree":transcript.steps["handshake"]["operatorTree"],
            "retainedCandidates":transcript.candidates.iter().map(|candidate|
                json!({"mutantId":candidate["mutant"]["id"],"operatorId":candidate["mutant"]["operatorId"]}))
                .collect::<Vec<_>>(),
            "boundFiles":files.iter().map(|file| (file.path.to_string_lossy().into_owned(),file.hash.clone())).collect::<BTreeMap<_,_>>(),
            "adapterCleanup":transcript.steps["cleanup"]
        });
        let result = Self {
            discovery: json!({"steps":transcript.steps,"candidates":transcript.candidates}),
            expected,
            project: project.root.clone(),
            project_guard,
            project_hashes,
            framework,
            framework_guard,
            framework_hashes,
            files,
            binding,
        };
        result.verify()?;
        Ok(result)
    }

    pub(crate) fn framework(&self) -> &Path {
        &self.framework
    }
    pub(crate) fn binding(&self) -> Value {
        self.binding.clone()
    }
    pub(crate) fn protected_paths(&self) -> Vec<PathBuf> {
        std::iter::once(self.framework.clone())
            .chain(self.files.iter().map(|file| file.path.clone()))
            .collect()
    }

    pub(crate) fn verify(&self) -> Result<(), NamespaceError> {
        self.verify_before_final_check(|| {})
    }

    fn verify_before_final_check(&self, before_final: impl FnOnce()) -> Result<(), NamespaceError> {
        self.project_guard.verify()?;
        self.framework_guard.verify()?;
        for file in &self.files {
            file.verify()?;
        }
        let cancel = Cancellation::default();
        if source_hashes(&self.project, &cancel)? != self.project_hashes
            || source_hashes(&self.framework, &cancel)? != self.framework_hashes
        {
            return Err(invalid("replay project or framework inputs changed"));
        }
        before_final();
        // A retained leaf checked early must still have its original identity
        // and metadata after later inventories finish.
        for file in &self.files {
            file.verify_identity()?;
        }
        self.project_guard.verify()?;
        self.framework_guard.verify()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "retained_replay_tests.rs"]
mod tests;
