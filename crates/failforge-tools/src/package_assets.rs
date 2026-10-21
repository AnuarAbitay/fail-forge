use crate::common::{self, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

pub(super) const FIXED: &[&str] = &[
    "README.md",
    "CHANGELOG.md",
    "LICENSE",
    "NOTICE",
    "SECURITY.md",
    "THIRD_PARTY_NOTICES.md",
    "docs/installation.md",
    "docs/doctor.md",
    "docs/packaging.md",
    "docs/beta-limitations.md",
    "docs/cli.md",
    "docs/mutation-cli.md",
    "docs/mutation-operators.md",
    "docs/dart-semantic-mutations.md",
    "docs/dependency-licenses.md",
    "adapters/dart/pubspec.yaml",
    "adapters/dart/pubspec.lock",
    "adapters/dart/LICENSE",
    "adapters/dart/NOTICE",
    "third-party/inventory.json",
    "third-party/SHA256SUMS",
];

pub(super) const RUNTIME: &[(&str, &str)] = &[
    (
        "share/doc/rust/COPYRIGHT-library.html",
        "COPYRIGHT-library.html",
    ),
    ("share/doc/rust/licenses/Apache-2.0.txt", "Apache-2.0.txt"),
    ("share/doc/rust/licenses/MIT.txt", "MIT.txt"),
    ("share/doc/rust/licenses/Unicode-3.0.txt", "Unicode-3.0.txt"),
    (
        "share/doc/rust/licenses/BSD-2-Clause.txt",
        "BSD-2-Clause.txt",
    ),
    (
        "share/doc/rust/licenses/LLVM-exception.txt",
        "LLVM-exception.txt",
    ),
    (
        "lib/rustlib/src/rust/library/compiler-builtins/LICENSE.txt",
        "compiler-builtins-LICENSE.txt",
    ),
    (
        "lib/rustlib/src/rust/library/compiler-builtins/libm/LICENSE.txt",
        "libm-LICENSE.txt",
    ),
];

pub(super) fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
}

pub(super) fn regular(root: &Path, relative: &str) -> Result<fs::File> {
    ensure(safe_relative(relative), "unsafe package path")?;
    let mut path = root.to_owned();
    let components: Vec<_> = Path::new(relative).components().collect();
    for (index, part) in components.iter().enumerate() {
        path.push(part.as_os_str());
        let metadata = fs::symlink_metadata(&path)?;
        ensure(
            if index + 1 == components.len() {
                metadata.is_file()
            } else {
                metadata.is_dir()
            },
            format!("package input is a symlink or special file: {relative}"),
        )?;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure(
        file.metadata()?.is_file(),
        "package input is not a regular file",
    )?;
    Ok(file)
}

pub(super) fn digest(root: &Path, relative: &str) -> Result<String> {
    let mut source = regular(root, relative)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        common::check_interrupt()?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn license_path(path: &str) -> bool {
    path.strip_prefix("third-party/licenses/")
        .and_then(|p| p.strip_suffix(".txt"))
        .is_some_and(|p| {
            p.len() == 64
                && p.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

fn adapter_source(path: &str) -> bool {
    let relative = path
        .strip_prefix("adapters/dart/bin/")
        .or_else(|| path.strip_prefix("adapters/dart/lib/"));
    relative.is_some_and(|relative| {
        relative.ends_with(".dart")
            && relative.split('/').all(|part| {
                !part.starts_with('.')
                    && ![
                        "target",
                        "build",
                        "cache",
                        "caches",
                        "artifacts",
                        "reports",
                        "tmp",
                        "temp",
                        "test",
                        "tests",
                        "node_modules",
                    ]
                    .contains(&part)
            })
    })
}

pub(super) fn select(tracked: &[String]) -> Result<BTreeSet<String>> {
    let mut selected: BTreeSet<_> = FIXED.iter().map(|p| (*p).to_owned()).collect();
    for path in tracked {
        ensure(safe_relative(path), "unsafe tracked path")?;
        if license_path(path) || adapter_source(path) {
            selected.insert(path.clone());
        }
    }
    for entry in [
        "adapters/dart/bin/failforge_dart_adapter.dart",
        "adapters/dart/bin/failforge_dart_namespace.dart",
    ] {
        ensure(
            selected.contains(entry),
            format!("missing tracked adapter entrypoint: {entry}"),
        )?;
    }
    ensure(
        selected.iter().any(|p| license_path(p)),
        "missing tracked dependency license texts",
    )?;
    Ok(selected)
}

pub(super) fn snapshot(
    root: &Path,
    paths: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<BTreeMap<String, String>> {
    paths
        .into_iter()
        .map(|p| {
            let p = p.as_ref();
            Ok((p.to_owned(), digest(root, p)?))
        })
        .collect()
}

pub(super) fn tree_digest(tree: &BTreeMap<String, String>) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(tree)?)))
}

pub(super) fn validate_notices(root: &Path, selected: &BTreeSet<String>) -> Result<()> {
    let inventory: Value = serde_json::from_reader(regular(root, "third-party/inventory.json")?)?;
    let inputs = inventory["inputs"]
        .as_array()
        .ok_or("missing license input bindings")?;
    let required = [
        "Cargo.lock",
        "adapters/dart/pubspec.yaml",
        "adapters/dart/pubspec.lock",
    ];
    let mut seen = BTreeSet::new();
    for input in inputs {
        let path = input["path"].as_str().ok_or("invalid license input path")?;
        ensure(
            required.contains(&path) && seen.insert(path),
            "unexpected or duplicate license input binding",
        )?;
        ensure(
            input["sha256"] == digest(root, path)?,
            format!("dependency license inventory is stale: {path}"),
        )?;
    }
    ensure(
        seen.len() == required.len(),
        "incomplete dependency license input bindings",
    )?;
    let mut checksums = String::new();
    regular(root, "third-party/SHA256SUMS")?.read_to_string(&mut checksums)?;
    let expected: BTreeSet<_> = selected
        .iter()
        .filter(|p| {
            license_path(p)
                || ["THIRD_PARTY_NOTICES.md", "third-party/inventory.json"].contains(&p.as_str())
        })
        .cloned()
        .collect();
    let mut verified = BTreeMap::new();
    for line in checksums.lines() {
        let (hash, path) = line
            .split_once("  ")
            .ok_or("invalid dependency checksum line")?;
        ensure(
            expected.contains(path) && verified.insert(path.to_owned(), hash.to_owned()).is_none(),
            "unexpected or duplicate dependency checksum path",
        )?;
        ensure(
            hash == digest(root, path)?,
            format!("dependency notice checksum mismatch: {path}"),
        )?;
        ensure(
            !license_path(path) || path == format!("third-party/licenses/{hash}.txt"),
            "dependency notice filename must match its content checksum",
        )?;
    }
    ensure(
        verified.keys().cloned().collect::<BTreeSet<_>>() == expected,
        "incomplete dependency notice checksums",
    )?;
    fn references(value: &Value, verified: &BTreeMap<String, String>) -> Result<()> {
        match value {
            Value::Object(map) => {
                if let Some(file) = map.get("file") {
                    let path = format!(
                        "third-party/{}",
                        file.as_str().ok_or("invalid retained notice path")?
                    );
                    ensure(
                        license_path(&path)
                            && verified.get(&path).is_some_and(|hash| {
                                map.get("sha256").and_then(Value::as_str) == Some(hash)
                            }),
                        "inventory notice is absent or has a different checksum",
                    )?;
                }
                for child in map.values() {
                    references(child, verified)?;
                }
            }
            Value::Array(items) => {
                for child in items {
                    references(child, verified)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    references(&inventory, &verified)
}

pub(super) fn copy(
    root: &Path,
    relative: &str,
    destination: &Path,
    output: &str,
    executable: bool,
) -> Result<Value> {
    ensure(safe_relative(output), "unsafe output path")?;
    let mut source = regular(root, relative)?;
    let target = destination.join(output);
    fs::create_dir_all(target.parent().ok_or("missing output parent")?)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut total = 0_u64;
    loop {
        common::check_interrupt()?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        file.write_all(&buffer[..count])?;
        total += count as u64;
    }
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))?;
    file.sync_all()?;
    Ok(
        json!({"path":output,"bytes":total,"sha256":format!("{:x}",hash.finalize()),"executable":executable}),
    )
}

pub(super) fn runtime(root: &Path, destination: &Path) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut files = vec![];
    let mut notices = vec![];
    for (source, name) in RUNTIME {
        let target = format!("third-party/rust-runtime/{name}");
        let file = copy(root, source, destination, &target, false).map_err(|error| format!("required Rust runtime notice unavailable ({source}); prepare the selected toolchain's rust-docs and rust-src components: {error}"))?;
        notices.push(json!({"sourcePath":source,"path":target,"sha256":file["sha256"]}));
        files.push(file);
    }
    Ok((files, notices))
}
