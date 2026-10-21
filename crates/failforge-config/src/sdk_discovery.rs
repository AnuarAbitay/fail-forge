//! Discover SDK layouts without executing PATH entries or installing toolchains.

use std::ffi::{CString, OsStr};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(crate) struct DiscoveredSdk {
    pub dart_sdk: PathBuf,
    pub flutter_sdk: Option<PathBuf>,
}

pub(crate) fn discover_path_sdk(search_path: &OsStr, cwd: &Path) -> Option<DiscoveredSdk> {
    for entry in std::env::split_paths(search_path) {
        let directory = if entry.is_absolute() {
            entry
        } else {
            cwd.join(entry)
        };
        // Resolving the directory separately retains a Flutter launcher root
        // when bin/dart itself links to an SDK outside the Flutter checkout.
        let launcher_root = fs::canonicalize(&directory)
            .ok()
            .and_then(|directory| directory.parent().map(Path::to_path_buf));
        for name in ["dart", "flutter"] {
            let executable = directory.join(name);
            if !is_executable(&executable) {
                continue;
            }
            // Resolve both binary and directory links before recognizing a
            // standard SDK or Flutter layout. Opaque shims are not executed.
            let Ok(executable) = fs::canonicalize(&executable) else {
                continue;
            };
            let Some(executable_root) = executable.parent().and_then(Path::parent) else {
                continue;
            };
            let candidates = [
                Some(executable_root.to_path_buf()),
                Some(executable_root.join("bin/cache/dart-sdk")),
                launcher_root
                    .as_ref()
                    .map(|root| root.join("bin/cache/dart-sdk")),
            ];
            for candidate in candidates.into_iter().flatten() {
                let Ok(candidate) = fs::canonicalize(candidate) else {
                    continue;
                };
                if is_dart_sdk(&candidate) {
                    let flutter_sdk = [
                        launcher_root.as_deref(),
                        Some(executable_root),
                        candidate.ancestors().nth(3),
                    ]
                    .into_iter()
                    .flatten()
                    .find(|root| owns_dart_sdk(root, &candidate))
                    .map(Path::to_path_buf);
                    return Some(DiscoveredSdk {
                        dart_sdk: candidate,
                        flutter_sdk,
                    });
                }
            }
        }
    }
    None
}

fn owns_dart_sdk(flutter_root: &Path, dart_sdk: &Path) -> bool {
    flutter_root.to_str().is_some()
        && is_executable(&flutter_root.join("bin/flutter"))
        && fs::canonicalize(flutter_root.join("bin/cache/dart-sdk"))
            .is_ok_and(|cached| cached == dart_sdk)
}

fn is_executable(path: &Path) -> bool {
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: name is a live, NUL-terminated path; access does not retain it.
    unsafe { libc::access(name.as_ptr(), libc::X_OK) == 0 }
}

fn is_dart_sdk(path: &Path) -> bool {
    // Effective configuration paths must be representable in JSON and YAML.
    path.to_str().is_some()
        && is_executable(&path.join("bin/dart"))
        && fs::metadata(path.join("lib/core/core.dart")).is_ok_and(|metadata| metadata.is_file())
}

#[cfg(test)]
mod tests;
