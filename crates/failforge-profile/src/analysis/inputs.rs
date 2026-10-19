//! Prepared package snapshots and byte bindings used by static analysis.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Cancellation, failure};
use crate::ProfileError;

pub(super) type Hashes = BTreeMap<PathBuf, String>;

#[derive(Debug, Clone)]
pub(super) struct PreparedProject {
    pub root: PathBuf,
    pub name: String,
    pub packages: BTreeMap<String, PathBuf>,
    pub flutter: bool,
}

pub(super) fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>, ProfileError> {
    let bytes = read_regular_prefix(path, limit)?;
    if bytes.len() > limit {
        return Err(failure("prepared metadata exceeds its byte limit"));
    }
    Ok(bytes)
}

pub(super) fn read_regular_prefix(path: &Path, limit: usize) -> Result<Vec<u8>, ProfileError> {
    let mut file = regular_file(path)?;
    let mut bytes = Vec::new();
    (&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn regular_file(path: &Path) -> Result<File, ProfileError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure("input must be a regular file"));
    }
    Ok(file)
}

// serde_json::Value alone accepts duplicate keys. Prepared dependency roots must
// have one unambiguous interpretation for this owner and the Dart analyzer.
struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor;
        impl<'de> Visitor<'de> for UniqueVisitor {
            type Value = UniqueValue;
            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a JSON value with unique keys")
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(UniqueValue(json!(value)))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                self.visit_none()
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
                let mut output = vec![];
                while let Some(UniqueValue(value)) = input.next_element()? {
                    output.push(value);
                }
                Ok(UniqueValue(output.into()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut input: A) -> Result<Self::Value, A::Error> {
                let mut output = serde_json::Map::new();
                while let Some((key, UniqueValue(value))) =
                    input.next_entry::<String, UniqueValue>()?
                {
                    if output.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate project metadata key"));
                    }
                }
                Ok(UniqueValue(output.into()))
            }
        }
        deserializer.deserialize_any(UniqueVisitor)
    }
}

pub(super) fn metadata_json(path: &Path) -> Result<Value, ProfileError> {
    parse_json(&read_regular(path, 8 * 1024 * 1024)?)
}

pub(super) fn parse_json(bytes: &[u8]) -> Result<Value, ProfileError> {
    serde_json::from_slice::<UniqueValue>(bytes)
        .map(|value| value.0)
        .map_err(|_| failure("invalid or duplicate prepared project metadata"))
}

/// The supported URI subset is deliberately local, matching prepared Dart
/// package configuration: file URIs and references relative to package_config.
fn local_uri(config: &Path, text: &str) -> Result<PathBuf, ProfileError> {
    if text.contains('\0') || text.contains('\\') {
        return Err(failure("invalid prepared dependency URI"));
    }
    // URI query and fragment are not part of the filesystem path.
    let path = text.split(['?', '#']).next().unwrap_or_default();
    let file_scheme = path
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"));
    let path = if file_scheme {
        let rest = &path[5..];
        if let Some(rest) = rest.strip_prefix("//") {
            let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
            if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
                return Err(failure("dependencies must already exist locally"));
            }
            format!("/{path}")
        } else {
            rest.to_owned()
        }
    } else if let Some(rest) = path.strip_prefix("//") {
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return Err(failure("dependencies must already exist locally"));
        }
        format!("/{path}")
    } else if path.split('/').next().unwrap_or_default().contains(':') {
        return Err(failure("dependencies must already exist locally"));
    } else {
        path.to_owned()
    };
    if path.is_empty() {
        return Err(failure("prepared dependency URI has no directory path"));
    }
    // Resolve URI references before percent-decoding: an encoded leading slash
    // remains within its relative URI component instead of replacing the base.
    let joined = if path.starts_with('/') {
        path
    } else {
        let base = file_uri(
            config
                .parent()
                .ok_or_else(|| failure("package configuration has no parent"))?,
        )?;
        format!("{}/{path}", base.strip_prefix("file://").expect("file URI"))
    };
    // URI dot-segments are removed before filesystem aliases are resolved.
    let mut segments = vec![];
    for segment in joined.split('/') {
        match segment {
            "" | "." => (),
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    let normalized = format!("/{}", segments.join("/"));
    let mut decoded = Vec::with_capacity(normalized.len());
    let bytes = normalized.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
            if let (Some(first), Some(second)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(first * 16 + second);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    let decoded =
        String::from_utf8(decoded).map_err(|_| failure("dependency URI must be UTF-8"))?;
    if decoded.contains('\0') {
        return Err(failure("invalid prepared dependency URI"));
    }
    let root = Path::new(&decoded).canonicalize()?;
    if !root.is_dir() {
        return Err(failure("prepared dependency is not a directory"));
    }
    Ok(root)
}

pub(super) fn dependency_roots(config: &Path) -> Result<BTreeMap<String, PathBuf>, ProfileError> {
    let value = metadata_json(config)?;
    if value.get("configVersion").and_then(Value::as_u64) != Some(2) {
        return Err(failure("package configuration must have version 2"));
    }
    let rows = value
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("package configuration must contain a package list"))?;
    let mut result = BTreeMap::new();
    for row in rows {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| failure("invalid dependency entry"))?;
        let uri = row
            .get("rootUri")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("invalid dependency entry"))?;
        let path = local_uri(config, uri)?;
        if result.insert(name.to_owned(), path).is_some() {
            return Err(failure("duplicate prepared dependency"));
        }
    }
    Ok(result)
}

pub(super) fn inspect_project(
    root: &Path,
    cancel: &Cancellation,
) -> Result<PreparedProject, ProfileError> {
    cancel.check()?;
    let root = root.canonicalize()?;
    if !root.join("pubspec.yaml").is_file() {
        return Err(failure("project must contain pubspec.yaml"));
    }
    let graph = metadata_json(&root.join(".dart_tool/package_graph.json"))?;
    let roots = graph
        .get("roots")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("one prepared standalone package is required"))?;
    let [own] = roots.as_slice() else {
        return Err(failure("one prepared standalone package is required"));
    };
    let own = own
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| failure("one prepared standalone package is required"))?;
    let packages = dependency_roots(&root.join(".dart_tool/package_config.json"))?;
    if packages.get(own) != Some(&root) {
        return Err(failure(
            "package configuration does not identify the project",
        ));
    }
    let mut adjacency = BTreeMap::new();
    for row in graph
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("dependency graph must contain a package list"))?
    {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| failure("invalid dependency graph"))?;
        let deps = match row.get("dependencies") {
            None => vec![],
            Some(value) => value
                .as_array()
                .ok_or_else(|| failure("invalid dependency graph"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| failure("invalid dependency graph"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        if adjacency.insert(name.to_owned(), deps).is_some() {
            return Err(failure("duplicate dependency graph package"));
        }
    }
    if !adjacency.contains_key(own) {
        return Err(failure("selected package missing from dependency graph"));
    }
    let flutter = packages.contains_key("flutter_test");
    if !flutter && !packages.contains_key("test") {
        return Err(failure(
            "prepare a test or flutter_test dependency before analysis",
        ));
    }
    let mut pending: Vec<_> = ["test", "test_core", "test_api", "flutter_test"]
        .into_iter()
        .filter(|name| packages.contains_key(*name))
        .map(str::to_owned)
        .collect();
    let mut visited = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        if name == own {
            continue;
        }
        pending.extend(
            adjacency
                .get(&name)
                .ok_or_else(|| failure("test dependency missing from package graph"))?
                .iter()
                .cloned(),
        );
    }
    source_hashes(&root, cancel)?;
    Ok(PreparedProject {
        root,
        name: own.to_owned(),
        packages,
        flutter,
    })
}

fn skip(relative: &Path, source: bool) -> bool {
    let parts: Vec<_> = relative.components().map(|part| part.as_os_str()).collect();
    parts.iter().any(|part| {
        *part == ".git"
            || *part == ".dart_tool"
            || (source && (*part == ".failforge" || *part == "__pycache__"))
    }) || parts.first().is_some_and(|part| *part == "build")
        || source
            && parts.len() == 1
            && relative.to_str().is_some_and(|name| {
                name.starts_with("incremental_kernel") && !name.ends_with(".dart")
            })
}

pub(super) fn hash_file(path: &Path, cancel: &Cancellation) -> Result<String, ProfileError> {
    let mut file = regular_file(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        cancel.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn tree(
    root: &Path,
    relative: &Path,
    source: bool,
    result: &mut Hashes,
    cancel: &Cancellation,
    depth: usize,
) -> Result<(), ProfileError> {
    if depth > 128 {
        return Err(failure("input tree exceeds supported depth"));
    }
    cancel.check()?;
    let mut paths = fs::read_dir(root.join(relative))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for name in paths {
        let relative = relative.join(name);
        if skip(&relative, source) {
            continue;
        }
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            if source {
                return Err(failure("project symlinks are outside supported snapshots"));
            }
            let target = path.canonicalize()?;
            let target_relative = target
                .strip_prefix(root)
                .map_err(|_| failure("external alias in bound inputs"))?;
            if skip(target_relative, false) {
                return Err(failure("input alias points into excluded tool state"));
            }
            let text = fs::read_link(&path)?;
            let content = if target.is_file() {
                hash_file(&target, cancel)?
            } else {
                String::new()
            };
            result.insert(
                relative,
                format!("alias:{text:?}:{target_relative:?}:{content}"),
            );
        } else if metadata.is_dir() {
            tree(root, &relative, source, result, cancel, depth + 1)?;
        } else {
            result.insert(relative, hash_file(&path, cancel)?);
        }
    }
    Ok(())
}

pub(super) fn source_hashes(root: &Path, cancel: &Cancellation) -> Result<Hashes, ProfileError> {
    let mut hashes = BTreeMap::new();
    tree(root, Path::new(""), true, &mut hashes, cancel, 0)?;
    if fs::symlink_metadata(root.join(".dart_tool")).is_ok_and(|info| info.file_type().is_symlink())
    {
        return Err(failure("symlink prepared configuration"));
    }
    for relative in [
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ] {
        let path = root.join(relative);
        if path.try_exists()? {
            hashes.insert(relative.into(), hash_file(&path, cancel)?);
        }
    }
    Ok(hashes)
}

pub(super) fn bound_tree(root: &Path, cancel: &Cancellation) -> Result<Hashes, ProfileError> {
    let mut hashes = BTreeMap::new();
    tree(root, Path::new(""), false, &mut hashes, cancel, 0)?;
    Ok(hashes)
}

pub(super) fn verify_source(
    root: &Path,
    expected: &Hashes,
    cancel: &Cancellation,
) -> Result<(), ProfileError> {
    if source_hashes(root, cancel)? != *expected {
        return Err(failure(
            "project inputs changed during applicability analysis",
        ));
    }
    Ok(())
}

fn file_uri(path: &Path) -> Result<String, ProfileError> {
    let text = path
        .to_str()
        .ok_or_else(|| failure("dependency path must be UTF-8"))?;
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}

pub(super) fn copy_snapshot(
    original: &PreparedProject,
    destination: &Path,
    before: &Hashes,
    cancel: &Cancellation,
) -> Result<PreparedProject, ProfileError> {
    fs::create_dir(destination)?;
    for relative in before.keys() {
        cancel.check()?;
        let source = original.root.join(relative);
        let target = destination.join(relative);
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| failure("snapshot path has no parent"))?,
        )?;
        let mut input = regular_file(&source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        let mut buffer = [0; 64 * 1024];
        loop {
            cancel.check()?;
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
        }
        output.set_permissions(fs::Permissions::from_mode(
            input.metadata()?.permissions().mode() & 0o777,
        ))?;
    }
    if source_hashes(destination, cancel)? != *before {
        return Err(failure(
            "copied snapshot inputs do not match frozen project",
        ));
    }
    verify_source(&original.root, before, cancel)?;
    let config = destination.join(".dart_tool/package_config.json");
    let mut metadata = metadata_json(&config)?;
    for package in metadata["packages"]
        .as_array_mut()
        .ok_or_else(|| failure("invalid snapshot package list"))?
    {
        let name = package["name"]
            .as_str()
            .ok_or_else(|| failure("invalid snapshot package name"))?;
        package["rootUri"] = if name == original.name {
            json!("../")
        } else {
            json!(file_uri(
                original
                    .packages
                    .get(name)
                    .ok_or_else(|| failure("snapshot dependency changed"))?
            )?)
        };
    }
    fs::write(
        config,
        serde_json::to_vec(&metadata).map_err(|_| failure("cannot serialize prepared metadata"))?,
    )?;
    inspect_project(destination, cancel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "failforge-profile-inputs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root.canonicalize().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn duplicate_metadata_is_rejected_at_every_nesting_level() {
        for text in [
            r#"{"packages":[],"packages":[]}"#,
            r#"{"packages":[{"name":"a","name":"b"}]}"#,
        ] {
            assert!(serde_json::from_str::<UniqueValue>(text).is_err());
        }
        assert!(
            serde_json::from_str::<UniqueValue>(r#"{"n":null,"b":true,"a":[1,2.5,"s"]}"#).is_ok()
        );
    }

    #[test]
    fn local_package_uris_preserve_spaces_unicode_and_relative_roots() {
        let fixture = Fixture::new();
        let package = fixture.0.join("some space Ж");
        fs::create_dir(&package).unwrap();
        let config = fixture.0.join("config.json");
        assert_eq!(
            local_uri(&config, "some%20space%20%D0%96").unwrap(),
            package
        );
        assert_eq!(
            local_uri(&config, "%2Fsome%20space%20%D0%96").unwrap(),
            package
        );
        assert_eq!(
            local_uri(
                &config,
                &file_uri(&package).unwrap().replacen("file:", "FILE:", 1)
            )
            .unwrap(),
            package
        );
        assert_eq!(
            local_uri(&config, &file_uri(&package).unwrap()[5..]).unwrap(),
            package
        );
        let outside = Fixture::new();
        symlink(&outside.0, fixture.0.join("alias")).unwrap();
        assert_eq!(
            local_uri(&config, "alias/../some%20space%20%D0%96").unwrap(),
            package
        );
        assert_eq!(
            local_uri(&config, &file_uri(&package).unwrap()).unwrap(),
            package
        );
        assert_eq!(
            local_uri(
                &config,
                &format!(
                    "file://localhost{}",
                    file_uri(&package).unwrap().strip_prefix("file://").unwrap()
                )
            )
            .unwrap(),
            package
        );
        for bad in [
            "https://example.com/a",
            "file://remote/a",
            "//remote/a",
            "a%00b",
            "a\\b",
            "",
            "#fragment",
            "file:",
        ] {
            assert!(local_uri(&config, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn snapshot_rebases_only_prepared_roots_and_retains_frozen_source_bytes() {
        let fixture = Fixture::new();
        let original = fixture.0.join("original");
        let test_package = fixture.0.join("test dependency");
        fs::create_dir_all(original.join(".dart_tool/test")).unwrap();
        fs::create_dir_all(original.join("lib")).unwrap();
        fs::create_dir(&test_package).unwrap();
        fs::write(original.join("pubspec.yaml"), "name: subject\n").unwrap();
        fs::write(original.join("lib/source.dart"), "bool f() => true;\n").unwrap();
        fs::write(original.join(".dart_tool/test/old-cache"), "disposable").unwrap();
        let config = json!({"configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/"},{"name":"test","rootUri":file_uri(&test_package).unwrap(),"packageUri":"lib/"}]});
        let graph = json!({"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]});
        fs::write(
            original.join(".dart_tool/package_config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        fs::write(
            original.join(".dart_tool/package_graph.json"),
            serde_json::to_vec(&graph).unwrap(),
        )
        .unwrap();
        let cancel = Cancellation::default();
        let project = inspect_project(&original, &cancel).unwrap();
        let hashes = source_hashes(&original, &cancel).unwrap();
        let snapshot =
            copy_snapshot(&project, &fixture.0.join("snapshot"), &hashes, &cancel).unwrap();
        assert_eq!(snapshot.packages["subject"], snapshot.root);
        assert_eq!(snapshot.packages["test"], test_package);
        assert_eq!(
            fs::read(snapshot.root.join("lib/source.dart")).unwrap(),
            fs::read(original.join("lib/source.dart")).unwrap()
        );
        assert!(!snapshot.root.join(".dart_tool/test").exists());
        verify_source(&original, &hashes, &cancel).unwrap();
        fs::write(original.join("lib/source.dart"), "changed").unwrap();
        assert!(
            copy_snapshot(
                &project,
                &fixture.0.join("changed-snapshot"),
                &hashes,
                &cancel
            )
            .is_err()
        );
    }

    #[test]
    fn hashes_detect_additions_changes_and_reject_source_symlinks() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("source.dart"), "one").unwrap();
        let cancel = Cancellation::default();
        let before = source_hashes(&fixture.0, &cancel).unwrap();
        fs::create_dir(fixture.0.join(".failforge")).unwrap();
        fs::write(fixture.0.join(".failforge/report"), "ignored").unwrap();
        verify_source(&fixture.0, &before, &cancel).unwrap();
        fs::write(fixture.0.join("source.dart"), "two").unwrap();
        assert!(verify_source(&fixture.0, &before, &cancel).is_err());
        symlink("source.dart", fixture.0.join("alias.dart")).unwrap();
        assert!(source_hashes(&fixture.0, &cancel).is_err());
        assert!(bound_tree(&fixture.0, &cancel).is_ok());
        fs::remove_file(fixture.0.join("alias.dart")).unwrap();
        symlink("/", fixture.0.join("external")).unwrap();
        assert!(bound_tree(&fixture.0, &cancel).is_err());
    }

    #[test]
    fn file_reader_never_blocks_on_fifo_and_enforces_size() {
        let fixture = Fixture::new();
        let path = fixture.0.join("pipe");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: the C string is live and names a new test-only path.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_regular(&path, 5).is_err());
        fs::write(fixture.0.join("file"), b"123456").unwrap();
        assert!(read_regular(&fixture.0.join("file"), 5).is_err());
    }

    #[test]
    fn streaming_hashes_match_sha256_vectors_across_multiple_read_blocks() {
        let fixture = Fixture::new();
        let path = fixture.0.join("input");
        let cancel = Cancellation::default();
        for (bytes, expected) in [
            (
                vec![],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".to_vec(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                vec![b'a'; 1_000_000],
                "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            ),
        ] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(hash_file(&path, &cancel).unwrap(), expected);
        }
        cancel.request(libc::SIGTERM);
        assert!(matches!(
            hash_file(&path, &cancel),
            Err(ProfileError::Cancelled(libc::SIGTERM))
        ));
    }
}
