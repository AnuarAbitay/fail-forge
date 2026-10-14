//! Diagnostic writes run outside Tokio. A blocked stderr must never prevent
//! process cleanup or runtime shutdown; delivery therefore has a finite budget.
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

const QUEUE_MESSAGES: usize = 64;
const MESSAGE_BYTES: usize = 4096;

struct Message {
    bytes: Vec<u8>,
    completed: Option<mpsc::Sender<bool>>,
}

#[derive(Clone)]
pub(crate) struct Diagnostics {
    sender: SyncSender<Message>,
}

impl Diagnostics {
    pub(crate) fn new() -> io::Result<Self> {
        Self::with_writer(io::stderr())
    }

    fn with_writer<W: Write + Send + 'static>(writer: W) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_MESSAGES);
        std::thread::Builder::new()
            .name("failforge-diagnostics".into())
            .spawn(move || write_messages(writer, receiver))?;
        Ok(Self { sender })
    }

    /// Returns whether stderr confirmed delivery within the supplied budget.
    /// No thread join is performed: stderr may itself be a blocked pipe.
    pub(crate) fn report(&self, message: &str, budget: Duration) -> bool {
        let (completed, receipt) = mpsc::channel();
        let message = Message {
            bytes: message.as_bytes()[..message.len().min(MESSAGE_BYTES)].to_vec(),
            completed: Some(completed),
        };
        self.sender.try_send(message).is_ok() && receipt.recv_timeout(budget).unwrap_or(false)
    }
}

impl Write for Diagnostics {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Tracing is best effort, bounded in both message count and bytes.
        // Reporting its sink failure through the same sink would recurse.
        let message = Message {
            bytes: bytes[..bytes.len().min(MESSAGE_BYTES)].to_vec(),
            completed: None,
        };
        let _ = self.sender.try_send(message);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_messages<W: Write>(mut writer: W, receiver: Receiver<Message>) {
    while let Ok(message) = receiver.recv() {
        let delivered = writer
            .write_all(&message.bytes)
            .and_then(|()| writer.flush())
            .is_ok();
        if let Some(completed) = message.completed {
            let _ = completed.send(delivered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn acknowledged_diagnostic_is_written_exactly() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let diagnostics = Diagnostics::with_writer(Buffer(bytes.clone())).unwrap();
        assert!(diagnostics.report("summary\n", Duration::from_secs(1)));
        assert_eq!(*bytes.lock().unwrap(), b"summary\n");
    }

    #[test]
    fn diagnostic_size_is_bounded() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let diagnostics = Diagnostics::with_writer(Buffer(bytes.clone())).unwrap();
        assert!(diagnostics.report(&"x".repeat(MESSAGE_BYTES * 2), Duration::from_secs(1)));
        assert_eq!(bytes.lock().unwrap().len(), MESSAGE_BYTES);
    }

    #[test]
    fn tracing_writer_and_flush_are_nonblocking_best_effort() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let mut diagnostics = Diagnostics::with_writer(Buffer(bytes.clone())).unwrap();
        assert_eq!(diagnostics.write(b"trace\n").unwrap(), 6);
        diagnostics.flush().unwrap();
        assert!(diagnostics.report("barrier", Duration::from_secs(1)));
        assert_eq!(*bytes.lock().unwrap(), b"trace\nbarrier");
    }

    struct FailedWriter;
    impl Write for FailedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_diagnostic_is_not_reported_as_delivered() {
        let diagnostics = Diagnostics::with_writer(FailedWriter).unwrap();
        assert!(!diagnostics.report("summary", Duration::from_secs(1)));
    }

    struct BlockedWriter {
        ready: Option<mpsc::Sender<()>>,
        release: Receiver<()>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(ready) = self.ready.take() {
                ready.send(()).unwrap();
                let _ = self.release.recv();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_stderr_does_not_block_the_reporting_caller() {
        let (ready, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let mut diagnostics = Diagnostics::with_writer(BlockedWriter {
            ready: Some(ready),
            release: resume,
        })
        .unwrap();
        diagnostics.write_all(b"blocked").unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let before = std::time::Instant::now();
        assert!(!diagnostics.report("summary", Duration::from_millis(20)));
        assert!(before.elapsed() < Duration::from_secs(1));
        // Releasing the test writer is independent of the diagnostic deadline.
        release.send(()).unwrap();
        assert!(diagnostics.report("barrier", Duration::from_secs(1)));
    }

    #[test]
    fn a_full_diagnostic_queue_is_bounded_and_drops_new_messages() {
        let (ready, started) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let mut diagnostics = Diagnostics::with_writer(BlockedWriter {
            ready: Some(ready),
            release: resume,
        })
        .unwrap();
        diagnostics.write_all(b"blocked").unwrap();
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..QUEUE_MESSAGES + 10 {
            diagnostics.write_all(b"x").unwrap();
        }
        let before_flush = std::time::Instant::now();
        std::io::Write::flush(&mut diagnostics).unwrap();
        assert!(before_flush.elapsed() < Duration::from_secs(1));
        assert!(!diagnostics.report("full", Duration::from_secs(1)));
        assert!(matches!(
            diagnostics.sender.try_send(Message {
                bytes: vec![],
                completed: None
            }),
            Err(mpsc::TrySendError::Full(_))
        ));
        release.send(()).unwrap();
    }
}
