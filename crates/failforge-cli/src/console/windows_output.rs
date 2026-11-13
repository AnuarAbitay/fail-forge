//! Deadline-bound synchronous Windows output. A worker owns each stream; its
//! retained thread handle authorizes cancellation without targeting a reused ID.
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::IO::CancelSynchronousIo;
use windows_sys::Win32::System::Threading::*;

struct Request {
    bytes: Vec<u8>,
    done: mpsc::SyncSender<io::Result<usize>>,
}
pub(crate) struct Writer {
    sender: mpsc::SyncSender<Request>,
    thread: File,
    stopped: Arc<AtomicBool>,
    timeout: Duration,
    failed: bool,
}
pub(crate) fn duplicate_stdio(stderr: bool) -> io::Result<File> {
    let source = unsafe {
        GetStdHandle(if stderr {
            STD_ERROR_HANDLE
        } else {
            STD_OUTPUT_HANDLE
        })
    };
    if source.is_null() || source == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "standard output handle is unavailable",
        ));
    }
    let mut target = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &mut target,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(target) })
}
impl Writer {
    pub(crate) fn stdio(stderr: bool, timeout: Duration) -> io::Result<Self> {
        Self::from_file(duplicate_stdio(stderr)?, timeout)
    }
    pub(crate) fn from_file(mut file: File, timeout: Duration) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Request>(1);
        let (ready, thread) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        std::thread::Builder::new()
            .name("windows-output".into())
            .spawn(move || {
                let mut handle = std::ptr::null_mut();
                let result = if unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        GetCurrentThread(),
                        GetCurrentProcess(),
                        &mut handle,
                        0,
                        0,
                        DUPLICATE_SAME_ACCESS,
                    )
                } == 0
                {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(unsafe { File::from_raw_handle(handle) })
                };
                if ready.send(result).is_err() {
                    return;
                }
                for request in receiver {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result = loop {
                        match file.write(&request.bytes) {
                            Err(error)
                                if error.kind() == io::ErrorKind::Interrupted
                                    && !stop.load(Ordering::Acquire) =>
                            {
                                continue;
                            }
                            result => break result,
                        }
                    };
                    let failed = result.as_ref().map_or(true, |count| *count == 0);
                    let _ = request.done.send(result);
                    if failed || stop.load(Ordering::Acquire) {
                        break;
                    }
                }
            })?;
        let thread = thread.recv_timeout(Duration::from_secs(2)).map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "output worker did not start")
        })??;
        Ok(Self {
            sender,
            thread,
            stopped,
            timeout,
            failed: false,
        })
    }
    fn cancel(&self, done: Option<&mpsc::Receiver<io::Result<usize>>>) {
        self.stopped.store(true, Ordering::Release);
        // Repeated cancellation covers the gap between the worker checking its
        // stop flag and entering WriteFile. Never join an unresponsive driver.
        let deadline = Instant::now() + Duration::from_millis(50);
        loop {
            unsafe {
                CancelSynchronousIo(self.thread.as_raw_handle());
            }
            if done.is_some_and(|receiver| {
                !matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty))
            }) || Instant::now() >= deadline
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn once(&mut self, bytes: &[u8], timeout: Duration) -> io::Result<usize> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "output worker is unavailable",
            ));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let (done, receive) = mpsc::sync_channel(1);
        let request = Request {
            bytes: bytes[..bytes.len().min(64 * 1024)].to_vec(),
            done,
        };
        if self.sender.try_send(request).is_err() {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "output queue is unavailable",
            ));
        }
        match receive.recv_timeout(timeout) {
            Ok(Ok(0)) => {
                self.failed = true;
                Err(io::ErrorKind::WriteZero.into())
            }
            Ok(Ok(count)) => Ok(count),
            Ok(Err(error)) => {
                self.failed = true;
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.failed = true;
                Err(io::ErrorKind::BrokenPipe.into())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.failed = true;
                self.cancel(Some(&receive));
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "output write did not drain before its deadline",
                ))
            }
        }
    }
    pub(crate) fn write_all_with_timeout(
        &mut self,
        mut bytes: &[u8],
        timeout: Duration,
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        while !bytes.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.failed = true;
                self.cancel(None);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "output frame write timed out",
                ));
            }
            let count = self.once(bytes, remaining)?;
            bytes = &bytes[count..];
        }
        Ok(())
    }
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.once(bytes, self.timeout)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            unsafe {
                CancelSynchronousIo(self.thread.as_raw_handle());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use windows_sys::Win32::System::Pipes::CreatePipe;
    fn pipe() -> (File, File) {
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 4096) },
            0
        );
        unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) }
    }
    #[test]
    fn binary_output_is_exact_and_closed_peer_is_reported() {
        let (mut read, write) = pipe();
        let mut writer = Writer::from_file(write, Duration::from_secs(1)).unwrap();
        writer.write_all(&[0, 255, 128, 10]).unwrap();
        let mut actual = [0; 4];
        read.read_exact(&mut actual).unwrap();
        assert_eq!(actual, [0, 255, 128, 10]);
        drop(read);
        assert!(writer.write_all(b"closed").is_err());
    }
    #[test]
    fn full_pipe_has_bounded_deadline_and_permanently_poisoned_writer() {
        let (_read, write) = pipe();
        let mut writer = Writer::from_file(write, Duration::from_millis(20)).unwrap();
        let started = Instant::now();
        assert_eq!(
            writer
                .write_all_with_timeout(&vec![b'x'; 1024 * 1024], Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(writer.write_all(b"later").is_err());
        let drop_started = Instant::now();
        drop(writer);
        assert!(drop_started.elapsed() < Duration::from_millis(100));
    }
}
