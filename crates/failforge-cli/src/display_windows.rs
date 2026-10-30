//! Bounded native stream delivery, independent of SDK/process cleanup.
use crate::windows_output::Writer;
use failforge_runner::{OutputStream, StreamReceiver};
use std::io::{self, Write};
use std::time::Duration;
pub(crate) fn spawn(mut receiver: StreamReceiver) -> io::Result<()> {
    let mut stdout = Writer::stdio(false, Duration::from_secs(1))?;
    let mut stderr = Writer::stdio(true, Duration::from_secs(1))?;
    std::thread::Builder::new()
        .name("failforge-display".into())
        .spawn(move || {
            while let Some(mut chunk) = receiver.blocking_recv() {
                let writer = match chunk.stream {
                    OutputStream::Stdout => &mut stdout,
                    OutputStream::Stderr => &mut stderr,
                };
                let mut offset = 0;
                while offset < chunk.bytes().len() {
                    match writer.write(&chunk.bytes()[offset..]) {
                        Ok(0) => {
                            chunk.report_error(io::ErrorKind::WriteZero.into());
                            return;
                        }
                        Ok(count) => {
                            offset += count;
                            chunk.mark_displayed(count);
                        }
                        Err(error) => {
                            chunk.report_error(error);
                            return;
                        }
                    }
                }
            }
        })?;
    Ok(())
}

/// The spelling of a path shown to a user. Canonical paths carry the verbatim
/// `\\?\` prefix; the ordinary drive or UNC form is used when Win32 lookup
/// proves that it names the same file, so the path can be copied as is. A path
/// that does not exist yet, such as a report directory, uses the lexical
/// ordinary spelling when that spelling is unambiguous.
pub(crate) fn user_path(path: &std::path::Path) -> std::path::PathBuf {
    let shown = failforge_runner::windows_process_path(path);
    if shown.exists() {
        shown
    } else {
        failforge_workspace::display_path(&shown).into()
    }
}

#[cfg(test)]
mod user_path_tests {
    use super::user_path;

    #[test]
    fn existing_verbatim_paths_are_shown_without_the_prefix() {
        let canonical = std::env::temp_dir().canonicalize().unwrap();
        assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
        let shown = user_path(&canonical);
        assert!(!shown.to_string_lossy().starts_with(r"\\?\"), "{shown:?}");
        assert_eq!(shown.canonicalize().unwrap(), canonical);
        // A path that does not exist yet uses the lexical ordinary spelling.
        let missing = canonical.join("ff-user-path-missing-entry");
        assert_eq!(
            user_path(&missing),
            shown.join("ff-user-path-missing-entry")
        );
        // Unless Win32 would trim it differently; then the literal stays.
        let trimmed = canonical.join("ff-user-path-missing-entry.");
        assert_eq!(user_path(&trimmed), trimmed);
    }
}
