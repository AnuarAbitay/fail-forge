//! Path normalization and bounded, nonblocking reads of configuration files.

use std::collections::{HashSet, VecDeque};
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::{ConfigError, MAX_YAML_BYTES};
use serde_json::Value;

pub fn resolve_path(value: &Value, name: &str, base: &Path) -> Result<PathBuf, ConfigError> {
    let text = value
        .as_str()
        .ok_or_else(|| ConfigError::new(format!("{name} must be a nonempty path string")))?;
    if text.is_empty() || text.contains('\0') {
        return Err(ConfigError::new(format!(
            "{name} must be a nonempty path without NUL"
        )));
    }
    resolve_native_path(Path::new(text), name, base)
}

pub fn resolve_native_path(value: &Path, name: &str, base: &Path) -> Result<PathBuf, ConfigError> {
    let text = value
        .to_str()
        .ok_or_else(|| ConfigError::new(format!("{name} must contain valid Unicode")))?;
    if text.is_empty() || text.contains('\0') {
        return Err(ConfigError::new(format!(
            "{name} must be a nonempty path without NUL"
        )));
    }
    let expanded = if let Some(rest) = text.strip_prefix('~') {
        let (user, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        let home = home_directory(user)
            .ok_or_else(|| ConfigError::new(format!("{name} cannot be resolved")))?;
        home.join(suffix.trim_start_matches('/'))
    } else {
        value.to_path_buf()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    canonicalize_missing(&absolute)
        .map_err(|_| ConfigError::new(format!("{name} cannot be resolved")))
}

fn home_directory(user: &str) -> Option<PathBuf> {
    if user.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            return Some(if home.is_empty() {
                PathBuf::from("/")
            } else {
                PathBuf::from(home)
            });
        }
    }
    let username = CString::new(user).ok()?;
    let mut capacity = 16_384;
    loop {
        let mut buffer = vec![0u8; capacity];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all output pointers remain valid for the entire libc call;
        // passwd string pointers are copied before the backing buffer is freed.
        let code = unsafe {
            if user.is_empty() {
                libc::getpwuid_r(
                    libc::getuid(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            } else {
                libc::getpwnam_r(
                    username.as_ptr(),
                    entry.as_mut_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    &mut result,
                )
            }
        };
        if code == libc::ERANGE && capacity < 1_048_576 {
            capacity *= 2;
            continue;
        }
        if code != 0 || result.is_null() {
            return None;
        }
        // SAFETY: successful getpw*_r initialized entry and its NUL-terminated
        // directory pointer refers to our still-live buffer.
        let directory = unsafe { (*result).pw_dir };
        if directory.is_null() {
            return None;
        }
        let home = unsafe { CStr::from_ptr(directory) }.to_str().ok()?;
        return Some(PathBuf::from(home));
    }
}

fn canonicalize_missing(path: &Path) -> io::Result<PathBuf> {
    enum Part {
        Component(OsString),
        EndLink(PathBuf),
    }
    let mut pending = path
        .components()
        .map(|component| Part::Component(component.as_os_str().to_owned()))
        .collect::<VecDeque<_>>();
    let mut result = PathBuf::new();
    let mut active_links = HashSet::new();
    while let Some(part) = pending.pop_front() {
        let component = match part {
            Part::Component(value) => value,
            Part::EndLink(path) => {
                active_links.remove(&path);
                continue;
            }
        };
        if component == "/" {
            result = PathBuf::from("/");
            continue;
        }
        if component == "." {
            continue;
        }
        if component == ".." {
            result.pop();
            continue;
        }
        result.push(&component);
        match fs::symlink_metadata(&result) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = result.clone();
                if !active_links.insert(link.clone()) {
                    return Err(io::Error::from_raw_os_error(libc::ELOOP));
                }
                let target = fs::read_link(&result)?;
                result.pop();
                pending.push_front(Part::EndLink(link));
                for part in target.components().rev() {
                    pending.push_front(Part::Component(part.as_os_str().to_owned()));
                }
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error),
        }
    }
    if result.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "non-Unicode resolved path",
        ));
    }
    Ok(result)
}

pub fn read_regular_bytes(path: &Path, limit: usize, follow_symlinks: bool) -> io::Result<Vec<u8>> {
    let mut flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
    if !follow_symlinks {
        flags |= libc::O_NOFOLLOW;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input is not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn profile_project(directory: &Path) -> Result<PathBuf, ConfigError> {
    let mut current = directory;
    loop {
        let candidate = current.join("pubspec.yaml");
        match read_regular_bytes(&candidate, MAX_YAML_BYTES, false) {
            Ok(raw) if raw.len() > MAX_YAML_BYTES => {
                return Err(ConfigError::new(format!(
                    "project pubspec.yaml exceeds 1 MiB: {}",
                    candidate.display()
                )));
            }
            Ok(_) => return Ok(current.to_path_buf()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(ConfigError::new(format!(
                    "project pubspec.yaml must be a readable regular file: {}",
                    candidate.display()
                )));
            }
        }
        current = current.parent().ok_or_else(|| {
            ConfigError::new(format!(
                "cannot find project: no ancestor pubspec.yaml for {}",
                directory.display()
            ))
        })?;
    }
}
