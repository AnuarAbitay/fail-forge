//! A small Rust launcher keeps the cleanup owner alive after controller death.
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitCode, Stdio};

use failforge_workspace::Cancellation;
use tokio::signal::unix::{SignalKind, signal};

pub(crate) const PARENT_ENV: &str = "FAILFORGE_MUTATION_PARENT_FD";

pub(crate) fn inherited_parent(value: &OsStr) -> io::Result<File> {
    let fd = value
        .to_str()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|fd| *fd >= 3)
        .ok_or_else(|| io::Error::other("invalid campaign parent channel"))?;
    // SAFETY: inspect/duplicate a borrowed descriptor before taking ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDONLY {
        return Err(io::Error::other("invalid campaign parent channel"));
    }
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(duplicate) };
    if !file.metadata()?.file_type().is_fifo() {
        return Err(io::Error::other("campaign parent channel must be a pipe"));
    }
    // The inherited source is exclusively this protocol's capability; consume it
    // after validating the duplicate. Only the private CLOEXEC handle remains.
    drop(unsafe { File::from_raw_fd(fd) });
    Ok(file)
}

pub(crate) fn watch_parent(mut parent: File, cancel: Cancellation) -> io::Result<()> {
    std::thread::Builder::new()
        .name("campaign-parent".into())
        .spawn(move || {
            let mut byte = [0u8; 1];
            loop {
                match parent.read(&mut byte) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    // No bytes are valid on this liveness-only channel. EOF, input
                    // or a broken channel all require draining the cleanup owner.
                    _ => {
                        cancel.request(libc::SIGINT);
                        break;
                    }
                }
            }
        })?;
    Ok(())
}

fn pipe() -> io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: pipe initializes both elements of this local descriptor array.
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let source = unsafe { File::from_raw_fd(descriptors[0]) };
    let sink = unsafe { File::from_raw_fd(descriptors[1]) };
    fn private(file: &File) -> io::Result<File> {
        // Keep protocol descriptors outside standard streams, even if callers
        // closed stdin/stdout/stderr before starting the CLI.
        let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    Ok((private(&source)?, private(&sink)?))
}

async fn launch() -> io::Result<u8> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let (parent, keep_alive) = pipe()?;
    let descriptor = parent.as_raw_fd();
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(std::env::args_os().skip(1))
        .stdin(Stdio::null())
        .env(PARENT_ENV, descriptor.to_string());
    // SAFETY: the child-only pre-exec action uses fcntl on a retained descriptor;
    // it neither allocates nor locks. Only this read capability crosses exec.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let pid = child.id() as libc::pid_t;
    drop(parent);
    let mut waiting = tokio::task::spawn_blocking(move || child.wait());
    let mut forwarded = false;
    let status = loop {
        tokio::select! {
            result = &mut waiting => break result.map_err(io::Error::other)??,
            _ = interrupt.recv(), if !forwarded => {
                // SAFETY: this is the retained child's PID, not a process group.
                unsafe { libc::kill(pid, libc::SIGINT); }
                forwarded = true;
            },
            _ = terminate.recv(), if !forwarded => {
                unsafe { libc::kill(pid, libc::SIGTERM); }
                forwarded = true;
            },
        }
    };
    drop(keep_alive);
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
        .clamp(0, 255) as u8)
}

pub(crate) fn execute() -> ExitCode {
    let result = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(launch()));
    match result {
        Ok(code) => ExitCode::from(code),
        Err(_) => ExitCode::from(125),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn private_pipe_does_not_inherit_and_closed_parent_requests_drain() {
        let (reader, writer) = pipe().unwrap();
        for file in [&reader, &writer] {
            assert!(file.as_raw_fd() >= 3);
            assert_ne!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        let cancel = Cancellation::default();
        watch_parent(reader, cancel.clone()).unwrap();
        assert!(cancel.signum().is_none());
        drop(writer);
        let limit = Instant::now() + Duration::from_secs(1);
        while cancel.signum().is_none() && Instant::now() < limit {
            std::thread::yield_now();
        }
        assert_eq!(cancel.signum(), Some(libc::SIGINT));
    }

    #[test]
    fn rejects_non_pipe_or_standard_parent_capabilities() {
        for invalid in ["-1", "0", "2", "word", "999999"] {
            assert!(inherited_parent(OsStr::new(invalid)).is_err());
        }
        let file = File::open("/dev/null").unwrap();
        assert!(inherited_parent(OsStr::new(&file.as_raw_fd().to_string())).is_err());
        assert!(
            file.metadata().is_ok(),
            "invalid capability must remain borrowed"
        );
    }
}
