//! Parent-side preparation. No process-global cwd or environment is changed.
use crate::CommandSpec;
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) fn prepare(spec: &CommandSpec, invocation: &Path) -> io::Result<Command> {
    let cwd = std::fs::canonicalize(absolute(&spec.cwd, invocation))?;
    if !cwd.is_dir() {
        return Err(io::Error::from(io::ErrorKind::NotADirectory));
    }
    let mut env: BTreeMap<OsString, OsString> = if spec.env.clear {
        BTreeMap::new()
    } else {
        std::env::vars_os().collect()
    };
    for key in &spec.env.remove {
        env.remove(key);
    }
    env.extend(
        spec.env
            .set
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    let candidates = program_candidates(&spec.program, &cwd, env.get(OsStr::new("PATH")))?;
    let payload = ExecPayload::new(&candidates, &spec.program, &spec.args, &env)?;
    // The first candidate only gives std Command its required program field.
    // The pre_exec callback below performs the actual ordered PATH search.
    let mut command = Command::new(&candidates[0]);
    command
        .args(&spec.args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the callback uses only execve and errno after fork. All candidates,
    // C strings and null-terminated pointer arrays are prepared in the parent and kept
    // alive by the immutable payload. It performs no allocation or locking.
    // An explicit execve also prevents libc execvp's ENOEXEC shell fallback.
    unsafe {
        command.pre_exec(move || payload.execute());
    }
    Ok(command)
}

pub(crate) fn absolute(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn program_candidates(
    program: &OsStr,
    cwd: &Path,
    path: Option<&OsString>,
) -> io::Result<Vec<PathBuf>> {
    if program.as_encoded_bytes().contains(&b'/') {
        return Ok(vec![absolute(Path::new(program), cwd)]);
    }
    let path = path.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    // Permission bits cannot establish executable access for the effective
    // caller (UID/GID, ACLs, noexec mounts), and any access check would race exec.
    // Preserve every candidate instead: execve decides in the fork child.
    let candidates: Vec<_> = std::env::split_paths(path)
        .map(|entry| absolute(&entry, cwd).join(program))
        .collect();
    if candidates.is_empty() {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    Ok(candidates)
}

struct ExecPayload {
    programs: Vec<CString>,
    _argv_values: Vec<CString>,
    _env_values: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}

// SAFETY: these pointers reference only immutable CString allocations owned by
// this same value. Moving it cannot move those allocations. No pointer is
// dereferenced by Rust or mutated, and execve reads them only in the fork child.
unsafe impl Send for ExecPayload {}
// SAFETY: sharing the immutable payload cannot invalidate its owned buffers.
unsafe impl Sync for ExecPayload {}

impl ExecPayload {
    fn new(
        executables: &[PathBuf],
        argv0: &OsStr,
        args: &[OsString],
        env: &BTreeMap<OsString, OsString>,
    ) -> io::Result<Self> {
        let cstring = |value: &[u8]| {
            CString::new(value).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
        };
        if executables.is_empty() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let programs = executables
            .iter()
            .map(|executable| cstring(executable.as_os_str().as_encoded_bytes()))
            .collect::<io::Result<Vec<_>>>()?;
        let mut argv_values = vec![cstring(argv0.as_encoded_bytes())?];
        for arg in args {
            argv_values.push(cstring(arg.as_encoded_bytes())?);
        }
        let mut env_values = Vec::with_capacity(env.len());
        for (key, value) in env {
            let mut bytes = key.as_encoded_bytes().to_vec();
            bytes.push(b'=');
            bytes.extend_from_slice(value.as_encoded_bytes());
            env_values.push(cstring(&bytes)?);
        }
        let mut argv: Vec<_> = argv_values.iter().map(|value| value.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<_> = env_values.iter().map(|value| value.as_ptr()).collect();
        envp.push(std::ptr::null());
        Ok(Self {
            programs,
            _argv_values: argv_values,
            _env_values: env_values,
            argv,
            envp,
        })
    }
    fn execute(&self) -> io::Result<()> {
        let mut denied = false;
        let mut last_error = libc::ENOENT;
        for program in &self.programs {
            // SAFETY: program and every argv/envp element point to immutable,
            // live NUL-terminated CString buffers prepared before fork; both
            // arrays end with null. execve never returns on success and retains
            // no pointers on failure. No allocation, locks or formatting occur.
            unsafe {
                libc::execve(program.as_ptr(), self.argv.as_ptr(), self.envp.as_ptr());
            }
            let error = io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            match error {
                libc::EACCES => denied = true,
                libc::ENOENT | libc::ENOTDIR => {}
                // In particular, ENOEXEC is returned without invoking a shell
                // or silently running a later candidate with the same name.
                _ => return Err(io::Error::from_raw_os_error(error)),
            }
            last_error = error;
        }
        Err(io::Error::from_raw_os_error(if denied {
            libc::EACCES
        } else {
            last_error
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn absolute_paths_preserve_native_components() {
        assert_eq!(
            absolute(Path::new("./tool"), Path::new("/work")),
            Path::new("/work/./tool")
        );
        assert_eq!(
            absolute(Path::new("/tool"), Path::new("/work")),
            Path::new("/tool")
        );
    }
    #[test]
    fn explicit_program_path_does_not_require_path() {
        assert_eq!(
            program_candidates(OsStr::new("./tool"), Path::new("/work"), None).unwrap(),
            vec![PathBuf::from("/work/./tool")]
        );
        assert_eq!(
            program_candidates(OsStr::new("tool"), Path::new("/work"), None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
    #[test]
    fn c_arrays_preserve_bytes_and_are_terminated() {
        let env = [(
            OsString::from("MODE"),
            OsString::from_vec(b"a=b\xff".to_vec()),
        )]
        .into_iter()
        .collect();
        let programs = [
            PathBuf::from("/first/tool"),
            PathBuf::from(OsString::from_vec(b"/second-\xff/tool".to_vec())),
        ];
        let payload = ExecPayload::new(
            &programs,
            OsStr::new("tool"),
            &[OsString::new(), "a b".into()],
            &env,
        )
        .unwrap();
        assert_eq!(payload.programs[0].as_bytes(), b"/first/tool");
        assert_eq!(payload.programs[1].as_bytes(), b"/second-\xff/tool");
        assert_eq!(payload._argv_values[0].as_bytes(), b"tool");
        assert_eq!(payload._argv_values[1].as_bytes(), b"");
        assert_eq!(payload._argv_values[2].as_bytes(), b"a b");
        assert_eq!(payload._env_values[0].as_bytes(), b"MODE=a=b\xff");
        for (pointer, value) in payload.argv.iter().zip(&payload._argv_values) {
            assert_eq!(*pointer, value.as_ptr());
        }
        for (pointer, value) in payload.envp.iter().zip(&payload._env_values) {
            assert_eq!(*pointer, value.as_ptr());
        }
        assert!(payload.argv.last().unwrap().is_null());
        assert!(payload.envp.last().unwrap().is_null());
        assert!(
            ExecPayload::new(&programs, OsStr::new("tool"), &["bad\0arg".into()], &env).is_err()
        );
    }
    #[test]
    fn path_candidates_preserve_order_and_empty_relative_entries() {
        let path = OsString::from("first:/absolute::.");
        assert_eq!(
            program_candidates(OsStr::new("tool"), Path::new("/work"), Some(&path)).unwrap(),
            vec![
                PathBuf::from("/work/first/tool"),
                PathBuf::from("/absolute/tool"),
                PathBuf::from("/work/tool"),
                PathBuf::from("/work/./tool"),
            ]
        );
        let empty = OsString::new();
        assert_eq!(
            program_candidates(OsStr::new("tool"), Path::new("/work"), Some(&empty)).unwrap(),
            vec![PathBuf::from("/work/tool")]
        );
    }
    #[test]
    fn path_candidates_preserve_non_utf8_programs_and_entries() {
        let program = OsString::from_vec(b"tool-\xfe".to_vec());
        let path = OsString::from_vec(b"bin-\xff:/absolute-\xfd".to_vec());
        assert_eq!(
            program_candidates(&program, Path::new("/work"), Some(&path)).unwrap(),
            vec![
                PathBuf::from(OsString::from_vec(b"/work/bin-\xff/tool-\xfe".to_vec())),
                PathBuf::from(OsString::from_vec(b"/absolute-\xfd/tool-\xfe".to_vec())),
            ]
        );
    }
    #[test]
    fn candidates_are_not_filtered_by_existence_or_mode_bits() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Directory(std::env::temp_dir().join(format!(
            "failforge-path-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )));
        std::fs::create_dir(&directory.0).unwrap();
        std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::create_dir(directory.0.join("first")).unwrap();
        std::fs::create_dir(directory.0.join("second")).unwrap();
        let first = directory.0.join("first/tool");
        std::fs::write(&first, b"not executable for its owner\n").unwrap();
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o001)).unwrap();
        let second = directory.0.join("second/tool");
        std::fs::write(&second, b"second candidate\n").unwrap();
        std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = OsString::from("missing:first:second");
        assert_eq!(
            program_candidates(OsStr::new("tool"), &directory.0, Some(&path)).unwrap(),
            vec![directory.0.join("missing/tool"), first, second]
        );
    }
    #[test]
    fn payload_rejects_empty_candidate_list_and_nul_in_every_candidate() {
        let env = BTreeMap::new();
        assert!(ExecPayload::new(&[], OsStr::new("tool"), &[], &env).is_err());
        let programs = [PathBuf::from("/first/tool"), PathBuf::from("/bad\0/tool")];
        assert!(ExecPayload::new(&programs, OsStr::new("tool"), &[], &env).is_err());
    }
}
