//! Resolve native executables against the child's environment, never a shell.
use crate::CommandSpec;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING,
};

pub(crate) struct PreparedCommand {
    pub(crate) program: PathBuf,
    pub(crate) command_line: Vec<u16>,
    pub(crate) cwd: Vec<u16>,
    pub(crate) env: Vec<u16>,
    environment: Vec<(OsString, OsString)>,
    pub(crate) stdin: Option<File>,
    pub(crate) lease: Option<File>,
}

impl PreparedCommand {
    pub(crate) fn set_lease(&mut self, lease: &crate::WorkspaceLease) -> io::Result<()> {
        self.lease = Some(lease.file()?);
        Ok(())
    }
    pub(crate) fn set_lease_environment(&mut self, value: OsString) {
        set_env(
            &mut self.environment,
            crate::WORKSPACE_LEASE_ENV.into(),
            value,
        );
        self.env = serialize_env(&self.environment);
    }
}

pub(crate) fn set_stdin(command: &mut PreparedCommand, input: File) {
    command.stdin = Some(input);
}

pub(crate) fn prepare(spec: &CommandSpec, invocation: &Path) -> io::Result<PreparedCommand> {
    let cwd = std::fs::canonicalize(absolute(&spec.cwd, invocation))?;
    if !cwd.is_dir() {
        return Err(io::ErrorKind::NotADirectory.into());
    }
    let mut environment: Vec<(OsString, OsString)> = if spec.env.clear {
        Vec::new()
    } else {
        std::env::vars_os().collect()
    };
    for key in &spec.env.remove {
        environment.retain(|(existing, _)| !env_keys_equal(existing, key));
    }
    for (key, value) in &spec.env.set {
        set_env(&mut environment, key.clone(), value.clone());
    }
    let path = environment
        .iter()
        .find(|(key, _)| env_keys_equal(key, OsStr::new("PATH")))
        .map(|(_, value)| value);
    let canonical_program = resolve_program(&spec.program, &cwd, path)?;
    let program = launch_spelling(&canonical_program);
    let mut command_line = Vec::new();
    quote_argument(&mut command_line, program.as_os_str());
    for arg in &spec.args {
        command_line.push(b' ' as u16);
        quote_argument(&mut command_line, arg);
    }
    command_line.push(0);
    // CreateProcessW's limit includes the terminating NUL.
    if command_line.len() > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command line exceeds Windows limit",
        ));
    }
    let env = serialize_env(&environment);
    // Keep canonical paths for resolution above; runtimes also consume the
    // current-directory spelling returned by GetCurrentDirectoryW.
    let launch_cwd = launch_spelling(&cwd);
    Ok(PreparedCommand {
        program,
        command_line,
        cwd: wide(launch_cwd.as_os_str()),
        env,
        environment,
        stdin: None,
        lease: None,
    })
}

fn set_env(environment: &mut Vec<(OsString, OsString)>, key: OsString, value: OsString) {
    environment.retain(|(existing, _)| !env_keys_equal(existing, &key));
    environment.push((key, value));
}

fn serialize_env(environment: &[(OsString, OsString)]) -> Vec<u16> {
    let mut entries: Vec<_> = environment.iter().collect();
    entries.sort_by(|(left, _), (right, _)| compare_env_keys(left, right));
    let mut env = Vec::new();
    for (key, value) in entries {
        env.extend(key.encode_wide());
        env.push(b'=' as u16);
        env.extend(value.encode_wide());
        env.push(0);
    }
    if env.is_empty() {
        env.push(0);
    }
    env.push(0);
    env
}

pub(crate) fn env_keys_equal(left: &OsStr, right: &OsStr) -> bool {
    compare_env_keys(left, right) == std::cmp::Ordering::Equal
}

fn compare_env_keys(left: &OsStr, right: &OsStr) -> std::cmp::Ordering {
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CSTR_LESS_THAN, CompareStringOrdinal};
    let left: Vec<_> = left.encode_wide().collect();
    let right: Vec<_> = right.encode_wide().collect();
    // SAFETY: vectors stay live for this read-only ordinal, case-insensitive
    // comparison. Environment keys necessarily fit the native i32 length limit.
    match unsafe {
        CompareStringOrdinal(
            left.as_ptr(),
            left.len() as i32,
            right.as_ptr(),
            right.len() as i32,
            1,
        )
    } {
        CSTR_LESS_THAN => std::cmp::Ordering::Less,
        CSTR_EQUAL => std::cmp::Ordering::Equal,
        _ => std::cmp::Ordering::Greater,
    }
}

pub(crate) fn absolute(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

pub(crate) fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

fn resolve_program(program: &OsStr, cwd: &Path, path: Option<&OsString>) -> io::Result<PathBuf> {
    let program = Path::new(program);
    let explicit = program.components().count() > 1 || program.has_root();
    let candidates: Vec<_> = if explicit {
        vec![absolute(program, cwd)]
    } else {
        std::env::split_paths(path.ok_or(io::ErrorKind::NotFound)?)
            .map(|entry| absolute(&entry, cwd).join(program))
            .collect()
    };
    for mut candidate in candidates {
        if candidate.extension().is_none() {
            candidate.set_extension("exe");
        }
        // Batch files are not executables: CreateProcess and Rust's Command
        // otherwise have different shell fallback behavior. Require the user
        // to select an interpreter explicitly rather than interpret arguments.
        let extension = candidate.extension().and_then(OsStr::to_str).unwrap_or("");
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "batch scripts require an explicit interpreter",
            ));
        }
        match std::fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => return std::fs::canonicalize(candidate),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

/// Some Windows runtimes reject verbatim module or current-directory paths.
/// Use an ordinary spelling only when native Win32 lookup proves that it names
/// the same file or directory as the canonical resolution above.
fn launch_spelling(canonical: &Path) -> PathBuf {
    let Some(ordinary) = ordinary_spelling(canonical) else {
        return canonical.into();
    };
    match (file_identity(canonical), file_identity(&ordinary)) {
        (Ok(expected), Ok(actual)) if expected == actual => ordinary,
        _ => canonical.into(),
    }
}

fn ordinary_spelling(canonical: &Path) -> Option<PathBuf> {
    let has_trimmed_suffix = |name: &OsStr| matches!(name.encode_wide().last(), Some(0x2e | 0x20));
    let mut parts = canonical.components();
    let Component::Prefix(prefix) = parts.next()? else {
        return None;
    };
    let mut ordinary = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            if has_trimmed_suffix(server) || has_trimmed_suffix(share) {
                return None;
            }
            let mut name = OsString::from(r"\\");
            name.push(server);
            name.push(r"\");
            name.push(share);
            name.push(r"\");
            PathBuf::from(name)
        }
        _ => return None,
    };
    if parts.next()? != Component::RootDir {
        return None;
    }
    for part in parts {
        let Component::Normal(name) = part else {
            return None;
        };
        // Win32 can trim these suffixes after removing the verbatim prefix.
        // Retain the literal spelling even if an alias currently has the same ID.
        if has_trimmed_suffix(name) {
            return None;
        }
        ordinary.push(name);
    }
    (ordinary.as_os_str().encode_wide().count() < 260).then_some(ordinary)
}

fn file_identity(path: &Path) -> io::Result<(u64, [u8; 16])> {
    let name = wide(path.as_os_str());
    // SAFETY: direct CreateFileW intentionally observes literal Win32 pathname
    // semantics. Rust's File::open can add a verbatim prefix and conceal an
    // ordinary spelling that resolves to a different entry.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful CreateFileW transferred a unique owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut info = FILE_ID_INFO::default();
    // SAFETY: retained live handle and ABI-matching writable information record.
    if unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// Microsoft C runtime argv encoding, preserving UTF-16 and empty arguments.
fn quote_argument(output: &mut Vec<u16>, value: &OsStr) {
    output.push(b'"' as u16);
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, slashes));
        }
        slashes = 0;
        output.push(unit);
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_arguments_escape_quotes_and_trailing_slashes() {
        for (input, expected) in [
            ("", "\"\""),
            ("a b", "\"a b\""),
            ("a\"b", "\"a\\\"b\""),
            ("a\\", "\"a\\\\\""),
        ] {
            let mut output = Vec::new();
            quote_argument(&mut output, OsStr::new(input));
            assert_eq!(String::from_utf16(&output).unwrap(), expected);
        }
    }

    #[test]
    fn ordinary_launch_alias_keeps_sensitive_names_even_for_the_same_file() {
        let root = std::env::temp_dir().join(format!(
            "ff-launch-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let ordinary = root.join("program.exe");
        std::fs::write(&ordinary, b"ordinary file").unwrap();
        let ordinary_alias = ordinary_spelling(&ordinary).unwrap();
        for suffix in [".", " "] {
            let mut name = ordinary.as_os_str().to_os_string();
            name.push(suffix);
            let canonical = PathBuf::from(name);
            let mut alias = ordinary_alias.as_os_str().to_os_string();
            alias.push(suffix);
            let alias = PathBuf::from(alias);
            std::fs::write(&canonical, b"distinct verbatim file").unwrap();
            assert_ne!(
                file_identity(&canonical).unwrap(),
                file_identity(&alias).unwrap()
            );
            assert_eq!(launch_spelling(&canonical), canonical);
            std::fs::remove_file(&canonical).unwrap();

            std::fs::hard_link(&ordinary, &canonical).unwrap();
            assert_eq!(
                file_identity(&canonical).unwrap(),
                file_identity(&alias).unwrap()
            );
            assert_eq!(launch_spelling(&canonical), canonical);
            std::fs::remove_file(canonical).unwrap();
        }
        std::fs::remove_file(ordinary).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
