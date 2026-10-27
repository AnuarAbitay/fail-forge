//! Resolve native executables against the child's environment, never a shell.
use crate::CommandSpec;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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
    let program = resolve_program(&spec.program, &cwd, path)?;
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
    Ok(PreparedCommand {
        program,
        command_line,
        cwd: wide(cwd.as_os_str()),
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
}
