//! Private acknowledgement that the CLI can safely receive cancellation.
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::FileTypeExt;

use failforge_runner::CommandSpec;

pub const READY_ENV: &str = "FAILFORGE_RUNNER_READY_FD";

pub struct Readiness(File);

impl Readiness {
    pub fn from_env() -> io::Result<Option<Self>> {
        Self::from_value(std::env::var_os(READY_ENV).as_deref())
    }

    fn from_value(value: Option<&OsStr>) -> io::Result<Option<Self>> {
        let Some(value) = value else { return Ok(None) };
        let text = value.to_str().ok_or_else(invalid)?;
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        let source: RawFd = text.parse().map_err(|_| invalid())?;
        if source < 3 {
            return Err(invalid());
        }
        // SAFETY: fcntl duplicates the integer descriptor without consuming it.
        let copied = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful duplication created this uniquely owned descriptor.
        let file = unsafe { File::from_raw_fd(copied) };
        // SAFETY: F_GETFL only inspects the verified live descriptor.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if !file.metadata()?.file_type().is_fifo() || flags & libc::O_ACCMODE != libc::O_WRONLY {
            return Err(invalid());
        }
        // Never block on an invalid/full channel. The controller's private pipe
        // is empty and a single byte is atomic; failure occurs before child spawn.
        // SAFETY: fcntl changes only flags of the inherited pipe description.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // The source belongs to the caller; retain only our owned duplicate.
        // Both must close on any subsequent child exec.
        // SAFETY: F_GETFD/F_SETFD operate only on the live source descriptor.
        let descriptor_flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe { libc::fcntl(source, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) }
                < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(Self(file)))
    }

    pub fn protect_child(&self, spec: &mut CommandSpec) {
        spec.env.set.remove(OsStr::new(READY_ENV));
        spec.env.remove.insert(READY_ENV.into());
    }

    /// Consume the writer so the acknowledgement can only be emitted once.
    pub fn acknowledge(mut self) -> io::Result<()> {
        self.0.write_all(b"R")
    }
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid native readiness capability",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn pipe() -> (File, File) {
        let mut descriptors = [-1; 2];
        // SAFETY: pipe writes exactly two descriptors into this local array.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: successful pipe created two unique owned descriptors.
        unsafe {
            (
                File::from_raw_fd(descriptors[0]),
                File::from_raw_fd(descriptors[1]),
            )
        }
    }

    #[test]
    fn strict_optional_write_pipe_capability() {
        assert!(Readiness::from_value(None).unwrap().is_none());
        for text in [
            "",
            "0",
            "1",
            "2",
            "-1",
            "+3",
            " 3",
            "3 ",
            "2147483647",
            "9999999999999999",
        ] {
            assert!(Readiness::from_value(Some(OsStr::new(text))).is_err());
        }
        let (read, write) = pipe();
        assert!(Readiness::from_value(Some(OsStr::new(&read.as_raw_fd().to_string()))).is_err());
        let regular = File::open(std::env::current_exe().unwrap()).unwrap();
        assert!(Readiness::from_value(Some(OsStr::new(&regular.as_raw_fd().to_string()))).is_err());
        let ready = Readiness::from_value(Some(OsStr::new(&write.as_raw_fd().to_string())))
            .unwrap()
            .unwrap();
        for fd in [write.as_raw_fd(), ready.0.as_raw_fd()] {
            // SAFETY: F_GETFD only observes these live fixture descriptors.
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
    }

    #[test]
    fn acknowledges_exactly_one_byte_and_removes_explicit_child_override() {
        let (mut read, write) = pipe();
        let ready = Readiness::from_value(Some(OsStr::new(&write.as_raw_fd().to_string())))
            .unwrap()
            .unwrap();
        let mut spec = CommandSpec {
            program: "program".into(),
            args: vec![],
            cwd: "/".into(),
            env: Default::default(),
            timeout_policy: Default::default(),
            capture_policy: Default::default(),
        };
        spec.env.set.insert(READY_ENV.into(), "untrusted".into());
        ready.protect_child(&mut spec);
        assert!(!spec.env.set.contains_key(OsStr::new(READY_ENV)));
        assert!(spec.env.remove.contains(OsStr::new(READY_ENV)));
        ready.acknowledge().unwrap();
        drop(write);
        let mut received = Vec::new();
        read.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"R");
    }

    #[test]
    fn closed_reader_fails_before_child_start() {
        let (read, write) = pipe();
        let ready = Readiness::from_value(Some(OsStr::new(&write.as_raw_fd().to_string())))
            .unwrap()
            .unwrap();
        drop(read);
        assert_eq!(
            ready.acknowledge().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }
}
