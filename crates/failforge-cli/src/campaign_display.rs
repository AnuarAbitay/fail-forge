//! Bounded terminal delivery never owns campaign cleanup or a workspace lease.
#[cfg(unix)]
use std::fs::File;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::{FromRawFd, RawFd};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

enum Message {
    Text { stderr: bool, text: String },
    Flush(mpsc::SyncSender<()>),
}

pub(crate) struct Display {
    sender: mpsc::SyncSender<Message>,
    failure: Arc<Mutex<Option<String>>>,
}

#[cfg(unix)]
fn duplicate(fd: RawFd) -> io::Result<File> {
    // SAFETY: borrow the standard descriptor; adopt only the new owned duplicate.
    let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

impl Display {
    pub(crate) fn stdio() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::start(
                duplicate(libc::STDOUT_FILENO)?,
                duplicate(libc::STDERR_FILENO)?,
            )
        }
        #[cfg(windows)]
        {
            Self::start(
                crate::windows_output::Writer::stdio(false, Duration::from_millis(250))?,
                crate::windows_output::Writer::stdio(true, Duration::from_millis(250))?,
            )
        }
    }

    fn start(
        mut stdout: impl Write + Send + 'static,
        mut stderr: impl Write + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(64);
        let failure = Arc::new(Mutex::new(None));
        let failed = Arc::clone(&failure);
        std::thread::Builder::new()
            .name("campaign-display".into())
            .spawn(move || {
                for message in receiver {
                    let result = match message {
                        Message::Text {
                            stderr: selected,
                            text,
                        } => {
                            let writer: &mut dyn Write =
                                if selected { &mut stderr } else { &mut stdout };
                            writer
                                .write_all(text.as_bytes())
                                .and_then(|()| writer.flush())
                        }
                        Message::Flush(done) => {
                            let _ = done.send(());
                            Ok(())
                        }
                    };
                    if let Err(error) = result {
                        *failed.lock().unwrap_or_else(|error| error.into_inner()) =
                            Some(format!("campaign terminal output failed: {error}"));
                        break;
                    }
                }
            })?;
        Ok(Self { sender, failure })
    }

    pub(crate) fn check(&self) -> io::Result<()> {
        match &*self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            Some(message) => Err(io::Error::other(message.clone())),
            None => Ok(()),
        }
    }

    pub(crate) fn text(&self, stderr: bool, text: String) -> io::Result<()> {
        self.check()?;
        self.sender
            .try_send(Message::Text { stderr, text })
            .map_err(|_| io::Error::other("campaign terminal output is unavailable or blocked"))
    }

    /// Progress is advisory: a burst may skip terminal updates, never journal records.
    pub(crate) fn progress(&self, text: String) -> io::Result<()> {
        self.check()?;
        match self.sender.try_send(Message::Text { stderr: true, text }) {
            Ok(()) | Err(mpsc::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(io::Error::other("campaign terminal output is unavailable"))
            }
        }
    }

    pub(crate) fn flush(&self, timeout: Duration) -> io::Result<()> {
        self.check()?;
        let start = Instant::now();
        let (done, receiver) = mpsc::sync_channel(1);
        let mut message = Message::Flush(done);
        loop {
            match self.sender.try_send(message) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(returned)) => {
                    message = returned;
                    let remaining = timeout.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return Err(io::Error::other("campaign terminal output did not drain"));
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(1)));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(io::Error::other("campaign terminal output is unavailable"));
                }
            }
        }
        receiver
            .recv_timeout(timeout.saturating_sub(start.elapsed()))
            .map_err(|_| io::Error::other("campaign terminal output did not drain"))?;
        self.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, value: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(value);
            Ok(value.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn streams_are_separate_and_flush_orders_all_messages() {
        let stdout = Buffer(Arc::new(Mutex::new(Vec::new())));
        let stderr = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = Display::start(stdout.clone(), stderr.clone()).unwrap();
        display.text(true, "progress\n".into()).unwrap();
        display.text(false, "table\n".into()).unwrap();
        display.flush(Duration::from_secs(1)).unwrap();
        assert_eq!(*stdout.0.lock().unwrap(), b"table\n");
        assert_eq!(*stderr.0.lock().unwrap(), b"progress\n");
    }

    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_output_is_reported_without_waiting_for_cleanup() {
        let display = Display::start(Broken, Broken).unwrap();
        display.text(true, "progress\n".into()).unwrap();
        assert!(display.flush(Duration::from_secs(1)).is_err());
    }

    struct Blocked(mpsc::Receiver<()>);
    impl Write for Blocked {
        fn write(&mut self, value: &[u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(value.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_output_has_bounded_queue_and_bounded_drain() {
        let (release, receive) = mpsc::channel();
        let display = Display::start(Blocked(receive), io::sink()).unwrap();
        display.text(false, "blocked".into()).unwrap();
        assert!(display.flush(Duration::from_millis(20)).is_err());
        assert!((0..100).any(|_| display.text(false, "bounded".into()).is_err()));
        drop(release);
    }

    #[test]
    fn progress_bursts_do_not_fail_a_healthy_terminal_or_lose_final_output() {
        let stdout = Buffer(Arc::new(Mutex::new(Vec::new())));
        let display = Display::start(stdout.clone(), io::sink()).unwrap();
        for _ in 0..10_000 {
            display.progress("completed\n".into()).unwrap();
        }
        display.flush(Duration::from_secs(1)).unwrap();
        display.text(false, "final table\n".into()).unwrap();
        display.flush(Duration::from_secs(1)).unwrap();
        assert_eq!(*stdout.0.lock().unwrap(), b"final table\n");
    }

    #[test]
    fn saturated_progress_still_reports_a_blocked_terminal_at_flush() {
        let (release, receive) = mpsc::channel();
        let display = Display::start(io::sink(), Blocked(receive)).unwrap();
        for _ in 0..10_000 {
            display.progress("completed\n".into()).unwrap();
        }
        assert!(display.flush(Duration::from_millis(20)).is_err());
        drop(release);
    }
}
