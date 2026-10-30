//! Bounded filesystem checks shared by the read-only doctor diagnostic.
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use failforge_config::CampaignConfig;
use failforge_workspace::{PreparedProject, dependency_roots};

const MAX_TEST_ENTRIES: usize = 100_000;

pub(super) const ACCESS_READ: i32 = 4;
pub(super) const ACCESS_WRITE: i32 = 2;
pub(super) const ACCESS_EXECUTE: i32 = 1;

#[cfg(unix)]
pub(super) fn accessible(path: &Path, mode: i32) -> bool {
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: name is NUL-terminated and remains alive during access.
    unsafe { libc::access(name.as_ptr(), mode) == 0 }
}

#[cfg(windows)]
pub(super) fn accessible(path: &Path, mode: i32) -> bool {
    use failforge_workspace::windows_fs as win;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::*;
    let file = if path.is_dir() {
        win::directory(path)
    } else {
        win::regular(path, false)
    };
    let Ok(file) = file else {
        return false;
    };
    let mut access = FILE_READ_ATTRIBUTES;
    if mode & self::ACCESS_READ != 0 {
        access |= FILE_GENERIC_READ;
    }
    if mode & self::ACCESS_WRITE != 0 {
        access |= FILE_GENERIC_WRITE;
    }
    if mode & self::ACCESS_EXECUTE != 0 {
        access |= FILE_GENERIC_EXECUTE;
    }
    let Ok(path) = std::path::absolute(path) else {
        return false;
    };
    let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: OPEN_EXISTING checks requested ACL rights without writing,
    // truncating, creating or executing anything. All ancestor opens reject
    // reparse points; identity checks bind this access probe to the held entry.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let probe = unsafe { std::fs::File::from_raw_handle(handle) };
    let Ok(expected) = win::identity(&file) else {
        return false;
    };
    win::identity(&probe).is_ok_and(|actual| actual == expected)
        && (if path.is_dir() {
            win::directory(&path)
        } else {
            win::regular(&path, false)
        })
        .and_then(|named| win::identity(&named))
        .is_ok_and(|actual| actual == expected)
}
fn alias(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        metadata.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        failforge_workspace::windows_fs::is_reparse(metadata)
    }
}

pub(super) fn executable(path: &Path) -> bool {
    path.is_file() && accessible(path, ACCESS_EXECUTE)
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

/// Count the adapter's prepared packages, or name the file and failure.
pub(super) fn adapter_dependencies(adapter: &Path) -> Result<usize, String> {
    let pubspec_path = adapter.join("pubspec.yaml");
    let config = adapter.join(".dart_tool/package_config.json");
    let at = |path: &Path, cause: &dyn std::fmt::Display| {
        format!("{}: {cause}", failforge_workspace::display_path(path))
    };
    let raw = failforge_workspace::read_regular(&pubspec_path, failforge_config::MAX_YAML_BYTES)
        .map_err(|error| at(&pubspec_path, &error))?;
    let text = std::str::from_utf8(&raw).map_err(|_| at(&pubspec_path, &"not UTF-8"))?;
    let pubspec =
        failforge_config::yaml::parse_yaml(text).map_err(|error| at(&pubspec_path, &error))?;
    let own = pubspec
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| at(&pubspec_path, &"no package name"))?;
    let packages = dependency_roots(&config).map_err(|error| at(&config, &error))?;
    let root = adapter
        .canonicalize()
        .map_err(|error| at(adapter, &error))?;
    if packages.get(own) != Some(&root) {
        return Err(at(
            &config,
            &format!("{own} does not resolve to the adapter root"),
        ));
    }
    for key in ["dependencies", "dev_dependencies"] {
        if let Some(value) = pubspec.get(key).filter(|value| !value.is_null()) {
            let entries = value
                .as_object()
                .ok_or_else(|| at(&pubspec_path, &format!("{key} is not a mapping")))?;
            if let Some(name) = entries.keys().find(|name| !packages.contains_key(*name)) {
                return Err(at(&config, &format!("{key} entry {name} is not prepared")));
            }
        }
    }
    Ok(packages.len())
}

pub(super) fn conventional_tests(root: &Path) -> Result<bool, ()> {
    let test = root.join("test");
    match fs::symlink_metadata(&test) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Ok(metadata) if metadata.is_dir() && !alias(&metadata) => (),
        _ => return Err(()),
    }
    let mut pending = vec![test];
    let mut remaining = MAX_TEST_ENTRIES;
    let mut found = false;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).map_err(|_| ())? {
            remaining = remaining.checked_sub(1).ok_or(())?;
            let entry = entry.map_err(|_| ())?;
            let metadata = entry.metadata().map_err(|_| ())?;
            if alias(&metadata) {
                return Err(());
            }
            let kind = metadata.file_type();
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
            .filter(|candidate| {
                candidate
                    .join(failforge_config::platform::FLUTTER_BIN)
                    .is_file()
            })
            .map(Path::to_owned)
    })
}

pub(super) fn flutter_ready(sdk: &Path, dart: &Path) -> bool {
    let host = if cfg!(target_os = "macos") {
        "darwin-x64"
    } else if cfg!(windows) {
        "windows-x64"
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
        && executable(&sdk.join(failforge_config::platform::FLUTTER_BIN))
        && executable(
            &engine
                .join(host)
                .join(failforge_config::platform::FLUTTER_TESTER),
        )
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
    if starts(&config.project, &destination)
        || starts(&destination, &config.project)
            && !starts(&destination, &config.project.join(".failforge"))
    {
        return false;
    }
    let overlaps = |input: &Path| starts(&destination, input) || starts(input, &destination);
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
                if !metadata.is_dir()
                    || alias(&metadata)
                    || !accessible(ancestor, ACCESS_READ | ACCESS_EXECUTE) =>
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
                return metadata.is_dir() && accessible(nearest, ACCESS_WRITE | ACCESS_EXECUTE);
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

fn starts(path: &Path, root: &Path) -> bool {
    #[cfg(unix)]
    {
        path.starts_with(root)
    }
    #[cfg(windows)]
    {
        crate::campaign_workspace::path_starts_with(path, root)
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn native_read_only_access_checks_never_create_destinations() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-doctor-native-{}",
            failforge_workspace::windows_fs::random_token().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        assert!(accessible(&root, ACCESS_READ | ACCESS_EXECUTE));
        assert!(accessible(&root, ACCESS_WRITE | ACCESS_EXECUTE));
        assert!(!accessible(&root.join("missing"), ACCESS_WRITE));
        assert!(!root.join("missing").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
