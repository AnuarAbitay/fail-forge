//! Private result handoff to the campaign controller, never inherited by tests.
use std::ffi::OsStr;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use failforge_runner::{CaptureSummary, CommandResult, CommandSpec};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::args::TestReporter;

pub(crate) const EVIDENCE_ENV: &str = "FAILFORGE_TEST_EVIDENCE_FD";
const MAX_CAPTURE: u64 = 256 * 1024 * 1024;
const MAX_REPORT: usize = 64 * 1024 * 1024;

pub(crate) struct EvidenceHandoff {
    file: File,
    mode: TestReporter,
    context: Option<failforge_dart::SelectionContext>,
}

impl EvidenceHandoff {
    pub(crate) fn from_env(mode: Option<TestReporter>) -> io::Result<Option<Self>> {
        let mut evidence = Self::from_value(std::env::var_os(EVIDENCE_ENV).as_deref(), mode)?;
        let context = crate::test_context::from_env(evidence.is_some())?;
        if let Some(evidence) = &mut evidence {
            evidence.context = context;
        }
        Ok(evidence)
    }

    fn from_value(value: Option<&OsStr>, mode: Option<TestReporter>) -> io::Result<Option<Self>> {
        let (value, mode) = match (value, mode) {
            (None, None) => return Ok(None),
            (Some(value), Some(mode)) => (value, mode),
            _ => return Err(invalid()),
        };
        let text = value.to_str().ok_or_else(invalid)?;
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        let source: RawFd = text.parse().map_err(|_| invalid())?;
        if source < 3 {
            return Err(invalid());
        }
        // SAFETY: duplicate this descriptor without adopting the caller's owner.
        let copied = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful duplication returned a fresh owned descriptor.
        let mut file = unsafe { File::from_raw_fd(copied) };
        validate_handoff(&mut file)?;
        // SAFETY: these fcntl calls access flags of a verified live descriptor.
        let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(source, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(Self {
            file,
            mode,
            context: None,
        }))
    }

    pub(crate) fn protect_child(&self, spec: &mut CommandSpec) {
        spec.env.set.remove(OsStr::new(EVIDENCE_ENV));
        spec.env.remove.insert(EVIDENCE_ENV.into());
        spec.env
            .set
            .remove(OsStr::new(crate::test_context::CONTEXT_ENV));
        spec.env
            .remove
            .insert(crate::test_context::CONTEXT_ENV.into());
    }

    /// Called only after Runner shutdown. A terminal LF commits the envelope;
    /// failed writes/syncs leave an uncommitted result, even if JSON is complete.
    pub(crate) fn publish(
        mut self,
        result: &CommandResult,
        exit_code: u8,
        native_ok: bool,
        cwd: &Path,
    ) -> io::Result<()> {
        let stdout = read_capture(&result.stdout, true)?;
        let stderr = read_capture(&result.stderr, false)?;
        let mut classification =
            failforge_dart::classify_dart_result(&stdout.bytes, result, self.mode.mode());
        if !native_ok {
            classification["outcome"] = "INFRA_ERROR".into();
            classification["reason"] = "native_owner_failed".into();
        }
        let mut catalogue = None;
        let context = self.context.unwrap_or_default();
        if matches!(
            classification["outcome"].as_str(),
            Some("KILLED" | "SURVIVED" | "LOAD_FAILURE")
        ) {
            match failforge_dart::analyze_catalogue(&stdout.bytes, cwd, self.mode.mode()) {
                Ok(snapshot) => {
                    failforge_dart::apply_selection(&mut classification, &snapshot, &context)
                        .map_err(|_| invalid())?;
                    catalogue = Some(snapshot);
                }
                Err(_) => {
                    classification["outcome"] = "INFRA_ERROR".into();
                    classification["reason"] = "invalid_author_test_catalogue".into();
                }
            }
        }
        let initialization = failforge_dart::initialization_handoff(
            &stdout.bytes,
            cwd,
            self.mode.mode(),
            &mut classification,
            &context,
        )
        .map_err(|_| invalid())?;
        let value = json!({
            "version":3,"reporterMode":self.mode.as_str(),"ownerExitCode":exit_code,
            "stdoutBytes":stdout.length,"stderrBytes":stderr.length,
            "stdoutSha256":stdout.hash,"stderrSha256":stderr.hash,
            "classification":classification,
            "catalogue":catalogue,
            "initialization":initialization,
        });
        let mut encoded = BoundedReport(Vec::new());
        serde_json::to_writer(&mut encoded, &value)?;
        validate_handoff(&mut self.file)?;
        self.file.write_all(&encoded.0)?;
        self.file.sync_all()?;
        self.file.write_all(b"\n")
    }
}

fn validate_handoff(file: &mut File) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: getters take no pointers and do not modify descriptor state.
    let (uid, flags) = unsafe {
        (
            libc::geteuid(),
            libc::fcntl(file.as_raw_fd(), libc::F_GETFL),
        )
    };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o7777 != 0o600
        || metadata.nlink() != 0
        || metadata.len() != 0
        || flags < 0
        || flags & libc::O_ACCMODE != libc::O_RDWR
        || flags & libc::O_APPEND != 0
        || file.stream_position()? != 0
    {
        return Err(invalid());
    }
    Ok(())
}

struct Capture {
    bytes: Vec<u8>,
    length: u64,
    hash: String,
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.is_file()
        && b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn read_capture(summary: &CaptureSummary, retain: bool) -> io::Result<Capture> {
    let path = summary.file_path.as_deref().ok_or_else(invalid)?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let before = file.metadata()?;
    if !before.is_file()
        || before.len() > MAX_CAPTURE
        || before.len() != summary.bytes_read
        || before.len() != summary.file_bytes_written
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    let mut hash = Sha256::new();
    let mut length = 0u64;
    let mut buffer = [0u8; 32 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        length += read as u64;
        if length > before.len() {
            return Err(invalid());
        }
        hash.update(&buffer[..read]);
        if retain {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }
    if length != before.len()
        || !same_file(&before, &file.metadata()?)
        || !same_file(&before, &std::fs::symlink_metadata(path)?)
    {
        return Err(invalid());
    }
    let digest = hash.finalize();
    if summary.sha256 != Some(digest.into()) {
        return Err(invalid());
    }
    Ok(Capture {
        bytes,
        length,
        hash: format!("{digest:x}"),
    })
}

struct BoundedReport(Vec<u8>);

impl Write for BoundedReport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (MAX_REPORT - 1).saturating_sub(self.0.len()) {
            return Err(invalid());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid native test evidence")
}

#[cfg(test)]
#[path = "test_evidence_tests.rs"]
mod tests;
