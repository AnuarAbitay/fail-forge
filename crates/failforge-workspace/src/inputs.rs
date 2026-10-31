//! Prepared package snapshots and byte bindings shared by profiles and campaigns.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Cancellation, InputError, failure};

pub type Hashes = BTreeMap<PathBuf, String>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedProject {
    pub root: PathBuf,
    pub name: String,
    pub packages: BTreeMap<String, PathBuf>,
    pub graph: Value,
    pub flutter: bool,
    pub framework_collision: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoundTree {
    pub root: PathBuf,
    pub files: Hashes,
    pub aliases: BTreeMap<PathBuf, BoundAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoundAlias {
    pub link: PathBuf,
    pub target: PathBuf,
}

pub fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>, InputError> {
    let bytes = read_regular_prefix(path, limit)?;
    if bytes.len() > limit {
        return Err(failure("prepared metadata exceeds its byte limit"));
    }
    Ok(bytes)
}

pub fn read_regular_prefix(path: &Path, limit: usize) -> Result<Vec<u8>, InputError> {
    let mut file = regular_file(path)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
pub(crate) fn regular_file(path: &Path) -> Result<File, InputError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(failure("input must be a regular file"));
    }
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn regular_file(path: &Path) -> Result<File, InputError> {
    Ok(crate::windows_fs::regular(path, false)?)
}

fn reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        crate::windows_fs::is_reparse(metadata)
    }
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

pub fn metadata_json(path: &Path) -> Result<Value, InputError> {
    parse_json(&read_regular(path, 8 * 1024 * 1024)?)
}

pub fn parse_json(bytes: &[u8]) -> Result<Value, InputError> {
    let guarded = guard_integer_tokens(bytes)?;
    serde_json::from_slice::<UniqueValue>(&guarded)
        .map(|value| value.0)
        .map_err(|_| failure("invalid or duplicate prepared project metadata"))
}

/// serde_json otherwise turns overflowing integer tokens into rounded f64s.
/// Inspect only unquoted numeric tokens; JSON grammar validation still belongs
/// to serde. Decimal/exponent tokens remain floats, even at integral values.
fn guard_integer_tokens(bytes: &[u8]) -> Result<Cow<'_, [u8]>, InputError> {
    let mut guarded = Cow::Borrowed(bytes);
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' {
            cursor += 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor = (cursor + 2).min(bytes.len()),
                    b'"' => {
                        cursor += 1;
                        break;
                    }
                    _ => cursor += 1,
                }
            }
            continue;
        }
        if bytes[cursor] != b'-' && !bytes[cursor].is_ascii_digit() {
            cursor += 1;
            continue;
        }
        let start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| matches!(byte, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            cursor += 1;
        }
        let token = &bytes[start..cursor];
        if token.iter().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
            continue;
        }
        // Every scanned byte is ASCII. Invalid signs or leading zeroes are
        // rejected either here or by serde's subsequent grammar validation.
        let text = std::str::from_utf8(token).expect("ASCII numeric token");
        let valid = if token[0] == b'-' {
            text.parse::<i64>().is_ok()
        } else {
            text.parse::<u64>().is_ok()
        };
        if !valid {
            return Err(failure(
                "prepared metadata integer exceeds the supported signed/unsigned 64-bit range",
            ));
        }
        if token == b"-0" {
            guarded.to_mut()[start] = b' ';
        }
    }
    Ok(guarded)
}

/// The supported URI subset is deliberately local, matching prepared Dart
/// package configuration: file URIs and references relative to package_config.
#[cfg(unix)]
fn local_uri(config: &Path, text: &str) -> Result<PathBuf, InputError> {
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

pub fn dependency_roots(config: &Path) -> Result<BTreeMap<String, PathBuf>, InputError> {
    let value = metadata_json(config)?;
    dependency_roots_from_value(config, &value)
}

/// Resolve the already captured package bytes against their original location.
/// Snapshot rebasing must not re-read a mutable source configuration here.
pub fn dependency_roots_from_value(
    config: &Path,
    value: &Value,
) -> Result<BTreeMap<String, PathBuf>, InputError> {
    if value.get("configVersion").and_then(Value::as_u64) != Some(2) {
        return Err(failure("package configuration must have version 2"));
    }
    let rows = value
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| failure("package configuration must contain a package list"))?;
    let mut result = BTreeMap::new();
    let mut roots = BTreeSet::new();
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
        if result.insert(name.to_owned(), path.clone()).is_some() {
            return Err(failure("duplicate prepared dependency"));
        }
        // Different package names cannot share a root. Compare the resolved
        // directories so URI spelling and filesystem aliases cannot hide it.
        if !roots.insert(path) {
            return Err(failure("prepared dependencies must have distinct roots"));
        }
    }
    Ok(result)
}

pub fn inspect_project(root: &Path, cancel: &Cancellation) -> Result<PreparedProject, InputError> {
    cancel.check()?;
    #[cfg(windows)]
    let _root_capability = crate::windows_fs::directory(root)?;
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
    let mut framework_collision = false;
    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        if name == own {
            framework_collision = true;
            break;
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
        graph,
        flutter,
        framework_collision,
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

#[cfg(windows)]
pub fn hash_file(path: &Path, cancel: &Cancellation) -> Result<String, InputError> {
    use crate::windows_fs as win;
    cancel.check()?;
    let at = |error| win::at_path(path, error);
    let parent = win::directory(path.parent().ok_or_else(|| failure("file has no parent"))?)?;
    let name = path
        .file_name()
        .ok_or_else(|| failure("file has no name"))?;
    let mut file = win::regular_at(&parent, name, false, false).map_err(at)?;
    cached_digest(&mut file, cancel, || win::has_writer_at(&parent, name), &at)
}

#[cfg(unix)]
pub fn hash_file(path: &Path, cancel: &Cancellation) -> Result<String, InputError> {
    cancel.check()?;
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

#[cfg(unix)]
fn tree(
    root: &Path,
    relative: &Path,
    source: bool,
    result: &mut BoundTree,
    cancel: &Cancellation,
    depth: usize,
) -> Result<(), InputError> {
    if depth > 128 {
        return Err(failure("input tree exceeds supported depth"));
    }
    cancel.check()?;
    let mut paths = fs::read_dir(root.join(relative))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for name in paths {
        cancel.check()?;
        let relative = relative.join(name);
        if skip(&relative, source) {
            continue;
        }
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path)?;
        if reparse(&metadata) {
            #[cfg(windows)]
            return Err(failure("reparse points are outside supported bound inputs"));
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
            if target.is_file() {
                result
                    .files
                    .insert(relative.clone(), hash_file(&target, cancel)?);
            } else if !target.is_dir() {
                return Err(failure("bound alias target must be a file or directory"));
            }
            result.aliases.insert(
                relative,
                BoundAlias {
                    link: text,
                    target: if target_relative.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        target_relative.to_owned()
                    },
                },
            );
        } else if metadata.is_dir() {
            tree(root, &relative, source, result, cancel, depth + 1)?;
        } else {
            result.files.insert(relative, hash_file(&path, cancel)?);
        }
    }
    Ok(())
}

pub fn source_hashes(root: &Path, cancel: &Cancellation) -> Result<Hashes, InputError> {
    let mut tree_binding = BoundTree {
        root: root.to_owned(),
        ..BoundTree::default()
    };
    tree(root, Path::new(""), true, &mut tree_binding, cancel, 0)?;
    let mut hashes = tree_binding.files;
    if fs::symlink_metadata(root.join(".dart_tool")).is_ok_and(|info| reparse(&info)) {
        return Err(failure("symlink prepared configuration"));
    }
    for relative in [
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ] {
        let path = root.join(relative);
        if fs::symlink_metadata(&path).is_ok_and(|info| reparse(&info)) {
            return Err(failure("symlink prepared configuration"));
        }
        if path.try_exists()? {
            hashes.insert(relative.into(), hash_file(&path, cancel)?);
        }
    }
    Ok(hashes)
}

pub fn bound_tree(root: &Path, cancel: &Cancellation) -> Result<BoundTree, InputError> {
    cancel.check()?;
    let root = root.canonicalize()?;
    let mut binding = BoundTree {
        root: root.clone(),
        ..BoundTree::default()
    };
    tree(&root, Path::new(""), false, &mut binding, cancel, 0)?;
    Ok(binding)
}

pub fn verify_source(
    root: &Path,
    expected: &Hashes,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    if source_hashes(root, cancel)? != *expected {
        return Err(failure(
            "project inputs changed during applicability analysis",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub fn file_uri(path: &Path) -> Result<String, InputError> {
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

#[cfg(unix)]
pub fn copy_snapshot(
    original: &PreparedProject,
    destination: &Path,
    before: &Hashes,
    cancel: &Cancellation,
) -> Result<PreparedProject, InputError> {
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

#[cfg(all(test, unix))]
#[path = "inputs_tests.rs"]
mod tests;

#[cfg(windows)]
#[path = "inputs_windows.rs"]
mod platform;
#[cfg(windows)]
pub use platform::file_uri;
#[cfg(windows)]
use platform::local_uri;
#[cfg(windows)]
pub fn copy_snapshot(
    original: &PreparedProject,
    destination: &Path,
    before: &Hashes,
    cancel: &Cancellation,
) -> Result<PreparedProject, InputError> {
    verify_source(&original.root, before, cancel)?;
    let copied = crate::copy_project(&original.root, destination, cancel)?;
    verify_source(&original.root, before, cancel)?;
    Ok(copied)
}

/// File digests from earlier walks in this process, by identity and stamp.
/// A campaign re-verifies the SDK and every dependency tree many times; with
/// real-time antivirus scanning each full read of the Dart SDK and Pub cache
/// took tens of seconds. Writes change the stamp, so edits are still found.
/// The cache never outlives the process, so separate executions hash afresh.
#[cfg(windows)]
static DIGESTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<crate::windows_fs::Stamp, String>>,
> = std::sync::LazyLock::new(Default::default);

#[cfg(windows)]
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The SHA-256 of an open regular file. A cached digest is reused only while
/// the stamp is unchanged and no other handle can write the file: NTFS may
/// move the times of an open writer only when it closes.
#[cfg(windows)]
fn cached_digest(
    file: &mut File,
    cancel: &Cancellation,
    has_writer: impl FnOnce() -> std::io::Result<bool>,
    at: &dyn Fn(std::io::Error) -> std::io::Error,
) -> Result<String, InputError> {
    use crate::windows_fs as win;
    let before = win::stamp(file).map_err(at)?;
    let cached = lock(&DIGESTS).get(&before).cloned();
    if let Some(digest) = cached {
        if !has_writer().map_err(at)? {
            return Ok(digest);
        }
    }
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        cancel.check()?;
        let count = file.read(&mut buffer).map_err(at)?;
        if count == 0 {
            break;
        }
        #[cfg(test)]
        HASHED_BYTES.with(|bytes| bytes.set(bytes.get() + count));
        digest.update(&buffer[..count]);
    }
    let digest = format!("{:x}", digest.finalize());
    // A write during the read changes the stamp, and a recent stamp may not
    // change on the next write within the same clock tick; cache neither.
    if before.settled() && win::stamp(file).map_err(at)? == before {
        lock(&DIGESTS).insert(before, digest.clone());
    }
    Ok(digest)
}

#[cfg(all(test, windows))]
thread_local! {
    /// File bytes this thread read to hash, so tests can observe cache hits.
    static HASHED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(windows)]
fn tree(
    root: &Path,
    relative: &Path,
    source: bool,
    result: &mut BoundTree,
    cancel: &Cancellation,
    depth: usize,
) -> Result<(), InputError> {
    fn visit(
        root: &Path,
        dir: &File,
        prefix: &Path,
        source: bool,
        result: &mut BoundTree,
        cancel: &Cancellation,
        depth: usize,
    ) -> Result<(), InputError> {
        use crate::windows_fs as win;
        if depth > 128 {
            return Err(failure("input tree exceeds supported depth"));
        }
        let at = |relative: &Path| {
            let path = root.join(relative);
            move |error| win::at_path(&path, error)
        };
        for name in win::names(dir).map_err(at(prefix))? {
            cancel.check()?;
            let relative = prefix.join(&name);
            if skip(&relative, source) {
                continue;
            }
            match win::directory_at(dir, &name) {
                Ok(child) => {
                    visit(root, &child, &relative, source, result, cancel, depth + 1)?;
                    win::same_entry(dir, &name, &child).map_err(at(&relative))?;
                }
                Err(directory) => {
                    // ERROR_DIRECTORY: the entry is not a directory, so the file
                    // open decides. Otherwise report both failed opens.
                    let mut file = match win::regular_at(dir, &name, false, false) {
                        Ok(file) => file,
                        Err(error) if directory.raw_os_error() == Some(267) => {
                            return Err(at(&relative)(error).into());
                        }
                        Err(error) => {
                            let both = std::io::Error::new(
                                directory.kind(),
                                format!("{directory}; opening it as a file: {error}"),
                            );
                            return Err(at(&relative)(both).into());
                        }
                    };
                    let path_error = at(&relative);
                    let digest = cached_digest(
                        &mut file,
                        cancel,
                        || win::has_writer_at(dir, &name),
                        &path_error,
                    )?;
                    win::same_entry(dir, &name, &file).map_err(at(&relative))?;
                    result.files.insert(
                        PathBuf::from(
                            relative
                                .to_str()
                                .ok_or_else(|| failure("input paths must be UTF-8"))?
                                .replace('\\', "/"),
                        ),
                        digest,
                    );
                }
            }
        }
        Ok(())
    }
    let directory = crate::windows_fs::directory(&root.join(relative))?;
    visit(root, &directory, relative, source, result, cancel, depth)
}

#[cfg(all(test, windows))]
mod windows_tree_tests {
    use super::*;
    use std::fs;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandleEx, SetFileInformationByHandle,
    };

    fn times(file: &fs::File) -> FILE_BASIC_INFO {
        let mut times = FILE_BASIC_INFO::default();
        let size = size_of::<FILE_BASIC_INFO>() as u32;
        let handle = file.as_raw_handle();
        assert_ne!(
            unsafe {
                GetFileInformationByHandleEx(handle, FileBasicInfo, (&raw mut times).cast(), size)
            },
            0
        );
        times
    }

    fn set_times(file: &fs::File, times: &FILE_BASIC_INFO) {
        let size = size_of::<FILE_BASIC_INFO>() as u32;
        let handle = file.as_raw_handle();
        assert_ne!(
            unsafe {
                SetFileInformationByHandle(
                    handle,
                    FileBasicInfo,
                    std::ptr::from_ref(times).cast(),
                    size,
                )
            },
            0
        );
    }

    /// Move the write and change times `hours` back, so the stamp is settled.
    /// The whole test may run within one clock tick; distinct ages keep the
    /// stamps of successive contents distinct.
    fn age(path: &Path, hours: i64) {
        let file = fs::File::options().write(true).open(path).unwrap();
        let mut old = times(&file);
        old.LastWriteTime -= hours * 3600 * 10_000_000;
        old.ChangeTime -= hours * 3600 * 10_000_000;
        set_times(&file, &old);
    }

    #[test]
    fn repeated_walks_reuse_digests_until_a_file_is_written() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-tree-cache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let file = root.join("a.dart");
        let read = || HASHED_BYTES.with(|bytes| bytes.replace(0));
        let walk = || {
            let tree = bound_tree(&root, &Cancellation::default()).unwrap();
            (read(), tree.files[Path::new("a.dart")].clone())
        };
        let sha = |text: &str| format!("{:x}", Sha256::digest(text));
        fs::write(&file, "one").unwrap();
        read();
        // A just-written file may be rewritten within the same clock tick
        // without moving its times, so its digest is never cached.
        assert_eq!(walk(), (3, sha("one")));
        assert_eq!(walk(), (3, sha("one")));
        fs::write(&file, "two").unwrap();
        assert_eq!(walk(), (3, sha("two")));
        // A settled file is read once and then reused.
        age(&file, 1);
        assert_eq!(walk(), (3, sha("two")));
        assert_eq!(walk(), (0, sha("two")));
        // Same size, new content: the write time and change time move.
        fs::write(&file, "six").unwrap();
        assert_eq!(walk(), (3, sha("six")));
        age(&file, 2);
        assert_eq!(walk(), (3, sha("six")));
        // Restoring the write time cannot hide a write: the change time moved.
        let modified = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "ten").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(fs::metadata(&file).unwrap().modified().unwrap(), modified);
        let restored = walk();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(restored, (3, sha("ten")));
    }

    #[test]
    fn an_open_writer_bypasses_the_cached_digest_even_with_unchanged_times() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-tree-writer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let file = root.join("a.dart");
        fs::write(&file, "one").unwrap();
        age(&file, 3);
        let read = || HASHED_BYTES.with(|bytes| bytes.replace(0));
        let digest = |tree: &BoundTree| tree.files[Path::new("a.dart")].clone();
        let sha = |text: &str| format!("{:x}", Sha256::digest(text));
        read();
        assert_eq!(
            digest(&bound_tree(&root, &Cancellation::default()).unwrap()),
            sha("one")
        );
        assert_eq!(
            hash_file(&file, &Cancellation::default()).unwrap(),
            sha("one")
        );
        assert_eq!(read(), 3, "the second read is a cache hit");
        // An open writer may move the times only when it closes, so the stamp
        // cannot vouch for the content while it stays open, even unchanged.
        // Opening for writing already moves the change time on NTFS; put the
        // cached times back to reach the writer check itself.
        let cached_times = times(&fs::File::open(&file).unwrap());
        let writer = fs::File::options().write(true).open(&file).unwrap();
        set_times(&writer, &cached_times);
        let tree = bound_tree(&root, &Cancellation::default()).unwrap();
        let single = hash_file(&file, &Cancellation::default()).unwrap();
        assert_eq!((digest(&tree), single), (sha("one"), sha("one")));
        assert_eq!(
            read(),
            6,
            "both reads bypass the cache while a writer is open"
        );
        drop(writer);
        assert_eq!(
            hash_file(&file, &Cancellation::default()).unwrap(),
            sha("one")
        );
        assert_eq!(
            read(),
            0,
            "the cache is trusted again once the writer closed"
        );
        // A writer that changed the file cannot put the change time back
        // while it stays open; NTFS reports the write.
        let mut writer = fs::File::options().write(true).open(&file).unwrap();
        let settled = times(&writer);
        writer.write_all(b"two").unwrap();
        set_times(&writer, &settled);
        let changed = hash_file(&file, &Cancellation::default()).unwrap();
        drop(writer);
        assert_eq!((changed, read()), (sha("two"), 3));
        // A file locked against every other open fails as a file, without the
        // directory open's error appended.
        let locked = fs::File::options()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let error = bound_tree(&root, &Cancellation::default())
            .unwrap_err()
            .to_string();
        drop(locked);
        let _ = fs::remove_dir_all(&root);
        assert!(error.contains(&crate::display_path(&file)), "{error}");
        assert!(!error.contains("opening it as a file"), "{error}");
    }

    #[test]
    fn a_writer_closed_between_the_cached_stamp_and_probe_cannot_hide_an_edit() {
        use crate::windows_fs as win;
        use std::cell::Cell;
        use std::ffi::OsStr;
        use std::io::Write;

        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-tree-closed-writer-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("a.dart");
        fs::write(&path, b"one").unwrap();
        age(&path, 3);
        let cancel = Cancellation::default();
        let sha = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let read = || HASHED_BYTES.with(|bytes| bytes.replace(0));
        read();
        assert_eq!(hash_file(&path, &cancel).unwrap(), sha(b"one"));
        assert_eq!(hash_file(&path, &cancel).unwrap(), sha(b"one"));
        assert_eq!(read(), 3, "the second settled read must use the cache");

        let parent = win::directory(&root).unwrap();
        let name = OsStr::new("a.dart");
        let mut input = win::regular_at(&parent, name, false, false).unwrap();
        let identity = win::identity(&input).unwrap();
        let before = win::stamp(&input).unwrap();
        assert!(before.settled());
        let injected = Cell::new(false);
        let probe_busy = Cell::new(None);
        let at = |error| win::at_path(&path, error);
        let actual = cached_digest(
            &mut input,
            &cancel,
            || {
                // This callback runs after cached_digest reads the old stamp.
                // Finish a same-size edit before the real writer probe runs;
                // no sleep, racing thread, or forged post-write time is needed.
                let mut writer = fs::File::options().write(true).open(&path)?;
                writer.write_all(b"two")?;
                writer.flush()?;
                drop(writer);
                injected.set(true);
                let busy = win::has_writer_at(&parent, name)?;
                probe_busy.set(Some(busy));
                Ok(busy)
            },
            &at,
        )
        .unwrap();
        let after = win::stamp(&input).unwrap();
        assert_eq!(win::identity(&input).unwrap(), identity);
        drop(input);
        drop(parent);
        fs::remove_dir_all(&root).unwrap();

        assert!(injected.get(), "the cache-hit writer probe must execute");
        assert_eq!(
            probe_busy.get(),
            Some(false),
            "the completed writer must be absent during the actual probe"
        );
        assert_ne!(after, before, "the completed edit must move its stamp");
        assert_eq!(
            actual,
            sha(b"two"),
            "a closed writer left stale cached bytes"
        );
    }

    #[test]
    fn walks_deeper_than_the_supported_depth_are_rejected() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-tree-depth-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // A walk accepts directories nested 128 deep and rejects 129.
        let nested = |name: &str, depth| {
            let top = root.join(name);
            fs::create_dir_all((0..depth).fold(top.clone(), |path, _| path.join("d"))).unwrap();
            top
        };
        let (deep, shallow) = (nested("deep", 129), nested("ok", 128));
        let error = bound_tree(&deep, &Cancellation::default())
            .unwrap_err()
            .to_string();
        let accepted = bound_tree(&shallow, &Cancellation::default());
        let _ = fs::remove_dir_all(&root);
        assert!(
            error.contains("input tree exceeds supported depth"),
            "{error}"
        );
        accepted.unwrap();
    }

    #[test]
    fn tree_errors_name_the_entry_and_keep_the_directory_open_reason() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-tree-errors-{}", std::process::id()));
        let tree = root.join("tree");
        let outside = root.join("outside");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(tree.join("nested").join("file.dart"), "void main() {}").unwrap();
        bound_tree(&tree, &Cancellation::default()).unwrap();
        // Junction creation needs no symlink privilege.
        let alias = tree.join("nested").join("alias");
        assert!(
            std::process::Command::new("cmd.exe")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&alias)
                .arg(&outside)
                .output()
                .unwrap()
                .status
                .success()
        );
        let error = bound_tree(&tree, &Cancellation::default())
            .unwrap_err()
            .to_string();
        let _ = fs::remove_dir(&alias);
        let _ = fs::remove_dir_all(&root);
        assert!(
            error.starts_with("cannot read or copy workspace inputs: "),
            "{error}"
        );
        assert!(error.contains(&crate::display_path(&alias)), "{error}");
        assert!(!error.contains(r"\\?\"), "{error}");
        assert!(
            error.contains("reparse points are outside supported workspaces"),
            "{error}"
        );
    }
}
