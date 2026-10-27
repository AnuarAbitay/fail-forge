#[cfg(unix)]
use std::ffi::CString;
use std::io::Read;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;

use crate::{Mutant, ProtocolError, sha256_hex};

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("mutation target must be a canonical relative package path")]
    InvalidPath,
    #[error("mutation source root must be a real directory")]
    InvalidRoot,
    #[error("mutation target contains a symlink")]
    Symlink,
    #[error("mutation target leaves the package root")]
    OutsideRoot,
    #[error("mutation target must be a regular file")]
    NotFile,
    #[error("mutation source exceeds its byte limit")]
    SourceTooLarge,
    #[error("mutation source is not UTF-8")]
    InvalidUtf8,
    #[error("mutation source hash changed")]
    HashMismatch,
    #[error("mutation byte range is invalid")]
    InvalidRange,
    #[error("mutation range splits a UTF-8 code point")]
    Utf8Boundary,
    #[error("mutation expected bytes do not match the exact range")]
    ExpectedMismatch,
    #[error("mutation replacement does not change the source")]
    NoChange,
    #[error("mutation output size cannot be represented")]
    OutputTooLarge,
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("mutation source I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

pub fn validate_relative_target(file: &str) -> Result<(), PatchError> {
    if file.is_empty()
        || file.contains(['\\', '\0'])
        || file.starts_with('/')
        || file
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || (file.as_bytes().get(1) == Some(&b':') && file.as_bytes()[0].is_ascii_alphabetic())
    {
        return Err(PatchError::InvalidPath);
    }
    #[cfg(windows)]
    for part in file.split('/') {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if part
            .chars()
            .any(|character| character < ' ' || "<>:\"|?*".contains(character))
            || part.ends_with(['.', ' '])
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || stem
                .strip_prefix("COM")
                .or_else(|| stem.strip_prefix("LPT"))
                .is_some_and(|suffix| {
                    suffix.chars().count() == 1
                        && suffix
                            .chars()
                            .all(|digit| matches!(digit, '1'..='9' | '¹' | '²' | '³'))
                })
        {
            return Err(PatchError::InvalidPath);
        }
    }
    Ok(())
}

/// Checks applicability only. Static validity and test execution remain separate.
pub fn apply_mutant(source: &[u8], mutant: &Mutant) -> Result<Vec<u8>, PatchError> {
    validate_relative_target(&mutant.file)?;
    mutant.validate_model()?;
    let text = std::str::from_utf8(source).map_err(|_| PatchError::InvalidUtf8)?;
    if sha256_hex(source) != mutant.source_hash {
        return Err(PatchError::HashMismatch);
    }
    let start = usize::try_from(mutant.edit.offset_bytes).map_err(|_| PatchError::InvalidRange)?;
    let length = usize::try_from(mutant.edit.length_bytes).map_err(|_| PatchError::InvalidRange)?;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= source.len())
        .ok_or(PatchError::InvalidRange)?;
    if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
        return Err(PatchError::Utf8Boundary);
    }
    if &source[start..end] != mutant.edit.expected.as_bytes() {
        return Err(PatchError::ExpectedMismatch);
    }
    if mutant.edit.expected == mutant.edit.replacement {
        return Err(PatchError::NoChange);
    }
    let capacity = source
        .len()
        .checked_sub(length)
        .and_then(|size| size.checked_add(mutant.edit.replacement.len()))
        .ok_or(PatchError::OutputTooLarge)?;
    let mut patched = Vec::new();
    patched
        .try_reserve_exact(capacity)
        .map_err(|_| PatchError::OutputTooLarge)?;
    patched.extend_from_slice(&source[..start]);
    patched.extend_from_slice(mutant.edit.replacement.as_bytes());
    patched.extend_from_slice(&source[end..]);
    Ok(patched)
}

/// Read-only helper for a synchronized snapshot. Rejects every mutation path
/// symlink, including links that currently happen to remain inside the root.
/// Root canonicalization allows an invoking path such as macOS /tmp. Every
/// mutation component is opened relative to an owned directory descriptor with
/// O_NOFOLLOW, so renaming a directory or replacing a target with a symlink
/// cannot redirect the read. Callers still own snapshot consistency: concurrent
/// writes to a regular source file must be excluded by snapshot preparation.
pub fn read_and_apply_mutant(
    root: &Path,
    mutant: &Mutant,
    max_source_bytes: usize,
) -> Result<Vec<u8>, PatchError> {
    apply_mutant(&read_mutant_source(root, mutant, max_source_bytes)?, mutant)
}

/// Bind even a statically invalid candidate to its original source without
/// treating static invalidity as permission to read another file.
pub(crate) fn read_mutant_source(
    root: &Path,
    mutant: &Mutant,
    max_source_bytes: usize,
) -> Result<Vec<u8>, PatchError> {
    validate_relative_target(&mutant.file)?;
    mutant.validate_model()?;
    #[cfg(windows)]
    let bound_root = failforge_workspace::windows_fs::directory(root)?;
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(PatchError::InvalidRoot);
    }
    #[cfg(windows)]
    if failforge_workspace::windows_fs::identity(&bound_root)?
        != failforge_workspace::windows_fs::identity(&failforge_workspace::windows_fs::directory(
            &root,
        )?)?
    {
        return Err(PatchError::InvalidRoot);
    }
    let mut file = open_relative_regular_file(&root, &mutant.file)?;
    let bound = u64::try_from(max_source_bytes).map_err(|_| PatchError::SourceTooLarge)?;
    let mut source = Vec::new();
    Read::by_ref(&mut file)
        .take(bound.saturating_add(1))
        .read_to_end(&mut source)?;
    if source.len() > max_source_bytes {
        return Err(PatchError::SourceTooLarge);
    }
    if sha256_hex(&source) != mutant.source_hash {
        return Err(PatchError::HashMismatch);
    }
    Ok(source)
}

#[cfg(unix)]
fn open_relative_regular_file(root: &Path, relative: &str) -> Result<std::fs::File, PatchError> {
    let mut directory = std::fs::File::open(root)?;
    if !directory.metadata()?.is_dir() {
        return Err(PatchError::InvalidRoot);
    }
    let components: Vec<_> = relative.split('/').collect();
    for (index, component) in components.iter().enumerate() {
        let name = CString::new(*component).map_err(|_| PatchError::InvalidPath)?;
        let last = index + 1 == components.len();
        let mut status = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: directory owns a live descriptor; name is a terminated path
        // component and status has sufficient native writable storage. No
        // symlink is followed by this preliminary type query.
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                status.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: successful fstatat initialized the native structure.
        let status = unsafe { status.assume_init() };
        let kind = status.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            return Err(PatchError::Symlink);
        }
        if (last && kind != libc::S_IFREG) || (!last && kind != libc::S_IFDIR) {
            return Err(PatchError::NotFile);
        }
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: the descriptor pins the already-open parent directory and
        // the single validated component cannot traverse it. O_NOFOLLOW is the
        // enforcement even if a symlink replaced the preliminary metadata;
        // nonblocking mode prevents a replaced FIFO from blocking this open.
        let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ELOOP) {
                Err(PatchError::Symlink)
            } else {
                Err(error.into())
            };
        }
        // SAFETY: openat returned a fresh, owned descriptor adopted exactly
        // once by this File. The previous directory closes after assignment.
        let opened = unsafe { std::fs::File::from_raw_fd(descriptor) };
        if (last && !opened.metadata()?.is_file()) || (!last && !opened.metadata()?.is_dir()) {
            return Err(PatchError::NotFile);
        }
        if last {
            return Ok(opened);
        }
        directory = opened;
    }
    Err(PatchError::InvalidPath)
}

#[cfg(windows)]
fn open_relative_regular_file(root: &Path, relative: &str) -> Result<std::fs::File, PatchError> {
    use failforge_workspace::windows_fs as fs;
    use std::ffi::OsStr;
    let mut directory = fs::directory(root)?;
    let parts: Vec<_> = relative.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        directory = fs::directory_at(&directory, OsStr::new(part))?;
    }
    Ok(fs::regular_at(
        &directory,
        OsStr::new(parts[parts.len() - 1]),
        false,
        false,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ByteEdit, MutationFamily, Symbol, ValidationState};
    #[cfg(unix)]
    use std::path::PathBuf;
    fn candidate(source: &[u8], offset: u64, expected: &str, replacement: &str) -> Mutant {
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: "BOOLEAN_LITERAL_FLIP".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/a b.dart".into(),
            source_hash: sha256_hex(source),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: offset,
                length_bytes: expected.len() as u64,
                expected: expected.into(),
                replacement: replacement.into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        mutant
    }
    #[test]
    fn unicode_crlf_bom_are_preserved_and_metadata_does_not_change_id() {
        let source = "\u{feff}// кириллица 😀\r\nfinal a = true;\r\n".as_bytes();
        let offset = source
            .windows(4)
            .position(|window| window == b"true")
            .unwrap() as u64;
        let mut mutant = candidate(source, offset, "true", "false");
        let original_id = mutant.id.clone();
        let patched = apply_mutant(source, &mutant).unwrap();
        assert_eq!(
            patched,
            "\u{feff}// кириллица 😀\r\nfinal a = false;\r\n".as_bytes()
        );
        mutant.components = vec!["channel".into()];
        mutant.symbol.method_name = Some("load".into());
        mutant.validation = ValidationState::Valid;
        assert_eq!(mutant.stable_id(), original_id);
        mutant.edit.replacement = "null".into();
        assert_ne!(mutant.stable_id(), original_id);
    }
    #[test]
    fn altered_hash_expected_id_utf8_and_ranges_are_rejected() {
        let source = b"true";
        let mut mutant = candidate(source, 0, "true", "false");
        assert!(matches!(
            apply_mutant(b"false", &mutant),
            Err(PatchError::HashMismatch)
        ));
        mutant.edit.expected = "xxxx".into();
        mutant.id = mutant.stable_id();
        assert!(matches!(
            apply_mutant(source, &mutant),
            Err(PatchError::ExpectedMismatch)
        ));
        mutant = candidate(source, u64::MAX, "true", "false");
        assert!(matches!(
            apply_mutant(source, &mutant),
            Err(PatchError::InvalidRange | PatchError::Protocol(ProtocolError::InvalidEditRange))
        ));
        mutant = candidate(source, 0, "true", "true");
        assert!(matches!(
            apply_mutant(source, &mutant),
            Err(PatchError::NoChange)
        ));
        mutant = candidate(b"\xff", 0, "", "x");
        assert!(matches!(
            apply_mutant(b"\xff", &mutant),
            Err(PatchError::InvalidUtf8)
        ));
        mutant = candidate("😀".as_bytes(), 1, "", "x");
        assert!(matches!(
            apply_mutant("😀".as_bytes(), &mutant),
            Err(PatchError::Utf8Boundary)
        ));
        mutant = candidate(source, 0, "true", "false");
        mutant.id = "0".repeat(64);
        assert!(matches!(
            apply_mutant(source, &mutant),
            Err(PatchError::Protocol(ProtocolError::MutantIdMismatch))
        ));
    }
    #[test]
    fn target_lexical_rules_are_portable_and_allow_spaces_unicode() {
        for invalid in [
            "",
            "/lib/a.dart",
            "../a.dart",
            "lib/../a.dart",
            "./lib/a.dart",
            "lib//a.dart",
            "lib/a.dart/",
            "lib\\a.dart",
            "C:/a.dart",
            "C:a.dart",
            "lib/a\0.dart",
        ] {
            assert!(validate_relative_target(invalid).is_err(), "{invalid:?}");
        }
        for valid in ["lib/a b.dart", "lib/кириллица.dart", "lib/a:b.dart"] {
            validate_relative_target(valid).unwrap();
        }
    }
    #[cfg(windows)]
    #[test]
    fn windows_paths_reject_streams_devices_and_win32_aliases() {
        for path in [
            "lib/a.dart:stream",
            "lib/NUL.dart",
            "lib/COM1.dart",
            "lib/COM¹.dart",
            "lib/LPT³",
            "lib/LPT9",
            "lib/name?.dart",
            "lib/space ",
            "lib/dot.",
        ] {
            assert!(
                matches!(validate_relative_target(path), Err(PatchError::InvalidPath)),
                "{path}"
            );
        }
        assert!(validate_relative_target("lib/данные 😀.dart").is_ok());
        assert!(validate_relative_target("lib/com10.dart").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn read_helper_rejects_symlinks_and_never_writes_original() {
        use std::os::unix::fs::symlink;
        let directory = std::env::temp_dir().join(format!(
            "ff-patch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        std::fs::create_dir_all(directory.join("lib")).unwrap();
        let _cleanup = Cleanup(directory.clone());
        let path = directory.join("lib/a b.dart");
        std::fs::write(&path, b"true").unwrap();
        let mut mutant = candidate(b"true", 0, "true", "false");
        assert_eq!(
            read_and_apply_mutant(&directory, &mutant, 4).unwrap(),
            b"false"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"true");
        assert!(matches!(
            read_and_apply_mutant(&directory, &mutant, 3),
            Err(PatchError::SourceTooLarge)
        ));
        symlink(&path, directory.join("lib/link.dart")).unwrap();
        mutant.file = "lib/link.dart".into();
        mutant.id = mutant.stable_id();
        assert!(matches!(
            read_and_apply_mutant(&directory, &mutant, 4),
            Err(PatchError::Symlink)
        ));
        symlink(directory.join("lib"), directory.join("alias")).unwrap();
        mutant.file = "alias/a b.dart".into();
        mutant.id = mutant.stable_id();
        assert!(matches!(
            read_and_apply_mutant(&directory, &mutant, 4),
            Err(PatchError::Symlink)
        ));
    }
}
