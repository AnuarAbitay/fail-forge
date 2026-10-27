//! Small native boundaries used by the shared campaign owner.
use failforge_runner::WorkspaceLease;
use std::fs::File;
use std::io;

pub(crate) fn workspace_lease(file: &File) -> io::Result<WorkspaceLease> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        WorkspaceLease::from_borrowed(file.as_fd())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        WorkspaceLease::from_borrowed(file.as_handle())
    }
}
