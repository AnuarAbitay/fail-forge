//! Shared prepared-project inspection, immutable byte bindings, and workspaces.

mod cancellation;
#[cfg(unix)]
mod copy;
#[cfg(windows)]
#[path = "copy_windows.rs"]
mod copy;
mod inputs;
#[cfg(unix)]
pub mod lifecycle;
#[cfg(windows)]
#[path = "lifecycle_windows.rs"]
pub mod lifecycle;
#[cfg(windows)]
pub mod windows_fs;

/// A path as message text. Canonical Windows paths carry the verbatim `\\?\`
/// prefix; text shows the ordinary drive or UNC spelling when that spelling
/// is unambiguous: under 260 characters and without a component ending in a
/// dot or space, which Win32 would trim. Never use the result to open files.
pub fn display_path(path: &std::path::Path) -> String {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let ordinary = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else if let Some(disk) = text
            .strip_prefix(r"\\?\")
            .filter(|rest| rest.as_bytes().get(1..3) == Some(b":\\"))
        {
            disk.to_owned()
        } else {
            return text.into_owned();
        };
        let trimmed = ordinary
            .split('\\')
            .any(|part| part.ends_with('.') || part.ends_with(' '));
        if trimmed || ordinary.encode_utf16().count() >= 260 {
            text.into_owned()
        } else {
            ordinary
        }
    }
    #[cfg(unix)]
    {
        path.display().to_string()
    }
}

pub use cancellation::Cancellation;
pub use copy::{copy_campaign_snapshot, copy_project};
pub use inputs::{
    BoundAlias, BoundTree, Hashes, PreparedProject, bound_tree, copy_snapshot, dependency_roots,
    dependency_roots_from_value, file_uri, hash_file, inspect_project, metadata_json, parse_json,
    read_regular, read_regular_prefix, source_hashes, verify_source,
};

/// Encode a script, snapshot, or packages-file argument for the Dart VM.
/// Windows VM startup parses these as URIs and misreads verbatim path prefixes.
pub fn dart_vm_argument(path: &std::path::Path) -> Result<std::ffi::OsString, InputError> {
    #[cfg(windows)]
    {
        Ok(file_uri(path)?.into())
    }
    #[cfg(unix)]
    {
        Ok(path.as_os_str().to_owned())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("cannot read or copy workspace inputs: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("workspace operation cancelled")]
    Cancelled(i32),
}

pub(crate) fn failure(message: impl Into<String>) -> InputError {
    InputError::Invalid(message.into())
}

#[cfg(test)]
mod display_path_tests {
    use super::display_path;
    use std::path::Path;

    #[cfg(windows)]
    #[test]
    fn verbatim_paths_are_shown_in_ordinary_form_only_when_unambiguous() {
        let shown = |text: &str| display_path(Path::new(text));
        assert_eq!(shown(r"\\?\C:\Users\me\pkg"), r"C:\Users\me\pkg");
        assert_eq!(shown(r"\\?\UNC\host\share\pkg"), r"\\host\share\pkg");
        assert_eq!(shown(r"C:\plain"), r"C:\plain");
        // Win32 would trim these components, so the literal spelling stays.
        assert_eq!(shown(r"\\?\C:\dir.\pkg"), r"\\?\C:\dir.\pkg");
        assert_eq!(shown(r"\\?\C:\dir \pkg"), r"\\?\C:\dir \pkg");
        // Device and volume-GUID namespaces have no ordinary spelling.
        assert_eq!(shown(r"\\?\Volume{0}\pkg"), r"\\?\Volume{0}\pkg");
        // `C:\` plus 256 characters is 259, the longest ordinary path.
        let short = format!(r"\\?\C:\{}", "a".repeat(256));
        assert_eq!(shown(&short), &short[4..]);
        let long = format!(r"\\?\C:\{}", "a".repeat(257));
        assert_eq!(shown(&long), long);
    }

    #[cfg(unix)]
    #[test]
    fn unix_paths_are_shown_as_is() {
        assert_eq!(display_path(Path::new("/tmp/pkg")), "/tmp/pkg");
    }
}

#[cfg(test)]
mod dart_vm_argument_tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;

    #[cfg(windows)]
    #[test]
    fn drive_and_verbatim_paths_share_an_encoded_vm_uri() {
        let expected = OsString::from("file:///D:/some%20space/%D0%96%20%25%23.dart");
        for path in [r"D:\some space\Ж %#.dart", r"\\?\D:\some space\Ж %#.dart"] {
            assert_eq!(dart_vm_argument(Path::new(path)).unwrap(), expected);
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_vm_arguments_preserve_original_path_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"/some space/\xff %#.dart".to_vec());
        assert_eq!(dart_vm_argument(Path::new(&path)).unwrap(), path);
    }
}
