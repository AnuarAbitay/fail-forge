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
    #[error("cannot read or copy workspace inputs")]
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
