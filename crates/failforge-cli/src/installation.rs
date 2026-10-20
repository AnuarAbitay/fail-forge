//! Locate resources relative to the running executable, never the build host.
use std::io;
use std::path::{Path, PathBuf};

/// Keep explicit paths relative to the invocation directory for `Context::new`.
pub(crate) fn resolve(requested: Option<&Path>) -> io::Result<PathBuf> {
    if let Some(requested) = requested {
        return Ok(requested.to_owned());
    }
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|_| {
            io::Error::other("cannot locate the running executable; use --installation PATH")
        })?;
    from_executable(&executable).ok_or_else(|| {
        io::Error::other(
            "cannot determine the installation from the executable location; use --installation PATH",
        )
    })
}

fn from_executable(executable: &Path) -> Option<PathBuf> {
    if !executable.is_absolute() || executable.file_name().is_none() {
        return None;
    }
    let directory = executable.parent()?;
    if directory.file_name()? == "bin" {
        return directory.parent().map(Path::to_owned);
    }
    if matches!(directory.file_name()?.to_str(), Some("debug" | "release")) {
        let target = directory.parent()?;
        if target.file_name()? == "target" {
            return target.parent().map(Path::to_owned);
        }
    }
    None
}

#[cfg(test)]
#[path = "installation_tests.rs"]
mod tests;
