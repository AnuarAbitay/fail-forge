//! Framed execution events with a bounded pipe write and a sealed prefix digest.
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_FRAME: usize = 64 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Stream<W = PipeWriter> {
    writer: W,
    hash: Sha256,
    count: u64,
    terminal: bool,
    failed: bool,
    limit: usize,
}

/// One whole-frame operation lets the production pipe enforce a deadline
/// spanning every short write. Tests can inject ordinary buffered writers.
pub(crate) trait FrameWriter {
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn flush_frame(&mut self) -> io::Result<()>;
}
impl<W: Write> FrameWriter for W {
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }
    fn flush_frame(&mut self) -> io::Result<()> {
        self.flush()
    }
}

impl Stream<PipeWriter> {
    pub(crate) fn stdout() -> io::Result<Self> {
        Ok(Self::new(PipeWriter::from_fd(
            libc::STDOUT_FILENO,
            WRITE_TIMEOUT,
        )?))
    }
}

impl<W: FrameWriter> Stream<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            hash: Sha256::new(),
            count: 0,
            terminal: false,
            failed: false,
            limit: MAX_FRAME,
        }
    }

    pub(crate) fn emit(&mut self, event: Value) -> io::Result<()> {
        self.emit_known(
            event,
            &[
                "stage_started",
                "stage_finished",
                "baseline",
                "original_before",
                "mutant",
                "original_after",
            ],
        )
    }

    pub(crate) fn emit_campaign(&mut self, event: Value) -> io::Result<()> {
        self.emit_known(
            event,
            &[
                "stage_started",
                "stage_finished",
                "discovery",
                "discovery_finished",
                "family_started",
                "family_event",
                "family_finished",
            ],
        )
    }

    pub(crate) fn emit_session(&mut self, event: Value) -> io::Result<()> {
        self.emit_known(
            event,
            &[
                "prepared",
                "stage_started",
                "stage_finished",
                "workspace",
                "snapshot",
                "bindings",
                "campaign_event",
                "campaign_finished",
                "cleanup",
            ],
        )
    }

    pub(crate) fn finish_session(&mut self, status: &str, error: Option<&str>) -> io::Result<()> {
        if !matches!(status, "completed" | "no_mutants" | "failed" | "cancelled")
            || (matches!(status, "completed" | "no_mutants") && error.is_some())
        {
            return Err(invalid("invalid session terminal"));
        }
        self.seal(json!({"status":status,"error":error}))
    }

    fn emit_known(&mut self, mut event: Value, kinds: &[&str]) -> io::Result<()> {
        self.ready()?;
        let fields = event
            .as_object_mut()
            .ok_or_else(|| invalid("family event must be an object"))?;
        if !fields
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| kinds.contains(&kind))
            || ["version", "sequence", "eventCount", "eventSha256"]
                .iter()
                .any(|key| fields.contains_key(*key))
        {
            return Err(invalid("invalid family event envelope"));
        }
        let next = self
            .count
            .checked_add(1)
            .ok_or_else(|| invalid("family event count overflow"))?;
        fields.insert("version".into(), 1.into());
        fields.insert("sequence".into(), next.into());
        let bytes = encode(&event, self.limit)?;
        if let Err(error) = self.writer.write_frame(&bytes) {
            self.failed = true;
            return Err(error);
        }
        // A complete LF frame belongs to the prefix even if a buffered sink's
        // flush subsequently fails. Such a sink still permanently poisons us.
        self.hash.update(&bytes);
        self.count = next;
        if let Err(error) = self.writer.flush_frame() {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed: usize,
        total: usize,
    ) -> io::Result<()> {
        if !matches!(status, "completed" | "no_mutants" | "failed" | "cancelled")
            || completed > total
        {
            return Err(invalid("invalid family terminal"));
        }
        self.seal(json!({"status":status, "error":error,
            "completedMutants":completed, "expectedMutants":total}))
    }

    pub(crate) fn finish_campaign(
        &mut self,
        status: &str,
        error: Option<&str>,
        completed_families: usize,
        expected_families: usize,
        completed_mutants: usize,
        expected_mutants: Option<usize>,
    ) -> io::Result<()> {
        if !matches!(status, "completed" | "no_mutants" | "failed" | "cancelled")
            || completed_families > expected_families
            || expected_mutants.is_some_and(|total| completed_mutants > total)
            || (expected_mutants.is_none()
                && (completed_mutants != 0 || matches!(status, "completed" | "no_mutants")))
        {
            return Err(invalid("invalid campaign terminal"));
        }
        self.seal(json!({"status":status, "error":error,
            "completedFamilies":completed_families, "expectedFamilies":expected_families,
            "completedMutants":completed_mutants, "expectedMutants":expected_mutants}))
    }

    fn seal(&mut self, mut event: Value) -> io::Result<()> {
        self.ready()?;
        let sequence = self
            .count
            .checked_add(1)
            .ok_or_else(|| invalid("family event count overflow"))?;
        event["version"] = 1.into();
        event["sequence"] = sequence.into();
        event["kind"] = "terminal".into();
        event["eventCount"] = self.count.into();
        event["eventSha256"] = format!("{:x}", self.hash.clone().finalize()).into();
        let bytes = encode(&event, self.limit)?;
        // Never append another event to a partially written terminal.
        self.terminal = true;
        if let Err(error) = self
            .writer
            .write_frame(&bytes)
            .and_then(|()| self.writer.flush_frame())
        {
            self.failed = true;
            return Err(error);
        }
        Ok(())
    }

    fn ready(&self) -> io::Result<()> {
        if self.failed || self.terminal {
            Err(invalid("family stream is already closed"))
        } else {
            Ok(())
        }
    }
}

struct Frame {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Frame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len()
            > self
                .limit
                .saturating_sub(self.bytes.len())
                .saturating_sub(1)
        {
            return Err(invalid("family event exceeds frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn encode(event: &Value, limit: usize) -> io::Result<Vec<u8>> {
    if limit == 0 {
        return Err(invalid("family event exceeds frame limit"));
    }
    let mut frame = Frame {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut frame, event)
        .map_err(|_| invalid("family event exceeds frame limit"))?;
    frame.bytes.push(b'\n');
    Ok(frame.bytes)
}

/// A private FIFO duplicate. Nonblocking writes and one deadline per frame
/// prevent a stalled controller from keeping owned attempts alive forever.
pub(crate) struct PipeWriter {
    file: File,
    timeout: Duration,
}
impl PipeWriter {
    fn from_fd(source: RawFd, timeout: Duration) -> io::Result<Self> {
        // SAFETY: duplicate the supplied descriptor without taking ownership.
        let copied = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl created this unique owned duplicate.
        let file = unsafe { File::from_raw_fd(copied) };
        // SAFETY: these commands inspect a live owned descriptor.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if !file.metadata()?.file_type().is_fifo() || flags & libc::O_ACCMODE != libc::O_WRONLY {
            return Err(invalid("family stream requires a private write pipe"));
        }
        // Both references must close across child exec. The runner redirects
        // each child stdout to its own capture before executing the program.
        // SAFETY: fcntl acts on the live source and its owned duplicate.
        let source_flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
        if source_flags < 0
            || unsafe { libc::fcntl(source, libc::F_SETFD, source_flags | libc::FD_CLOEXEC) } < 0
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, timeout })
    }

    fn frame(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + self.timeout;
        while !bytes.is_empty() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "family event pipe write timed out",
                ));
            }
            // SAFETY: the byte slice is live and the descriptor is owned.
            let count = unsafe {
                libc::write(
                    self.file.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len().min(64 * 1024),
                )
            };
            if count > 0 {
                bytes = &bytes[count as usize..];
                continue;
            }
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "family event pipe",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            self.wait(deadline)?;
        }
        Ok(())
    }

    fn wait(&self, deadline: Instant) -> io::Result<()> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "family event pipe write timed out",
                ));
            }
            let milliseconds = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
            let mut poll = libc::pollfd {
                fd: self.file.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: poll reads and updates one initialized local entry.
            let result = unsafe { libc::poll(&mut poll, 1, milliseconds) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result > 0 {
                if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "family event pipe reader closed",
                    ));
                }
                if poll.revents & libc::POLLOUT != 0 {
                    return Ok(());
                }
            }
        }
    }
}
impl FrameWriter for PipeWriter {
    fn write_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.frame(bytes)
    }
    fn flush_frame(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[path = "family_stream_tests.rs"]
mod tests;
