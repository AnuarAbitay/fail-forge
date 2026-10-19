//! Prepared package snapshots and byte bindings shared by profiles and campaigns.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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

pub fn inspect_project(root: &Path, cancel: &Cancellation) -> Result<PreparedProject, InputError> {
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
    if fs::symlink_metadata(root.join(".dart_tool")).is_ok_and(|info| info.file_type().is_symlink())
    {
        return Err(failure("symlink prepared configuration"));
    }
    for relative in [
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ] {
        let path = root.join(relative);
        if fs::symlink_metadata(&path).is_ok_and(|info| info.file_type().is_symlink()) {
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

pub(crate) fn file_uri(path: &Path) -> Result<String, InputError> {
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

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
