use super::capture::CaptureState;
use super::file::FileProducer;
use super::stream::{OutputStream, StreamQueue};
use super::{error_description, lock};
use crate::result::CaptureIssue;
use std::io;
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::Duration;
#[cfg(unix)]
use tokio::io::Interest;
#[cfg(unix)]
use tokio::io::unix::AsyncFd;

#[cfg(unix)]
pub(super) fn nonblocking_pipe(fd: OwnedFd) -> io::Result<AsyncFd<OwnedFd>> {
    // SAFETY: fd is owned and remains valid across both fcntl calls; F_GETFL
    // and F_SETFL take no pointers. This is the read end of a dedicated child
    // pipe, not a dup of controller stdout/stderr.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same valid owned descriptor and integer-only fcntl operation.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    AsyncFd::with_interest(fd, Interest::READABLE)
}

#[cfg(unix)]
pub(super) type PipeReader = AsyncFd<OwnedFd>;
#[cfg(windows)]
pub(super) type PipeReader = OwnedHandle;

#[cfg(windows)]
pub(super) fn nonblocking_pipe(pipe: OwnedHandle) -> io::Result<PipeReader> {
    Ok(pipe)
}

pub(super) async fn read_pipe(
    pipe: PipeReader,
    state: Arc<Mutex<CaptureState>>,
    file: Option<FileProducer>,
    stream: Option<Arc<StreamQueue>>,
    kind: OutputStream,
    read_chunk: usize,
    mut quota: u64,
) {
    let mut buffer = vec![0; read_chunk];
    loop {
        let read = read_pipe_chunk(&pipe, &mut buffer).await;
        let bytes = match read {
            Ok(0) => {
                lock(&state).summary.eof = true;
                break;
            }
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                lock(&state).issue(CaptureIssue::ReadError(error_description(&error)));
                break;
            }
        };
        let bytes = &buffer[..bytes];
        lock(&state).record(bytes);
        if let Some(stream) = &stream {
            stream.push(kind, bytes);
        }
        if let Some(file) = &file {
            let accepted = bytes
                .len()
                .min(usize::try_from(quota).unwrap_or(usize::MAX));
            quota -= accepted as u64;
            if accepted < bytes.len() {
                lock(&state).issue(CaptureIssue::FileQuotaExceeded);
            }
            if !file.queue.push(&bytes[..accepted]).await {
                break;
            }
        }
        // AsyncFd can remain ready for an arbitrarily noisy process. Yield
        // explicitly so supervisor timers and cancellation always get polled.
        tokio::task::yield_now().await;
    }
    // The file producer's Drop delivers EOF on every exit path.
}

#[cfg(unix)]
async fn read_pipe_chunk(pipe: &PipeReader, buffer: &mut [u8]) -> io::Result<usize> {
    let mut read_now = |fd: &OwnedFd| {
        // SAFETY: AsyncFd owns the live pipe; buffer is writable for this size.
        let bytes = unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if bytes < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(bytes as usize)
        }
    };
    match read_now(pipe.get_ref()) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            pipe.async_io(Interest::READABLE, read_now).await
        }
        result => result,
    }
}

#[cfg(windows)]
async fn read_pipe_chunk(pipe: &PipeReader, buffer: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    loop {
        let mut available = 0;
        // SAFETY: the owned anonymous pipe remains live, no data pointer is
        // supplied and available points to writable DWORD storage. Peek reports
        // buffered bytes without consuming them or waiting for a future write.
        if unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle().cast(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            return Err(error);
        }
        if available == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        }
        let amount = buffer.len().min(available as usize).min(u32::MAX as usize) as u32;
        let mut read = 0;
        // SAFETY: this is the sole reader, so the peeked bytes remain available.
        // Read only that bounded amount into the valid writable buffer; this
        // synchronous ReadFile cannot wait for bytes not yet written.
        if unsafe {
            ReadFile(
                pipe.as_raw_handle().cast(),
                buffer.as_mut_ptr(),
                amount,
                &mut read,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                return Ok(0);
            }
            return Err(error);
        }
        return Ok(read as usize);
    }
}

#[cfg(all(test, unix))]
mod tests;
