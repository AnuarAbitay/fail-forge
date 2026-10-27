//! Platform-specific fixture operations; Windows permissions represent only the
//! readonly attribute, while ownership/DACL assertions use windows_fs directly.
use std::fs::{self, Permissions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(crate) fn permissions(mode: u32) -> Permissions {
    #[cfg(unix)]
    {
        Permissions::from_mode(mode)
    }
    #[cfg(windows)]
    {
        let mut value = fs::metadata(std::env::current_exe().unwrap())
            .unwrap()
            .permissions();
        value.set_readonly(mode & 0o222 == 0);
        value
    }
}
pub(crate) fn readonly(path: impl AsRef<Path>) -> bool {
    fs::metadata(path).unwrap().permissions().readonly()
}
pub(crate) fn symlink(source: impl AsRef<Path>, target: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
    }
    #[cfg(windows)]
    {
        let source_path = source.as_ref();
        let resolved = if source_path.is_absolute() {
            source_path.to_owned()
        } else {
            target.as_ref().parent().unwrap().join(source_path)
        };
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
        } else {
            std::os::windows::fs::symlink_file(source, target)
        }
    }
}

pub(crate) fn absolute(path: impl AsRef<Path>) -> std::path::PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().unwrap().join(path)
    }
}

#[cfg(windows)]
#[expect(
    clippy::permissions_set_readonly_false,
    reason = "Windows permissions expose the readonly attribute, not Unix access bits"
)]
pub(crate) fn remove_tree(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if failforge_workspace::windows_fs::is_reparse(&metadata) {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        let _ = if metadata.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        };
        return;
    }
    if metadata.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                remove_tree(&entry.path());
            }
        }
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(false);
    let _ = fs::set_permissions(path, permissions);
    let _ = if metadata.is_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
}
