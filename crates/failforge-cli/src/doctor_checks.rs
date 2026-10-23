//! Bounded filesystem checks shared by the read-only doctor diagnostic.
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use failforge_config::CampaignConfig;
use failforge_workspace::{PreparedProject, dependency_roots};

const MAX_TEST_ENTRIES: usize = 100_000;

pub(super) fn accessible(path: &Path, mode: i32) -> bool {
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: name is NUL-terminated and remains alive during access.
    unsafe { libc::access(name.as_ptr(), mode) == 0 }
}

pub(super) fn executable(path: &Path) -> bool {
    path.is_file() && accessible(path, libc::X_OK)
}

pub(super) fn readable_regular(path: &Path) -> bool {
    // The shared reader rejects final symlinks and nonregular files without
    // blocking on FIFOs. A one-byte prefix is enough to establish readability.
    failforge_workspace::read_regular_prefix(path, 0).is_ok()
}

pub(super) fn supported_version(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let parts = text.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || part.len() > 1 && part.starts_with('0')
                || !part.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return None;
    }
    let numbers = parts
        .iter()
        .map(|part| part.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    (numbers[0] == 3 && numbers[1] >= 13).then(|| text.to_owned())
}

pub(super) fn adapter_dependencies(adapter: &Path) -> Result<usize, ()> {
    let raw = failforge_workspace::read_regular(
        &adapter.join("pubspec.yaml"),
        failforge_config::MAX_YAML_BYTES,
    )
    .map_err(|_| ())?;
    let text = std::str::from_utf8(&raw).map_err(|_| ())?;
    let pubspec = failforge_config::yaml::parse_yaml(text).map_err(|_| ())?;
    let own = pubspec
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or(())?;
    let packages =
        dependency_roots(&adapter.join(".dart_tool/package_config.json")).map_err(|_| ())?;
    if packages.get(own) != Some(&adapter.canonicalize().map_err(|_| ())?) {
        return Err(());
    }
    for key in ["dependencies", "dev_dependencies"] {
        if let Some(value) = pubspec.get(key).filter(|value| !value.is_null()) {
            let entries = value.as_object().ok_or(())?;
            if entries.keys().any(|name| !packages.contains_key(name)) {
                return Err(());
            }
        }
    }
    Ok(packages.len())
}

pub(super) fn conventional_tests(root: &Path) -> Result<bool, ()> {
    let test = root.join("test");
    match fs::symlink_metadata(&test) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Ok(metadata) if metadata.is_dir() => (),
        _ => return Err(()),
    }
    let mut pending = vec![test];
    let mut remaining = MAX_TEST_ENTRIES;
    let mut found = false;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|_| ())? {
            remaining = remaining.checked_sub(1).ok_or(())?;
            let entry = entry.map_err(|_| ())?;
            let kind = entry.file_type().map_err(|_| ())?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if !kind.is_file() {
                return Err(());
            } else if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with("_test.dart"))
            {
                if !readable_regular(&entry.path()) {
                    return Err(());
                }
                found = true;
            }
        }
    }
    Ok(found)
}

pub(super) fn flutter_sdk(config: &CampaignConfig) -> Option<PathBuf> {
    config.flutter_sdk.clone().or_else(|| {
        config
            .dart_sdk
            .ancestors()
            .nth(3)
            .filter(|candidate| candidate.join("bin/flutter").is_file())
            .map(Path::to_owned)
    })
}

pub(super) fn flutter_ready(sdk: &Path, dart: &Path) -> bool {
    let host = if cfg!(target_os = "macos") {
        "darwin-x64"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    };
    let engine = sdk.join("bin/cache/artifacts/engine");
    sdk.join("bin/cache/dart-sdk")
        .canonicalize()
        .ok()
        .zip(dart.canonicalize().ok())
        .is_some_and(|(bundled, selected)| bundled == selected)
        && sdk.join("packages/flutter").is_dir()
        && executable(&sdk.join("bin/flutter"))
        && executable(&engine.join(host).join("flutter_tester"))
        && [
            sdk.join("bin/cache/flutter_tools.snapshot"),
            engine.join(host).join("icudtl.dat"),
            engine.join("common/flutter_patched_sdk/platform_strong.dill"),
            dart.join("bin/snapshots/frontend_server_aot.dart.snapshot"),
            sdk.join("packages/flutter/pubspec.yaml"),
            sdk.join("packages/flutter/lib/src/services/platform_channel.dart"),
        ]
        .iter()
        .all(|path| readable_regular(path))
}

pub(super) fn report_destination(
    config: &CampaignConfig,
    original: Option<&PreparedProject>,
    helper: &Path,
) -> bool {
    let automatic = config.output.is_none();
    let destination = config
        .output
        .clone()
        .unwrap_or_else(|| config.project.join(".failforge/runs"));
    if !automatic && fs::symlink_metadata(&destination).is_ok() {
        return false;
    }
    if config.project.starts_with(&destination)
        || destination.starts_with(&config.project)
            && !destination.starts_with(config.project.join(".failforge"))
    {
        return false;
    }
    let overlaps = |input: &Path| destination.starts_with(input) || input.starts_with(&destination);
    if [&config.dart_sdk, &config.adapter_root, helper]
        .iter()
        .any(|input| overlaps(input))
        || config
            .flutter_sdk
            .clone()
            .or_else(|| {
                original
                    .filter(|project| project.flutter)
                    .and_then(|_| flutter_sdk(config))
            })
            .as_ref()
            .is_some_and(|input| overlaps(input))
        || original.is_some_and(|project| {
            project
                .packages
                .values()
                .filter(|path| **path != project.root)
                .any(|path| overlaps(path))
        })
    {
        return false;
    }
    for ancestor in destination.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata)
                if !metadata.is_dir() || !accessible(ancestor, libc::R_OK | libc::X_OK) =>
            {
                return false;
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return false,
            _ => (),
        }
    }
    let mut nearest = destination.as_path();
    loop {
        match fs::symlink_metadata(nearest) {
            Ok(metadata) => {
                return metadata.is_dir() && accessible(nearest, libc::W_OK | libc::X_OK);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let Some(parent) = nearest.parent() else {
                    return false;
                };
                nearest = parent;
            }
            _ => return false,
        }
    }
}
