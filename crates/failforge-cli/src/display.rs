use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use failforge_runner::{OutputStream, StreamChunk, StreamReceiver};

pub(crate) fn spawn(mut receiver: StreamReceiver) -> io::Result<()> {
    // Duplication changes only the new descriptor's close-on-exec flag. It does
    // not change O_NONBLOCK or any shared open-file-description status flags.
    let mut stdout = duplicate_stdio(libc::STDOUT_FILENO)?;
    let mut stderr = duplicate_stdio(libc::STDERR_FILENO)?;
    std::thread::Builder::new()
        .name("failforge-display".into())
        .spawn(move || {
            while let Some(mut chunk) = receiver.blocking_recv() {
                let result = match chunk.stream {
                    OutputStream::Stdout => write_chunk(&mut stdout, &mut chunk),
                    OutputStream::Stderr => write_chunk(&mut stderr, &mut chunk),
                };
                if let Err(error) = result {
                    chunk.report_error(error);
                    break;
                }
            }
        })?;
    // Detaching is deliberate: a terminal write can block forever, while
    // lifecycle control and runtime shutdown must continue independently.
    Ok(())
}

fn duplicate_stdio(fd: RawFd) -> io::Result<File> {
    // SAFETY: fcntl takes descriptor/integer arguments and no pointers. A valid
    // descriptor is borrowed, not consumed; invalid/closed stdio returns EBADF.
    // F_DUPFD_CLOEXEC atomically gives the caller a separate, CLOEXEC descriptor.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful F_DUPFD_CLOEXEC returned a fresh descriptor. No other
    // Rust owner has adopted it; OwnedFd closes it exactly once on every path.
    let owned = unsafe { OwnedFd::from_raw_fd(duplicate) };
    Ok(File::from(owned))
}

fn write_chunk<W: Write + AsRawFd>(writer: &mut W, chunk: &mut StreamChunk) -> io::Result<()> {
    let mut offset = 0;
    while offset < chunk.bytes().len() {
        let written = write_fd_once(writer, &chunk.bytes()[offset..])?;
        offset += written;
        chunk.mark_displayed(written);
    }
    writer.flush()
}

fn write_fd_once<W: Write + AsRawFd>(writer: &mut W, bytes: &[u8]) -> io::Result<usize> {
    loop {
        match write_once(writer, bytes) {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_writable(writer.as_raw_fd())?;
            }
            result => return result,
        }
    }
}

fn wait_writable(fd: RawFd) -> io::Result<()> {
    if fd < 0 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        // SAFETY: descriptor is initialized storage for one pollfd. The caller
        // retains the writer's owned FD throughout this blocking query. This
        // runs only on the detached display thread, never on a Tokio worker.
        let result = unsafe { libc::poll(&mut descriptor, 1, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        if descriptor.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
            // Retry the actual write on HUP/ERR to retain its precise error
            // (including BrokenPipe). Readiness does not guarantee a full write.
            return Ok(());
        }
    }
}

fn write_once<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<usize> {
    loop {
        match writer.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => return Ok(written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use failforge_runner::{
        CapturePolicy, CommandSpec, EnvSpec, OutputMode, Runner, TimeoutPolicy,
    };
    use std::collections::VecDeque;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    struct ScriptedWriter {
        results: VecDeque<io::Result<usize>>,
        bytes: Vec<u8>,
    }

    impl Write for ScriptedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let written = self.results.pop_front().expect("unexpected write")?;
            self.bytes.extend_from_slice(&bytes[..written]);
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn writer(results: impl IntoIterator<Item = io::Result<usize>>) -> ScriptedWriter {
        ScriptedWriter {
            results: results.into_iter().collect(),
            bytes: Vec::new(),
        }
    }

    #[test]
    fn exact_binary_bytes_are_written_without_conversion() {
        let mut writer = writer([Ok(4)]);
        assert_eq!(write_once(&mut writer, &[0, 255, 128, 10]).unwrap(), 4);
        assert_eq!(writer.bytes, [0, 255, 128, 10]);
    }

    #[test]
    fn partial_write_returns_only_confirmed_bytes() {
        let mut writer = writer([Ok(2), Ok(2)]);
        let first = write_once(&mut writer, b"abcd").unwrap();
        assert_eq!(first, 2);
        assert_eq!(write_once(&mut writer, &b"abcd"[first..]).unwrap(), 2);
        assert_eq!(writer.bytes, b"abcd");
    }

    #[test]
    fn interrupted_write_is_retried_without_losing_bytes() {
        let mut writer = writer([Err(io::ErrorKind::Interrupted.into()), Ok(3)]);
        assert_eq!(write_once(&mut writer, b"abc").unwrap(), 3);
        assert_eq!(writer.bytes, b"abc");
    }

    #[test]
    fn zero_write_and_sink_failure_are_explicit_errors() {
        let mut zero = writer([Ok(0)]);
        assert_eq!(
            write_once(&mut zero, b"abc").unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        let mut failed = writer([Err(io::ErrorKind::BrokenPipe.into())]);
        assert_eq!(
            write_once(&mut failed, b"abc").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn duplicate_owns_a_cloexec_descriptor_and_preserves_shared_status_flags() {
        let (source, mut reader) = UnixStream::pair().unwrap();
        // SAFETY: source owns this live descriptor; F_GETFL takes no pointers.
        let original_flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
        let mut file = duplicate_stdio(source.as_raw_fd()).unwrap();
        assert_ne!(file.as_raw_fd(), source.as_raw_fd());
        // SAFETY: both descriptors have live owners, and these fcntl queries
        // only inspect integer status/descriptor flags without modifying them.
        let (source_flags, duplicate_flags, descriptor_flags) = unsafe {
            (
                libc::fcntl(source.as_raw_fd(), libc::F_GETFL),
                libc::fcntl(file.as_raw_fd(), libc::F_GETFL),
                libc::fcntl(file.as_raw_fd(), libc::F_GETFD),
            )
        };
        assert_eq!(source_flags, original_flags);
        assert_eq!(duplicate_flags, original_flags);
        assert_ne!(descriptor_flags & libc::FD_CLOEXEC, 0);
        drop(source);
        file.write_all(&[0, 255, 128, 10]).unwrap();
        let mut actual = [0; 4];
        reader.read_exact(&mut actual).unwrap();
        assert_eq!(actual, [0, 255, 128, 10]);
        drop(file);
        assert_eq!(reader.read(&mut [0]).unwrap(), 0);
        assert!(duplicate_stdio(-1).is_err());
    }

    struct BackpressuredFile {
        file: File,
        blocked: Option<mpsc::Sender<()>>,
    }

    impl Write for BackpressuredFile {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let result = self.file.write(bytes);
            match &result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if let Some(blocked) = self.blocked.take() {
                        blocked.send(()).unwrap();
                    }
                }
                _ => {}
            }
            result
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    impl AsRawFd for BackpressuredFile {
        fn as_raw_fd(&self) -> RawFd {
            self.file.as_raw_fd()
        }
    }

    #[test]
    fn real_eagain_is_retried_after_the_reader_drains_without_changing_flags() {
        let (source, mut reader) = UnixStream::pair().unwrap();
        source.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut file = duplicate_stdio(source.as_raw_fd()).unwrap();
        drop(source);
        let observer = file.try_clone().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut prefilled = 0;
        loop {
            assert!(
                Instant::now() < deadline,
                "test socket never reached EAGAIN"
            );
            match file.write(&[b'p'; 4096]) {
                Ok(0) => panic!("socket prefill wrote zero"),
                Ok(count) => prefilled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("socket prefill: {error}"),
            }
        }
        // SAFETY: observer owns a live duplicate; this query only reads flags.
        let flags = unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut writer = BackpressuredFile {
                file,
                blocked: Some(blocked_tx),
            };
            let result = write_fd_once(&mut writer, &[0, 255, 128]);
            let _ = finished_tx.send(result);
        });
        // This acknowledgement comes from the actual failed native write,
        // before poll, so the test cannot accidentally bypass its EAGAIN branch.
        blocked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut received = Vec::new();
        let mut completed = None;
        loop {
            let mut bytes = [0; 4096];
            match reader.read(&mut bytes) {
                Ok(0) => panic!("socket closed before the retried write"),
                Ok(count) => received.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::park_timeout(Duration::from_millis(2));
                }
                Err(error) => panic!("socket drain: {error}"),
            }
            if completed.is_none() {
                completed = finished_rx.try_recv().ok();
            }
            if let Some(result) = &completed {
                assert_eq!(result.as_ref().unwrap(), &3);
                if received.len() == prefilled + 3 {
                    break;
                }
            }
            assert!(received.len() <= prefilled + 3);
            assert!(
                Instant::now() < deadline,
                "EAGAIN recovery exceeded test deadline"
            );
        }
        thread.join().unwrap();
        assert_eq!(&received[..prefilled], vec![b'p'; prefilled]);
        assert_eq!(&received[prefilled..], [0, 255, 128]);
        // SAFETY: observer's open file description remains owned and unchanged.
        assert_eq!(
            unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }

    #[test]
    fn closing_the_reader_after_actual_eagain_returns_broken_pipe_without_hanging() {
        let (source, reader) = UnixStream::pair().unwrap();
        source.set_nonblocking(true).unwrap();
        let mut file = duplicate_stdio(source.as_raw_fd()).unwrap();
        drop(source);
        let observer = file.try_clone().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            assert!(
                Instant::now() < deadline,
                "test socket never reached EAGAIN"
            );
            match file.write(&[b'p'; 4096]) {
                Ok(0) => panic!("socket prefill wrote zero"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("socket prefill: {error}"),
            }
        }
        // SAFETY: observer owns a live duplicate; this query only reads flags.
        let flags = unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut writer = BackpressuredFile {
                file,
                blocked: Some(blocked_tx),
            };
            let result = write_fd_once(&mut writer, &[0, 255, 128]);
            let _ = finished_tx.send(result);
        });
        let acknowledgement = blocked_rx.recv_timeout(Duration::from_secs(2));
        // Close the independent peer before asserting even when ACK failed.
        // The native HUP/ERR must release poll and let the writer finish.
        drop(reader);
        let completed = finished_rx.recv_timeout(Duration::from_secs(2));
        let joined = if completed.is_ok() {
            Some(thread.join())
        } else {
            None
        };
        assert!(
            acknowledgement.is_ok(),
            "native write never acknowledged EAGAIN"
        );
        let error = completed
            .expect("closed reader left writer pending")
            .unwrap_err();
        joined.unwrap().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        // SAFETY: observer retains the writer's original open file description.
        assert_eq!(
            unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }

    #[test]
    fn an_invalid_readiness_descriptor_is_an_explicit_error() {
        assert_eq!(
            wait_writable(-1).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    struct PartialFile {
        file: File,
        calls: usize,
    }

    impl Write for PartialFile {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            self.file.write(&bytes[..bytes.len().min(2)])
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    impl AsRawFd for PartialFile {
        fn as_raw_fd(&self) -> RawFd {
            self.file.as_raw_fd()
        }
    }

    #[tokio::test]
    async fn actual_fd_partial_writes_acknowledge_the_production_stream_chunk() {
        let (source, mut reader) = UnixStream::pair().unwrap();
        let mut writer = PartialFile {
            file: duplicate_stdio(source.as_raw_fd()).unwrap(),
            calls: 0,
        };
        reader
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let runner = Runner::new();
        let spec = CommandSpec {
            program: "/bin/sh".into(),
            args: ["-c", "printf '\\000\\377abcdef'"].map(Into::into).to_vec(),
            cwd: std::env::current_dir().unwrap(),
            env: EnvSpec::default(),
            timeout_policy: TimeoutPolicy {
                timeout: Some(Duration::from_secs(1)),
                ..TimeoutPolicy::default()
            },
            capture_policy: CapturePolicy {
                mode: OutputMode::StreamAndCapture,
                ..CapturePolicy::default()
            },
        };
        let mut handle = runner.start(spec).unwrap();
        let mut receiver = handle.take_stream().unwrap();
        let captured = tokio::time::timeout(Duration::from_secs(2), async {
            let mut bytes = Vec::new();
            while let Some(mut chunk) = receiver.recv().await {
                assert_eq!(chunk.stream, OutputStream::Stdout);
                bytes.extend_from_slice(chunk.bytes());
                write_chunk(&mut writer, &mut chunk).unwrap();
            }
            bytes
        })
        .await
        .unwrap();
        assert_eq!(captured, [0, 255, b'a', b'b', b'c', b'd', b'e', b'f']);
        assert!(writer.calls >= 4);
        let mut actual = [0; 8];
        reader.read_exact(&mut actual).unwrap();
        assert_eq!(actual, [0, 255, b'a', b'b', b'c', b'd', b'e', b'f']);
        drop(receiver);
        let result = tokio::time::timeout(Duration::from_secs(2), handle.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.stdout.stream_dropped_bytes, 0);
        assert_eq!(result.stdout.stream_unconfirmed_bytes, 0);
        assert!(result.cleanup.is_complete());
        runner.shutdown().await.unwrap();
    }
}
