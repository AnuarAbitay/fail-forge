use super::capture::CaptureState;
use super::{error_description, lock};
use crate::result::CaptureIssue;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::Notify;

pub(super) struct FileQueueState {
    pub(super) chunks: VecDeque<Vec<u8>>,
    pub(super) used: usize,
    pub(super) closed: bool,
}

pub(super) struct FileQueue {
    pub(super) state: Mutex<FileQueueState>,
    limit: usize,
    condition: Condvar,
    space: Notify,
}

impl FileQueue {
    pub(super) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(FileQueueState {
                chunks: VecDeque::new(),
                used: 0,
                closed: false,
            }),
            limit,
            condition: Condvar::new(),
            space: Notify::new(),
        })
    }

    pub(super) async fn push(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if bytes.len() > self.limit {
            return false;
        }
        loop {
            let notified = self.space.notified();
            {
                let mut state = lock(&self.state);
                if state.closed {
                    return false;
                }
                if bytes.len() <= self.limit.saturating_sub(state.used) {
                    state.used += bytes.len();
                    state.chunks.push_back(bytes.to_vec());
                    drop(state);
                    self.condition.notify_one();
                    return true;
                }
            }
            notified.await;
        }
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut state = lock(&self.state);
        loop {
            if let Some(bytes) = state.chunks.pop_front() {
                return Some(bytes);
            }
            if state.closed {
                return None;
            }
            state = self
                .condition
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn release(&self, bytes: usize) {
        lock(&self.state).used -= bytes;
        self.space.notify_one();
    }

    pub(super) fn close(&self, discard: bool) {
        let mut state = lock(&self.state);
        state.closed = true;
        if discard {
            while let Some(bytes) = state.chunks.pop_front() {
                state.used -= bytes.len();
            }
        }
        drop(state);
        self.space.notify_one();
        self.condition.notify_all();
    }
}

/// The reader owns the only producer. Construct this before creating its
/// future so normal return, unwind and cancellation (even before first poll)
/// all deliver queue EOF while preserving accepted file chunks for the writer.
pub(super) struct FileProducer {
    pub(super) queue: Arc<FileQueue>,
}

impl Drop for FileProducer {
    fn drop(&mut self) {
        self.queue.close(false);
    }
}

/// The only file consumer retires its queue on return or unwind. Its current
/// chunk remains charged until writing stops, including a panic in `Write`.
struct FileConsumer<'a> {
    queue: &'a FileQueue,
    state: &'a Mutex<CaptureState>,
    reserved: usize,
}

impl FileConsumer<'_> {
    fn release(&mut self) {
        self.queue.release(self.reserved);
        self.reserved = 0;
    }
}

impl Drop for FileConsumer<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            lock(self.state).issue(CaptureIssue::WriteError(
                "capture worker stopped unexpectedly".into(),
            ));
        }
        self.release();
        self.queue.close(true);
    }
}

// An uninterruptible regular-file syscall may outlive the drain budget. These
// workers are outside Tokio's blocking pool, and at most eight can exist in
// the whole process. A permit survives a timed-out session until the actual
// worker returns; repeated failed runs cannot leak an unbounded thread count.
pub(super) static FILE_WORKERS: AtomicUsize = AtomicUsize::new(0);
pub(super) const MAX_FILE_WORKERS: usize = 8;

pub(super) struct WorkerPermit;

impl WorkerPermit {
    pub(super) fn acquire() -> io::Result<Self> {
        let mut count = FILE_WORKERS.load(Ordering::Acquire);
        loop {
            if count >= MAX_FILE_WORKERS {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "capture worker limit reached",
                ));
            }
            match FILE_WORKERS.compare_exchange_weak(
                count,
                count + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self),
                Err(actual) => count = actual,
            }
        }
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        FILE_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) struct PreparedFile {
    pub(super) file: File,
    pub(super) permit: WorkerPermit,
}

pub(super) struct FileWorker {
    pub(super) queue: Arc<FileQueue>,
    pub(super) thread: thread::JoinHandle<()>,
}

pub(super) fn write_file(mut file: impl Write, queue: &FileQueue, state: &Mutex<CaptureState>) {
    let mut consumer = FileConsumer {
        queue,
        state,
        reserved: 0,
    };
    while let Some(bytes) = queue.pop() {
        consumer.reserved = bytes.len();
        let mut offset = 0;
        while offset < bytes.len() {
            match file.write(&bytes[offset..]) {
                Ok(0) => {
                    lock(state).issue(CaptureIssue::WriteError("WriteZero, OS error None".into()));
                    consumer.release();
                    queue.close(true);
                    lock(state).summary.writer_finished = true;
                    return;
                }
                Ok(written) => {
                    offset += written;
                    let mut state = lock(state);
                    match state.summary.file_bytes_written.checked_add(written as u64) {
                        Some(total) => state.summary.file_bytes_written = total,
                        None => state.issue(CaptureIssue::CounterOverflow),
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    lock(state).issue(CaptureIssue::WriteError(error_description(&error)));
                    consumer.release();
                    queue.close(true);
                    lock(state).summary.writer_finished = true;
                    return;
                }
            }
        }
        consumer.release();
    }
    // std::fs::File is unbuffered; flush does not imply fsync/durable storage.
    if let Err(error) = file.flush() {
        lock(state).issue(CaptureIssue::WriteError(error_description(&error)));
    }
    lock(state).summary.writer_finished = true;
}

#[cfg(all(test, unix))]
mod tests;
