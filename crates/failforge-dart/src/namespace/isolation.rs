use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use failforge_workspace::{Cancellation, dependency_roots_from_value, parse_json};
use serde_json::{Value, json};

use super::fs::{BoundFile, Inventory, Root};
use super::plan::{Edit, runtime_edits};
use super::scanner::{MAX_ITEMS, Scanner, check_uris};
use super::{
    MAX_EDITS, MAX_FILE_BYTES, MAX_FILES, MAX_SOURCE_BYTES, NamespaceError, invalid, require,
};

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Preflight,
    BeforeWrite(usize),
    AfterWrite(usize),
    TemporaryReady,
    Renamed,
}
#[cfg(test)]
type Hook = Box<dyn FnMut(Stage) -> Result<(), NamespaceError>>;
#[cfg(test)]
thread_local! {
    pub(super) static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(super) fn stage(value: Stage) -> Result<(), NamespaceError> {
    HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(hook) => hook(value),
        None => Ok(()),
    })
}

struct Write {
    target: BoundFile,
    original: Vec<u8>,
    replacement: Vec<u8>,
}

fn same_inventory(
    root: &Root,
    prefix: &str,
    expected: &Inventory,
    owned: bool,
    cancel: &Cancellation,
) -> Result<(), NamespaceError> {
    require(
        root.inventory(prefix, owned, cancel)? == *expected,
        "namespace inputs changed before instrumentation",
    )
}

fn canonical_alias(alias: &str) -> bool {
    alias.len() <= 100
        && alias.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn bind_config(
    original: &Root,
    copied: &Root,
    own: &str,
    original_config: &Value,
    copied_config: &Value,
) -> Result<(), NamespaceError> {
    let relative = ".dart_tool/package_config.json";
    let original_roots =
        dependency_roots_from_value(&original.path.join(relative), original_config)?;
    let copied_roots = dependency_roots_from_value(&copied.path.join(relative), copied_config)?;
    require(
        original_roots.get(own) == Some(&original.path)
            && copied_roots.get(own) == Some(&copied.path),
        "namespace package root does not identify the verified subject",
    )?;
    for (root, config) in [(original, original_config), (copied, copied_config)] {
        let row = config["packages"]
            .as_array()
            .expect("validated packages")
            .iter()
            .find(|row| row["name"] == own)
            .expect("validated subject");
        let package_uri = row.get("packageUri").and_then(Value::as_str).unwrap_or("");
        let library = dependency_roots_from_value(
            &root.path.join("package_config.json"),
            &json!({"configVersion":2,"packages":[{"name":own,"rootUri":package_uri}]}),
        )
        .map_err(|_| {
            invalid("namespace subject packageUri must identify the verified lib directory")
        })?;
        require(
            library.get(own) == Some(&root.path.join("lib")),
            "namespace subject packageUri must identify the verified lib directory",
        )?;
    }
    // A fresh snapshot legitimately rebases rootUri. Every other field must
    // retain its original semantics, particularly packageUri/languageVersion:
    // these decide which code the proven imports actually load.
    let normalize = |config: &Value, roots: &BTreeMap<String, PathBuf>| {
        let mut normalized = config.clone();
        for row in normalized["packages"]
            .as_array_mut()
            .expect("validated packages")
        {
            let name = row["name"].as_str().expect("validated package name");
            row["rootUri"] = if name == own {
                Value::Null
            } else {
                json!(roots.get(name).expect("validated dependency root"))
            };
        }
        normalized
    };
    require(
        normalize(original_config, &original_roots) == normalize(copied_config, &copied_roots),
        "namespace copied package configuration differs from original",
    )
}

fn file_uri(path: &Path) -> Result<String, NamespaceError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("namespace path must be UTF-8"))?;
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            uri.push(char::from(byte));
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push('/');
    Ok(uri)
}

fn test_bin(uri: &str) -> Result<PathBuf, NamespaceError> {
    let scheme = uri
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("file:"));
    require(
        scheme.is_some(),
        "test dependency is not a local prepared package",
    )?;
    let mut path = &uri[5..];
    if let Some(authority) = path.strip_prefix("//") {
        let (host, tail) = authority
            .split_once('/')
            .ok_or_else(|| invalid("test dependency is not a local prepared package"))?;
        require(
            host.is_empty() || host == "localhost",
            "test dependency is not a local prepared package",
        )?;
        path = &authority[host.len()..];
        let _ = tail;
    }
    path = path.split(['?', '#']).next().unwrap_or_default();
    let mut bytes = vec![];
    let mut index = 0;
    while index < path.len() {
        if path.as_bytes()[index] == b'%' && index + 2 < path.len() {
            let hex = |byte: u8| char::from(byte).to_digit(16);
            if let (Some(left), Some(right)) = (
                hex(path.as_bytes()[index + 1]),
                hex(path.as_bytes()[index + 2]),
            ) {
                bytes.push((left * 16 + right) as u8);
                index += 3;
                continue;
            }
        }
        bytes.push(path.as_bytes()[index]);
        index += 1;
    }
    let text =
        String::from_utf8(bytes).map_err(|_| invalid("test dependency URI must be UTF-8"))?;
    require(
        text.starts_with('/') && !text.contains('\0'),
        "test dependency is not a local prepared package",
    )?;
    let bin = PathBuf::from(text).join("bin/test.dart");
    require(bin.is_file(), "prepared test entry point missing")?;
    Ok(bin)
}

fn rewritten(
    raw: &[u8],
    edits: &[Edit],
    own: &[u8],
    alias: &[u8],
) -> Result<Vec<u8>, NamespaceError> {
    let mut changed = vec![];
    let mut positions = vec![];
    let mut cursor = 0;
    for edit in edits {
        let start = edit.offset_bytes;
        let end = start
            .checked_add(edit.length_bytes)
            .ok_or_else(|| invalid("overlapping namespace edits"))?;
        require(
            start >= cursor && raw.get(start..end) == Some(own),
            "overlapping namespace edits",
        )?;
        changed.extend_from_slice(&raw[cursor..start]);
        positions.push(changed.len());
        changed.extend_from_slice(alias);
        require(
            changed.len() <= MAX_FILE_BYTES,
            "rewritten namespace source exceeds byte limit",
        )?;
        cursor = end;
    }
    changed.extend_from_slice(&raw[cursor..]);
    require(
        changed.len() <= MAX_FILE_BYTES,
        "rewritten namespace source exceeds byte limit",
    )?;
    // Prove inversion in one pass, avoiding quadratic splice copying for a
    // source with many edits. Each non-prefix range must remain byte-identical.
    let mut restored = vec![];
    let mut cursor = 0;
    for position in positions {
        let end = position + alias.len();
        require(
            changed.get(position..end) == Some(alias),
            "test namespace inverse range differs",
        )?;
        restored.extend_from_slice(&changed[cursor..position]);
        restored.extend_from_slice(own);
        cursor = end;
    }
    restored.extend_from_slice(&changed[cursor..]);
    require(restored == raw, "test namespace inverse proof failed")?;
    Ok(changed)
}

fn commit(
    copy: &Root,
    writes: &mut [Write],
    cancel: &Cancellation,
    verify: impl Fn() -> Result<(), NamespaceError>,
) -> Result<(), NamespaceError> {
    for write in &*writes {
        write.target.verify(copy)?;
    }
    let mut applied = vec![];
    let result = (|| {
        for (index, write) in writes.iter_mut().enumerate() {
            cancel.check()?;
            #[cfg(test)]
            stage(Stage::BeforeWrite(index))?;
            let previous = write.target.generation();
            let result = write.target.replace(copy, &write.replacement);
            if write.target.generation() != previous {
                applied.push(index);
            }
            result?;
            #[cfg(test)]
            stage(Stage::AfterWrite(index))?;
        }
        verify()?;
        cancel.check()?;
        Ok(())
    })();
    if let Err(primary) = result {
        let mut rollback_errors = vec![];
        for index in applied.into_iter().rev() {
            let write = &mut writes[index];
            if let Err(error) = write
                .target
                .restore(copy, &write.replacement, &write.original)
            {
                rollback_errors.push(error.to_string());
            }
        }
        if rollback_errors.is_empty() {
            return Err(primary);
        }
        return Err(invalid(format!(
            "namespace rollback failed after {primary}: {}",
            rollback_errors.join("; ")
        )));
    }
    Ok(())
}

/// Rebind a fresh copy while leaving both upstream package trees untouched.
/// Only exact header/runtime package-prefix ranges are eligible. A missing
/// runtime plan is reported before any write so the caller can invoke Dart's
/// resolved analyzer once and retry using the same verified original sources.
pub fn isolate_subject(
    project: &Path,
    copy: &Path,
    framework: &Path,
    alias: &str,
    plan: Option<&Value>,
    cancel: &Cancellation,
) -> Result<Value, NamespaceError> {
    cancel.check()?;
    let original = Root::open(project)?;
    let copied = Root::open(copy)?;
    let upstream = Root::open(framework)?;
    for other in [&original.path, &upstream.path] {
        require(
            !copied.path.starts_with(other) && !other.starts_with(&copied.path),
            "namespace copy overlaps an original input",
        )?;
    }
    let (config_raw, config_target) =
        copied.read(".dart_tool/package_config.json", MAX_FILE_BYTES, true)?;
    let mut config = parse_json(&config_raw)?;
    let (graph_raw, _) = copied.read(".dart_tool/package_graph.json", MAX_FILE_BYTES, true)?;
    let graph = parse_json(&graph_raw)?;
    let roots = graph["roots"]
        .as_array()
        .filter(|roots| roots.len() == 1)
        .ok_or_else(|| invalid("ambiguous project roots"))?;
    let own = roots[0]
        .as_str()
        .ok_or_else(|| invalid("ambiguous project roots"))?
        .to_owned();
    let (original_graph, _) =
        original.read(".dart_tool/package_graph.json", MAX_FILE_BYTES, false)?;
    require(
        parse_json(&original_graph)? == graph,
        "namespace copied package graph differs from original",
    )?;
    let (original_config, _) =
        original.read(".dart_tool/package_config.json", MAX_FILE_BYTES, false)?;
    bind_config(
        &original,
        &copied,
        &own,
        &parse_json(&original_config)?,
        &config,
    )?;
    require(
        canonical_alias(alias) && alias != own,
        "invalid subject alias",
    )?;
    let packages = config["packages"]
        .as_array()
        .ok_or_else(|| invalid("ambiguous project configuration"))?;
    require(
        !packages.iter().any(|entry| entry["name"] == alias),
        "subject alias collides with a dependency",
    )?;
    let selected: Vec<_> = packages
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry["name"] == own)
        .collect();
    require(selected.len() == 1, "ambiguous project configuration")?;
    let own_index = selected[0].0;
    require(selected[0].1.is_object(), "ambiguous project configuration")?;
    let tests: Vec<_> = packages
        .iter()
        .filter(|entry| entry["name"] == "test")
        .collect();
    require(tests.len() == 1, "ambiguous test package")?;
    let test_bin = test_bin(
        tests[0]["rootUri"]
            .as_str()
            .ok_or_else(|| invalid("test dependency is not a local prepared package"))?,
    )?;
    let original_lib = original.inventory("lib", false, cancel)?;
    let framework_lib = upstream.inventory("lib", false, cancel)?;
    require(
        !original_lib.is_empty() && original_lib == framework_lib,
        "framework copy is not the exact upstream library",
    )?;
    require(
        copied.inventory("lib", true, cancel)? == original_lib,
        "copied subject library differs from original",
    )?;
    let original_tests = original.inventory("test", false, cancel)?;
    require(
        copied.inventory("test", true, cancel)? == original_tests,
        "author tests changed before instrumentation",
    )?;
    let own_prefix = format!("package:{own}/");
    let alias_prefix = format!("package:{alias}/");
    let mut source_bytes = 0;
    let mut source_items = 0;
    for name in original_lib.keys().filter(|name| name.ends_with(".dart")) {
        cancel.check()?;
        let (raw, _) = original.read(&format!("lib/{name}"), MAX_FILE_BYTES, false)?;
        source_bytes += raw.len();
        require(
            source_bytes <= MAX_SOURCE_BYTES,
            "namespace source inventory exceeds 64 MiB",
        )?;
        let scanner = Scanner::new(&raw, cancel)?;
        source_items += scanner.item_count();
        require(
            source_items <= MAX_ITEMS,
            "namespace aggregate lexical item budget exceeded",
        )?;
        match check_uris(
            &scanner,
            own_prefix.as_bytes(),
            alias_prefix.as_bytes(),
            &BTreeSet::new(),
            cancel,
        ) {
            Err(error @ NamespaceError::Cancelled(_)) => return Err(error),
            Err(_) => {
                return Err(invalid(
                    "subject library has self package references or unsupported URI",
                ));
            }
            Ok(()) => {}
        }
    }
    let mut sources = BTreeMap::new();
    for name in original_tests.keys().filter(|name| name.ends_with(".dart")) {
        cancel.check()?;
        require(
            sources.len() < MAX_FILES,
            "namespace planning requires 1..2000 author Dart files",
        )?;
        let name = format!("test/{name}");
        let (raw, _) = original.read(&name, MAX_FILE_BYTES, false)?;
        source_bytes += raw.len();
        require(
            source_bytes <= MAX_SOURCE_BYTES,
            "namespace source inventory exceeds 64 MiB",
        )?;
        let scanner = Scanner::new(&raw, cancel)?;
        source_items += scanner.item_count();
        require(
            source_items <= MAX_ITEMS,
            "namespace aggregate lexical item budget exceeded",
        )?;
        sources.insert(name, scanner);
    }
    if plan.is_none()
        && sources.values().any(|scanner| {
            scanner
                .code_identifiers()
                .contains(b"resolvePackageUri".as_slice())
                || scanner
                    .code_identifiers()
                    .contains(b"resolvePackageUriSync".as_slice())
        })
    {
        return Err(NamespaceError::PlanRequired);
    }
    let runtime = runtime_edits(
        &own,
        alias,
        &crate::sha256_hex(&original_config),
        &sources,
        plan,
        cancel,
    )?;
    let mut writes = vec![];
    let mut proof = vec![];
    let mut edit_count = 0;
    let mut expected_tests = original_tests.clone();
    for (name, scanner) in &sources {
        cancel.check()?;
        let headers: Vec<_> = scanner
            .header_uris(cancel)?
            .into_iter()
            .filter(|token| scanner.value(token).starts_with(own_prefix.as_bytes()))
            .map(|token| Edit {
                kind: "header_package_uri".into(),
                offset_bytes: token.content_start,
                length_bytes: own_prefix.len(),
                expected: own_prefix.clone(),
                replacement: alias_prefix.clone(),
            })
            .collect();
        let runtime = &runtime[name];
        let mut edits = headers.clone();
        edits.extend_from_slice(runtime);
        edits.sort_by_key(|edit| edit.offset_bytes);
        edit_count += edits.len();
        require(
            edit_count <= MAX_EDITS,
            "namespace plan edit budget exceeded",
        )?;
        let allowed = edits.iter().map(|edit| edit.offset_bytes).collect();
        check_uris(
            scanner,
            own_prefix.as_bytes(),
            alias_prefix.as_bytes(),
            &allowed,
            cancel,
        )?;
        let raw = scanner.raw();
        let (copy_raw, target) = copied.read(name, MAX_FILE_BYTES, true)?;
        require(
            copy_raw == raw,
            "author tests changed before instrumentation",
        )?;
        let changed = rewritten(raw, &edits, own_prefix.as_bytes(), alias_prefix.as_bytes())?;
        if !edits.is_empty() {
            let changed_hash = crate::sha256_hex(&changed);
            expected_tests.insert(
                name.strip_prefix("test/")
                    .expect("generated test path")
                    .into(),
                changed_hash.clone(),
            );
            proof.push(json!({"file":name,"importDirectives":headers.len(),"runtimePackageUris":runtime.len(),"upstreamSha256":crate::sha256_hex(raw),"rewrittenSha256":changed_hash,"restoredSha256":crate::sha256_hex(raw),"assertionsUnchanged":runtime.is_empty(),"nonNamespaceBytesUnchanged":true,"edits":edits}));
            writes.push(Write {
                target,
                original: copy_raw,
                replacement: changed,
            });
        }
    }
    require(
        !proof.is_empty(),
        "no author test imports or runtime URIs were isolated",
    )?;
    let mut subject = config["packages"][own_index].clone();
    subject["name"] = alias.into();
    subject["rootUri"] = "../".into();
    config["packages"][own_index]["rootUri"] = file_uri(&upstream.path)?.into();
    config["packages"]
        .as_array_mut()
        .expect("validated package array")
        .push(subject);
    let changed_config = serde_json::to_vec(&config)
        .map_err(|_| invalid("cannot serialize namespace package configuration"))?;
    require(
        changed_config.len() <= MAX_FILE_BYTES,
        "namespace package configuration exceeds its byte limit",
    )?;
    writes.push(Write {
        target: config_target,
        original: config_raw,
        replacement: changed_config.clone(),
    });
    let verify_inputs = |token: &Cancellation| {
        same_inventory(&original, "lib", &original_lib, false, token)?;
        same_inventory(&upstream, "lib", &framework_lib, false, token)?;
        same_inventory(&original, "test", &original_tests, false, token)?;
        require(
            original
                .read(".dart_tool/package_config.json", MAX_FILE_BYTES, false)?
                .0
                == original_config,
            "namespace plan package configuration changed",
        )?;
        require(
            original
                .read(".dart_tool/package_graph.json", MAX_FILE_BYTES, false)?
                .0
                == original_graph,
            "namespace original package graph changed",
        )?;
        require(
            copied
                .read(".dart_tool/package_graph.json", MAX_FILE_BYTES, true)?
                .0
                == graph_raw,
            "namespace package graph changed",
        )?;
        same_inventory(&copied, "lib", &original_lib, true, token)
    };
    #[cfg(test)]
    stage(Stage::Preflight)?;
    verify_inputs(cancel)?;
    same_inventory(&copied, "test", &original_tests, true, cancel)?;
    commit(&copied, &mut writes, cancel, || {
        // Integrity checks take precedence over a concurrent cancellation. They
        // complete before deciding whether to roll back the prepared copy.
        let verify = Cancellation::default();
        verify_inputs(&verify)?;
        same_inventory(&copied, "test", &expected_tests, true, &verify)?;
        require(
            copied
                .read(".dart_tool/package_config.json", MAX_FILE_BYTES, true)?
                .0
                == changed_config,
            "namespace package configuration changed",
        )
    })?;
    Ok(
        json!({"alias":alias,"frameworkRoot":framework,"frameworkLibraryHashes":framework_lib,"importProof":proof,"testBin":test_bin}),
    )
}
