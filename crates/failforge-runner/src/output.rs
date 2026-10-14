//! Byte-oriented output capture. Readers never decode child output.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::result::{CaptureIssue, CaptureSummary};
use crate::spec::{CapturePolicy, OutputMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The reservation includes the consumer's current chunk. A slow consumer
/// cannot evade the shared stdout/stderr byte budget merely by dequeuing.
pub struct StreamChunk {
    /// Display routing label. Mutating it does not change capture accounting.
    pub stream: OutputStream,
    accounting_stream: OutputStream,
    bytes: Vec<u8>,
    displayed: usize,
    queue: Arc<StreamQueue>,
}

impl StreamChunk {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Acknowledge bytes actually written, including partial sink writes.
    /// Over-acknowledgement is clamped to the remaining bytes of this chunk.
    pub fn mark_displayed(&mut self, bytes: usize) {
        let count = bytes.min(self.bytes.len() - self.displayed);
        self.displayed += count;
        let mut state = lock(&self.queue.state);
        state.unconfirmed[self.accounting_stream.index()] -= count as u64;
    }

    /// Report a failed write of this chunk. The source is the original child
    /// stream, even if the public display routing label was changed.
    pub fn report_error(&self, error: io::Error) {
        self.queue.report_error(self.accounting_stream, error);
    }
}

impl Drop for StreamChunk {
    fn drop(&mut self) {
        let mut state = lock(&self.queue.state);
        let index = self.accounting_stream.index();
        let lost = (self.bytes.len() - self.displayed) as u64;
        state.used -= self.bytes.len();
        state.unconfirmed[index] -= lost;
        state.record_dropped(self.accounting_stream, lost);
    }
}

/// Optional raw display output. Dropping this receiver disables display and
/// accounts queued bytes as dropped; it never disables raw capture.
pub struct StreamReceiver {
    queue: Arc<StreamQueue>,
}

impl StreamReceiver {
    /// Bytes lost by the bounded stream queue, in stdout/stderr order.
    /// Protocol consumers must reject a response if its stream has lost bytes.
    pub fn dropped_bytes(&self) -> (u64, u64) {
        let state = lock(&self.queue.state);
        (state.dropped[0], state.dropped[1])
    }

    pub async fn recv(&mut self) -> Option<StreamChunk> {
        loop {
            let notified = self.queue.ready.notified();
            // Pop and EOF inspection are one decision. A producer cannot
            // publish its final chunk and close between two separate locks.
            let next = {
                let mut state = lock(&self.queue.state);
                match state.chunks.pop_front() {
                    Some((stream, bytes)) => Some(Some(StreamChunk {
                        stream,
                        accounting_stream: stream,
                        bytes,
                        displayed: 0,
                        queue: self.queue.clone(),
                    })),
                    None if state.closed => Some(None),
                    None => None,
                }
            };
            match next {
                Some(Some(chunk)) => {
                    return Some(chunk);
                }
                Some(None) => return None,
                None => notified.await,
            }
        }
    }

    /// For a dedicated display thread, outside Tokio's blocking pool.
    pub fn blocking_recv(&mut self) -> Option<StreamChunk> {
        let mut state = lock(&self.queue.state);
        loop {
            if let Some((stream, bytes)) = state.chunks.pop_front() {
                return Some(StreamChunk {
                    stream,
                    accounting_stream: stream,
                    bytes,
                    displayed: 0,
                    queue: self.queue.clone(),
                });
            }
            if state.closed {
                return None;
            }
            state = self
                .queue
                .condition
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Report a sink error without a chunk, identifying the affected child
    /// stream explicitly. For a failed chunk write, prefer its `report_error`.
    /// No argument or environment data is included in the stored error.
    pub fn report_error(&self, stream: OutputStream, error: io::Error) {
        self.queue.report_error(stream, error);
    }
}

impl Drop for StreamReceiver {
    fn drop(&mut self) {
        lock(&self.queue.state).receiver_alive = false;
        self.queue.close(true);
    }
}

struct StreamQueueState {
    chunks: VecDeque<(OutputStream, Vec<u8>)>,
    used: usize,
    dropped: [u64; 2],
    unconfirmed: [u64; 2],
    closed: bool,
    finalized: bool,
    receiver_alive: bool,
    error: Option<(OutputStream, CaptureIssue)>,
}

impl StreamQueueState {
    fn record_dropped(&mut self, stream: OutputStream, bytes: u64) {
        let index = stream.index();
        match self.dropped[index].checked_add(bytes) {
            Some(total) => self.dropped[index] = total,
            None => {
                self.dropped[index] = u64::MAX;
                if self.error.is_none() {
                    self.error = Some((stream, CaptureIssue::CounterOverflow));
                }
            }
        }
    }
}

struct StreamQueue {
    state: Mutex<StreamQueueState>,
    limit: usize,
    ready: Notify,
    condition: Condvar,
}

impl StreamQueue {
    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(StreamQueueState {
                chunks: VecDeque::new(),
                used: 0,
                dropped: [0, 0],
                unconfirmed: [0, 0],
                closed: false,
                finalized: false,
                receiver_alive: true,
                error: None,
            }),
            limit,
            ready: Notify::new(),
            condition: Condvar::new(),
        })
    }

    fn push(&self, stream: OutputStream, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut state = lock(&self.state);
        let index = stream.index();
        if state.closed || bytes.len() > self.limit.saturating_sub(state.used) {
            state.record_dropped(stream, bytes.len() as u64);
            return;
        }
        state.used += bytes.len();
        state.unconfirmed[index] = state.unconfirmed[index].saturating_add(bytes.len() as u64);
        state.chunks.push_back((stream, bytes.to_vec()));
        drop(state);
        self.ready.notify_one();
        self.condition.notify_one();
    }

    fn report_error(&self, stream: OutputStream, error: io::Error) {
        let mut state = lock(&self.state);
        if !state.finalized && state.error.is_none() {
            state.error = Some((stream, CaptureIssue::StreamError(error_description(&error))));
        }
        drop(state);
        self.close(true);
    }

    #[cfg(test)]
    fn pop(self: &Arc<Self>) -> Option<StreamChunk> {
        lock(&self.state)
            .chunks
            .pop_front()
            .map(|(stream, bytes)| StreamChunk {
                stream,
                accounting_stream: stream,
                bytes,
                displayed: 0,
                queue: self.clone(),
            })
    }

    fn close(&self, discard: bool) {
        let mut state = lock(&self.state);
        state.closed = true;
        if discard {
            while let Some((stream, bytes)) = state.chunks.pop_front() {
                let index = stream.index();
                state.used -= bytes.len();
                state.unconfirmed[index] -= bytes.len() as u64;
                state.record_dropped(stream, bytes.len() as u64);
            }
        }
        drop(state);
        self.ready.notify_one();
        self.condition.notify_all();
    }
}

struct CaptureState {
    summary: CaptureSummary,
    tail: TailBuffer,
}

impl CaptureState {
    fn record(&mut self, bytes: &[u8]) {
        match self.summary.bytes_read.checked_add(bytes.len() as u64) {
            Some(total) => self.summary.bytes_read = total,
            None => {
                self.summary.bytes_read = u64::MAX;
                self.issue(CaptureIssue::CounterOverflow);
            }
        }
        self.tail.push(bytes);
    }

    fn issue(&mut self, issue: CaptureIssue) {
        if self.summary.issue.is_none() {
            self.summary.issue = Some(issue);
        }
    }

    fn snapshot(&self) -> CaptureSummary {
        let mut summary = self.summary.clone();
        summary.tail = self.tail.snapshot();
        summary.tail_truncated = self.tail.truncated;
        summary
    }
}

struct FileQueueState {
    chunks: VecDeque<Vec<u8>>,
    used: usize,
    closed: bool,
}

struct FileQueue {
    state: Mutex<FileQueueState>,
    limit: usize,
    condition: Condvar,
    space: Notify,
}

impl FileQueue {
    fn new(limit: usize) -> Arc<Self> {
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

    async fn push(&self, bytes: &[u8]) -> bool {
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

    fn close(&self, discard: bool) {
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
struct FileProducer {
    queue: Arc<FileQueue>,
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
static FILE_WORKERS: AtomicUsize = AtomicUsize::new(0);
const MAX_FILE_WORKERS: usize = 8;

struct WorkerPermit;

impl WorkerPermit {
    fn acquire() -> io::Result<Self> {
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

struct PreparedFile {
    file: File,
    permit: WorkerPermit,
}

struct FileWorker {
    queue: Arc<FileQueue>,
    thread: thread::JoinHandle<()>,
}

pub(crate) struct OutputSession {
    state: [Arc<Mutex<CaptureState>>; 2],
    prepared: [Option<PreparedFile>; 2],
    workers: [Option<FileWorker>; 2],
    readers: [Option<JoinHandle<()>>; 2],
    stream: Option<Arc<StreamQueue>>,
    read_chunk_bytes: usize,
    file_quota: u64,
    started: bool,
    final_snapshot: Option<(CaptureSummary, CaptureSummary)>,
}

/// A bounded view usable by the supervisor's unwind/abort guard. It owns no
/// child, reader task, file descriptor, or background-worker join handle.
pub(crate) struct OutputSnapshot {
    state: [Arc<Mutex<CaptureState>>; 2],
    stream: Option<Arc<StreamQueue>>,
}

impl OutputSnapshot {
    pub(crate) fn capture(&self) -> (CaptureSummary, CaptureSummary) {
        let mut stdout = lock(&self.state[0]).snapshot();
        let mut stderr = lock(&self.state[1]).snapshot();
        if let Some(stream) = &self.stream {
            let state = lock(&stream.state);
            for (index, summary) in [&mut stdout, &mut stderr].into_iter().enumerate() {
                summary.stream_dropped_bytes = state.dropped[index];
                summary.stream_unconfirmed_bytes = state.unconfirmed[index];
                if let Some((_, issue)) = state
                    .error
                    .as_ref()
                    .filter(|(kind, _)| kind.index() == index && summary.issue.is_none())
                {
                    summary.issue = Some(issue.clone());
                }
            }
        }
        (stdout, stderr)
    }
}

impl OutputSession {
    pub(crate) fn snapshotter(&self) -> OutputSnapshot {
        OutputSnapshot {
            state: self.state.clone(),
            stream: self.stream.clone(),
        }
    }
    pub(crate) fn prepare(policy: &CapturePolicy, invocation_cwd: &Path) -> io::Result<Self> {
        if policy.read_chunk_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero read chunk",
            ));
        }
        let state = std::array::from_fn(|_| {
            Arc::new(Mutex::new(CaptureState {
                summary: CaptureSummary {
                    writer_finished: policy.capture_dir.is_none(),
                    ..CaptureSummary::default()
                },
                tail: TailBuffer::new(policy.tail_bytes_per_stream),
            }))
        });
        let mut prepared = [None, None];
        if let Some(path) = &policy.capture_dir {
            let dir = invocation_cwd.join(path);
            let stdout_permit = WorkerPermit::acquire()?;
            let stderr_permit = WorkerPermit::acquire()?;
            std::fs::create_dir_all(&dir)?;
            for (index, (name, permit)) in
                [("stdout.bin", stdout_permit), ("stderr.bin", stderr_permit)]
                    .into_iter()
                    .enumerate()
            {
                let path = dir.join(name);
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                lock(&state[index]).summary.file_path = Some(path);
                prepared[index] = Some(PreparedFile { file, permit });
            }
        }
        Ok(Self {
            state,
            prepared,
            workers: [None, None],
            readers: [None, None],
            stream: (policy.mode == OutputMode::StreamAndCapture)
                .then(|| StreamQueue::new(policy.stream_queue_bytes)),
            read_chunk_bytes: policy.read_chunk_bytes,
            file_quota: policy.file_quota_bytes_per_stream,
            started: false,
            final_snapshot: None,
        })
    }

    pub(crate) fn begin(
        &mut self,
        stdout: OwnedFd,
        stderr: OwnedFd,
    ) -> io::Result<Option<StreamReceiver>> {
        if self.started {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "output already started",
            ));
        }
        self.started = true;
        let stdout = nonblocking_pipe(stdout).inspect_err(|error| {
            lock(&self.state[0]).issue(CaptureIssue::ReadError(error_description(error)));
        })?;
        let stderr = nonblocking_pipe(stderr).inspect_err(|error| {
            lock(&self.state[1]).issue(CaptureIssue::ReadError(error_description(error)));
        })?;
        let pipes = [stdout, stderr];
        for (index, pipe) in pipes.into_iter().enumerate() {
            let file_queue = if let Some(prepared) = self.prepared[index].take() {
                let queue = FileQueue::new(self.read_chunk_bytes.saturating_mul(2));
                let worker_queue = queue.clone();
                let state = self.state[index].clone();
                let handle = thread::Builder::new()
                    .name("failforge-capture".into())
                    .spawn(move || {
                        let _permit = prepared.permit;
                        write_file(prepared.file, &worker_queue, &state);
                    })
                    .inspect_err(|error| {
                        let mut state = lock(&self.state[index]);
                        state.issue(CaptureIssue::WriteError(error_description(error)));
                        state.summary.writer_finished = true;
                    })?;
                self.workers[index] = Some(FileWorker {
                    queue: queue.clone(),
                    thread: handle,
                });
                Some(queue)
            } else {
                None
            };
            let state = self.state[index].clone();
            let stream = self.stream.clone();
            let read_chunk = self.read_chunk_bytes;
            let quota = self.file_quota;
            let kind = if index == 0 {
                OutputStream::Stdout
            } else {
                OutputStream::Stderr
            };
            self.readers[index] = Some(tokio::spawn(read_pipe(
                pipe,
                state,
                file_queue.map(|queue| FileProducer { queue }),
                stream,
                kind,
                read_chunk,
                quota,
            )));
        }
        Ok(self.stream.as_ref().map(|queue| StreamReceiver {
            queue: queue.clone(),
        }))
    }

    pub(crate) fn failure(&self) -> Option<CaptureIssue> {
        for index in 0..2 {
            let mut state = lock(&self.state[index]);
            if self.readers[index]
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
                && !state.summary.eof
                && state.summary.issue.is_none()
            {
                state.issue(CaptureIssue::ReadError(
                    "output reader stopped unexpectedly".into(),
                ));
            }
            if self.workers[index]
                .as_ref()
                .is_some_and(|worker| worker.thread.is_finished())
                && !state.summary.writer_finished
                && state.summary.issue.is_none()
            {
                state.issue(CaptureIssue::WriteError(
                    "capture worker stopped unexpectedly".into(),
                ));
            }
            if let Some(issue) = state.summary.issue.clone() {
                return Some(issue);
            }
        }
        self.stream
            .as_ref()
            .and_then(|queue| lock(&queue.state).error.clone().map(|(_, issue)| issue))
    }

    pub(crate) async fn finish(&mut self, budget: Duration) -> (CaptureSummary, CaptureSummary) {
        if let Some(snapshot) = &self.final_snapshot {
            return snapshot.clone();
        }
        let deadline = tokio::time::Instant::now().checked_add(budget);
        loop {
            let readers_done = self
                .readers
                .iter()
                .all(|reader| reader.as_ref().is_none_or(JoinHandle::is_finished));
            if let Some(stream) = self.stream.as_ref().filter(|_| readers_done) {
                stream.close(false);
            }
            let workers_done = self.workers.iter().all(|worker| {
                worker
                    .as_ref()
                    .is_none_or(|worker| worker.thread.is_finished())
            });
            let stream_done = self.stream.as_ref().is_none_or(|stream| {
                let state = lock(&stream.state);
                state.used == 0 || !state.receiver_alive
            });
            if readers_done && workers_done && stream_done {
                break;
            }
            if deadline.is_none_or(|deadline| tokio::time::Instant::now() >= deadline) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        for index in 0..2 {
            if let Some(reader) = self.readers[index].take() {
                if !reader.is_finished() {
                    reader.abort();
                    lock(&self.state[index]).issue(CaptureIssue::DrainExpired);
                }
                match reader.await {
                    Err(error) if !error.is_cancelled() => {
                        lock(&self.state[index]).issue(CaptureIssue::ReadError(
                            "output reader stopped unexpectedly".into(),
                        ));
                    }
                    _ => {}
                }
            }
            if let Some(worker) = self.workers[index].take() {
                worker.queue.close(true);
                if worker.thread.is_finished() {
                    if worker.thread.join().is_err() {
                        lock(&self.state[index]).issue(CaptureIssue::WriteError(
                            "capture worker stopped unexpectedly".into(),
                        ));
                    }
                } else {
                    lock(&self.state[index]).issue(CaptureIssue::DrainExpired);
                    // Dropping a std JoinHandle detaches it. Its WorkerPermit
                    // remains owned by the actual thread until it returns.
                }
            }
            if self.prepared[index].take().is_some() || !self.started {
                lock(&self.state[index]).summary.writer_finished = true;
            }
        }
        if let Some(stream) = &self.stream {
            stream.close(true);
            lock(&stream.state).finalized = true;
        }
        let (stdout, stderr) = self.snapshotter().capture();
        self.final_snapshot = Some((stdout.clone(), stderr.clone()));
        (stdout, stderr)
    }
}

impl Drop for OutputSession {
    fn drop(&mut self) {
        for reader in self.readers.iter().flatten() {
            reader.abort();
        }
        for worker in self.workers.iter().flatten() {
            worker.queue.close(true);
        }
        if let Some(stream) = &self.stream {
            stream.close(true);
        }
    }
}

fn error_description(error: &io::Error) -> String {
    format!("{:?}, OS error {:?}", error.kind(), error.raw_os_error())
}

fn nonblocking_pipe(fd: OwnedFd) -> io::Result<AsyncFd<OwnedFd>> {
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

async fn read_pipe(
    pipe: AsyncFd<OwnedFd>,
    state: Arc<Mutex<CaptureState>>,
    file: Option<FileProducer>,
    stream: Option<Arc<StreamQueue>>,
    kind: OutputStream,
    read_chunk: usize,
    mut quota: u64,
) {
    let mut buffer = vec![0; read_chunk];
    loop {
        let read = pipe
            .async_io(Interest::READABLE, |fd| {
                // SAFETY: fd remains owned by AsyncFd; buffer is writable for
                // its entire declared length, and read never stores pointers.
                let bytes =
                    unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
                if bytes < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(bytes as usize)
                }
            })
            .await;
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

fn write_file(mut file: impl Write, queue: &FileQueue, state: &Mutex<CaptureState>) {
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

/// The retained suffix of one child stream. The byte limit is independent of
/// the number or shape of reads, including a read larger than the entire tail.
struct TailBuffer {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl TailBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            limit,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if bytes.len() >= self.limit {
            self.truncated |= !self.bytes.is_empty() || bytes.len() > self.limit;
            self.bytes.clear();
            self.bytes.extend(&bytes[bytes.len() - self.limit..]);
            return;
        }
        let remove = self
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(self.limit);
        self.truncated |= remove != 0;
        self.bytes.drain(..remove);
        self.bytes.extend(bytes);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;

    static CAPTURE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "failforge-output-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        // SAFETY: fds points to storage for the two descriptors pipe writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: a successful pipe returned two fresh descriptors, and these
        // OwnedFd values are their only owners.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    fn ended_pipe(bytes: &[u8]) -> OwnedFd {
        let (read, write) = pipe();
        let mut file = File::from(write);
        file.write_all(bytes).unwrap();
        drop(file);
        read
    }

    fn state(limit: usize) -> Arc<Mutex<CaptureState>> {
        Arc::new(Mutex::new(CaptureState {
            summary: CaptureSummary::default(),
            tail: TailBuffer::new(limit),
        }))
    }

    fn stream_receiver(queue: &Arc<StreamQueue>) -> StreamReceiver {
        StreamReceiver {
            queue: queue.clone(),
        }
    }

    #[test]
    fn empty_tail_has_no_truncation() {
        for limit in [0, 1, 32_768] {
            let mut tail = TailBuffer::new(limit);
            tail.push(&[]);
            assert!(tail.snapshot().is_empty());
            assert!(!tail.truncated);
        }
    }

    #[test]
    fn zero_tail_retains_nothing_and_reports_real_output() {
        let mut tail = TailBuffer::new(0);
        tail.push(b"abc");
        assert!(tail.snapshot().is_empty());
        assert!(tail.truncated);
    }

    #[test]
    fn tail_preserves_raw_bytes_at_exact_boundary() {
        let mut tail = TailBuffer::new(4);
        tail.push(&[0, 0xff]);
        tail.push(&[b'\n', 0x80]);
        assert_eq!(tail.snapshot(), [0, 0xff, b'\n', 0x80]);
        assert!(!tail.truncated);
    }

    #[test]
    fn oversized_read_keeps_only_its_suffix() {
        let mut tail = TailBuffer::new(3);
        tail.push(b"a");
        tail.push(b"0123456789");
        assert_eq!(tail.snapshot(), b"789");
        assert!(tail.truncated);
    }

    #[test]
    fn replacing_full_tail_marks_prior_bytes_lost() {
        let mut tail = TailBuffer::new(3);
        tail.push(b"abc");
        assert!(!tail.truncated);
        tail.push(b"def");
        assert_eq!(tail.snapshot(), b"def");
        assert!(tail.truncated);
    }

    #[test]
    fn every_read_partition_produces_the_same_bounded_suffix() {
        let bytes: Vec<u8> = (0..10_000).map(|index| (index % 256) as u8).collect();
        for limit in [0, 1, 7, 256, 10_000, 10_001] {
            for read_size in [1, 3, 257, 4096, 10_000] {
                let mut tail = TailBuffer::new(limit);
                for read in bytes.chunks(read_size) {
                    tail.push(read);
                    assert!(tail.bytes.len() <= limit);
                }
                assert_eq!(tail.snapshot(), bytes[bytes.len().saturating_sub(limit)..]);
                assert_eq!(tail.truncated, bytes.len() > limit);
            }
        }
    }

    #[test]
    fn capture_state_counts_all_raw_bytes_and_preserves_first_issue() {
        let mut state = CaptureState {
            summary: CaptureSummary::default(),
            tail: TailBuffer::new(3),
        };
        state.record(&[0, 255, 128]);
        state.record(b"xy");
        state.issue(CaptureIssue::FileQuotaExceeded);
        state.issue(CaptureIssue::DrainExpired);
        let summary = state.snapshot();
        assert_eq!(summary.bytes_read, 5);
        assert_eq!(summary.tail, [128, b'x', b'y']);
        assert!(summary.tail_truncated);
        assert_eq!(summary.issue, Some(CaptureIssue::FileQuotaExceeded));
    }

    #[test]
    fn capture_counter_overflow_is_explicit() {
        let state = state(2);
        lock(&state).summary.bytes_read = u64::MAX;
        lock(&state).record(b"x");
        assert_eq!(lock(&state).summary.bytes_read, u64::MAX);
        assert_eq!(
            lock(&state).summary.issue,
            Some(CaptureIssue::CounterOverflow)
        );
    }

    #[test]
    fn stream_budget_is_shared_and_includes_inflight_chunks() {
        let queue = StreamQueue::new(6);
        queue.push(OutputStream::Stdout, b"abcdef");
        let mut chunk = queue.pop().unwrap();
        assert_eq!(chunk.stream, OutputStream::Stdout);
        assert_eq!(chunk.bytes(), b"abcdef");
        queue.push(OutputStream::Stderr, b"xy");
        assert!(queue.pop().is_none());
        assert_eq!(lock(&queue.state).dropped, [0, 2]);
        chunk.mark_displayed(2);
        assert_eq!(lock(&queue.state).unconfirmed, [4, 0]);
        queue.push(OutputStream::Stderr, b"z");
        assert_eq!(lock(&queue.state).dropped, [0, 3]);
        drop(chunk);
        assert_eq!(lock(&queue.state).used, 0);
        assert_eq!(lock(&queue.state).dropped, [4, 3]);
        assert_eq!(lock(&queue.state).unconfirmed, [0, 0]);
        queue.push(OutputStream::Stderr, &[255, 0]);
        let mut chunk = queue.pop().unwrap();
        chunk.mark_displayed(usize::MAX);
        chunk.mark_displayed(1);
        drop(chunk);
        assert_eq!(lock(&queue.state).dropped, [4, 3]);
    }

    #[test]
    fn changing_the_public_display_label_cannot_corrupt_source_byte_accounting() {
        let queue = StreamQueue::new(4);
        queue.push(OutputStream::Stdout, b"abcd");
        let mut chunk = queue.pop().unwrap();
        chunk.stream = OutputStream::Stderr;
        chunk.mark_displayed(2);
        assert_eq!(lock(&queue.state).unconfirmed, [2, 0]);
        drop(chunk);
        assert_eq!(lock(&queue.state).unconfirmed, [0, 0]);
        assert_eq!(lock(&queue.state).dropped, [2, 0]);
        assert_eq!(lock(&queue.state).used, 0);
        assert!(lock(&queue.state).error.is_none());
        queue.push(OutputStream::Stderr, b"next");
        let mut next = queue.pop().unwrap();
        next.mark_displayed(4);
        drop(next);
        assert_eq!(lock(&queue.state).dropped, [2, 0]);
    }

    #[test]
    fn receiver_loss_snapshot_observes_queue_and_unacknowledged_chunk_losses() {
        let queue = StreamQueue::new(4);
        let receiver = StreamReceiver {
            queue: queue.clone(),
        };
        assert_eq!(receiver.dropped_bytes(), (0, 0));
        queue.push(OutputStream::Stdout, b"abcd");
        queue.push(OutputStream::Stderr, b"x");
        assert_eq!(receiver.dropped_bytes(), (0, 1));
        let mut chunk = queue.pop().unwrap();
        chunk.mark_displayed(2);
        drop(chunk);
        assert_eq!(receiver.dropped_bytes(), (2, 1));
    }

    #[test]
    fn stream_zero_budget_and_oversized_chunks_are_accounted() {
        for limit in [0, 1, 2] {
            let queue = StreamQueue::new(limit);
            queue.push(OutputStream::Stdout, &[]);
            queue.push(OutputStream::Stderr, &[0, 255, 10]);
            assert_eq!(lock(&queue.state).used, 0);
            assert_eq!(lock(&queue.state).dropped, [0, 3]);
            assert!(queue.pop().is_none());
        }
    }

    #[test]
    fn stream_dropped_counter_overflow_is_explicit() {
        let queue = StreamQueue::new(0);
        lock(&queue.state).dropped[1] = u64::MAX;
        queue.push(OutputStream::Stderr, b"x");
        assert_eq!(lock(&queue.state).dropped[1], u64::MAX);
        assert_eq!(
            lock(&queue.state).error,
            Some((OutputStream::Stderr, CaptureIssue::CounterOverflow))
        );
    }

    #[test]
    fn stream_close_preserves_or_discards_queued_bytes_explicitly() {
        let queue = StreamQueue::new(8);
        queue.push(OutputStream::Stdout, b"abc");
        queue.push(OutputStream::Stderr, b"def");
        queue.close(false);
        assert_eq!(queue.pop().unwrap().bytes(), b"abc");
        queue.close(true);
        queue.close(true);
        queue.push(OutputStream::Stdout, b"x");
        assert_eq!(lock(&queue.state).dropped, [4, 3]);
        assert_eq!(lock(&queue.state).used, 0);
        assert!(queue.pop().is_none());
    }

    #[tokio::test]
    async fn async_stream_receive_and_receiver_drop_account_every_byte() {
        let queue = StreamQueue::new(20);
        let mut receiver = stream_receiver(&queue);
        let producer = queue.clone();
        tokio::spawn(async move {
            producer.push(OutputStream::Stderr, &[255, 0, 128]);
            producer.close(false);
        });
        let mut chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chunk.stream, OutputStream::Stderr);
        chunk.mark_displayed(1);
        drop(chunk);
        assert!(receiver.recv().await.is_none());
        assert_eq!(lock(&queue.state).dropped, [0, 2]);

        let queue = StreamQueue::new(20);
        let receiver = stream_receiver(&queue);
        queue.push(OutputStream::Stdout, b"abc");
        drop(receiver);
        queue.push(OutputStream::Stderr, b"de");
        assert_eq!(lock(&queue.state).dropped, [3, 2]);
        assert_eq!(lock(&queue.state).used, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_the_async_stream_delivers_all_final_queued_chunks_before_eof() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for _ in 0..128 {
                let queue = StreamQueue::new(64);
                let mut receiver = stream_receiver(&queue);
                let producer = queue.clone();
                let task = tokio::spawn(async move {
                    producer.push(OutputStream::Stdout, b"first");
                    tokio::task::yield_now().await;
                    producer.push(OutputStream::Stderr, b"final");
                    producer.close(false);
                });
                let mut received = Vec::new();
                while let Some(mut chunk) = receiver.recv().await {
                    received.push((chunk.stream, chunk.bytes().to_vec()));
                    chunk.mark_displayed(chunk.bytes().len());
                }
                task.await.unwrap();
                assert_eq!(
                    received,
                    vec![
                        (OutputStream::Stdout, b"first".to_vec()),
                        (OutputStream::Stderr, b"final".to_vec()),
                    ]
                );
                assert_eq!(lock(&queue.state).dropped, [0, 0]);
                assert_eq!(lock(&queue.state).used, 0);
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn blocking_stream_receiver_wakes_on_bytes_and_close() {
        let queue = StreamQueue::new(20);
        let mut receiver = stream_receiver(&queue);
        let worker = thread::spawn(move || {
            let mut chunk = receiver.blocking_recv().unwrap();
            assert_eq!(chunk.bytes(), b"bytes");
            chunk.mark_displayed(chunk.bytes().len());
            drop(chunk);
            assert!(receiver.blocking_recv().is_none());
        });
        queue.push(OutputStream::Stdout, b"bytes");
        queue.close(false);
        worker.join().unwrap();
        assert_eq!(lock(&queue.state).dropped, [0, 0]);
        assert_eq!(lock(&queue.state).unconfirmed, [0, 0]);
    }

    #[tokio::test]
    async fn sink_error_identifies_stream_and_omits_secret_text() {
        let queue = StreamQueue::new(20);
        let mut receiver = stream_receiver(&queue);
        queue.push(OutputStream::Stderr, b"abc");
        let chunk = receiver.recv().await.unwrap();
        chunk.report_error(io::Error::new(io::ErrorKind::BrokenPipe, "SECRET_ENV"));
        receiver.report_error(OutputStream::Stdout, io::Error::other("second failure"));
        assert!(matches!(
            lock(&queue.state).error,
            Some((OutputStream::Stderr, CaptureIssue::StreamError(_)))
        ));
        assert!(!format!("{:?}", lock(&queue.state).error).contains("SECRET_ENV"));
        drop(chunk);
        assert_eq!(lock(&queue.state).dropped, [0, 3]);
        assert!(receiver.recv().await.is_none());
    }

    // Regression: a later receive must not change the failed chunk's source.
    #[tokio::test]
    async fn adversarial_sink_error_belongs_to_failed_chunk_with_two_inflight_chunks() {
        struct BrokenSink;
        impl Write for BrokenSink {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let queue = StreamQueue::new(6);
        let mut receiver = stream_receiver(&queue);
        queue.push(OutputStream::Stdout, b"out");
        queue.push(OutputStream::Stderr, b"err");
        let first = receiver.recv().await.unwrap();
        let second = receiver.recv().await.unwrap();
        assert_eq!(first.stream, OutputStream::Stdout);
        assert_eq!(second.stream, OutputStream::Stderr);
        assert_eq!(lock(&queue.state).used, 6);

        let error = BrokenSink.write_all(first.bytes()).unwrap_err();
        first.report_error(error);
        let snapshot = OutputSnapshot {
            state: [state(0), state(0)],
            stream: Some(queue.clone()),
        };
        let (stdout, stderr) = snapshot.capture();
        assert!(
            matches!(stdout.issue, Some(CaptureIssue::StreamError(_))),
            "stdout sink failed, but issues are stdout={:?}, stderr={:?}",
            stdout.issue,
            stderr.issue
        );
        assert!(stderr.issue.is_none());
    }

    #[tokio::test]
    async fn chunk_errors_keep_original_source_in_both_orders_and_preserve_first_error() {
        for (first_source, second_source) in [
            (OutputStream::Stdout, OutputStream::Stderr),
            (OutputStream::Stderr, OutputStream::Stdout),
        ] {
            let queue = StreamQueue::new(6);
            let mut receiver = stream_receiver(&queue);
            queue.push(first_source, b"one");
            queue.push(second_source, b"two");
            let mut first = receiver.recv().await.unwrap();
            let second = receiver.blocking_recv().unwrap();
            first.stream = second_source;
            first.mark_displayed(1);
            first.report_error(io::Error::new(io::ErrorKind::BrokenPipe, "SECRET_FIRST"));
            let original = lock(&queue.state).error.clone().unwrap();
            second.report_error(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "SECRET_SECOND",
            ));
            receiver.report_error(second_source, io::Error::other("SECRET_THIRD"));
            assert_eq!(lock(&queue.state).error.as_ref(), Some(&original));
            assert_eq!(original.0, first_source);
            assert!(
                matches!(&original.1, CaptureIssue::StreamError(text) if text.contains("BrokenPipe") && !text.contains("SECRET"))
            );
            drop(first);
            drop(second);
            let state = lock(&queue.state);
            assert_eq!(state.used, 0);
            assert_eq!(state.unconfirmed, [0, 0]);
            assert_eq!(state.dropped[first_source.index()], 2);
            assert_eq!(state.dropped[second_source.index()], 3);
        }
    }

    #[test]
    fn receiver_error_without_a_chunk_uses_explicit_source_and_finalization_cutoff() {
        let queue = StreamQueue::new(6);
        let receiver = stream_receiver(&queue);
        receiver.report_error(
            OutputStream::Stderr,
            io::Error::new(io::ErrorKind::BrokenPipe, "SECRET_ENV"),
        );
        let snapshot = OutputSnapshot {
            state: [state(0), state(0)],
            stream: Some(queue.clone()),
        };
        let (stdout, stderr) = snapshot.capture();
        assert!(stdout.issue.is_none());
        assert!(
            matches!(&stderr.issue, Some(CaptureIssue::StreamError(text)) if text.contains("BrokenPipe") && !text.contains("SECRET_ENV"))
        );
        let original = lock(&queue.state).error.clone();
        lock(&queue.state).finalized = true;
        receiver.report_error(OutputStream::Stdout, io::Error::other("late failure"));
        assert_eq!(lock(&queue.state).error, original);

        let already_finalized = StreamQueue::new(6);
        lock(&already_finalized.state).finalized = true;
        stream_receiver(&already_finalized)
            .report_error(OutputStream::Stderr, io::Error::other("late failure"));
        assert!(lock(&already_finalized.state).error.is_none());
    }

    #[tokio::test]
    async fn file_queue_budget_includes_worker_chunk_and_cancellation_is_safe() {
        let queue = FileQueue::new(3);
        assert!(queue.push(b"abc").await);
        let chunk = queue.pop().unwrap();
        assert_eq!(chunk, b"abc");
        assert_eq!(lock(&queue.state).used, 3);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), queue.push(b"x"))
                .await
                .is_err()
        );
        assert_eq!(lock(&queue.state).used, 3);
        queue.release(chunk.len());
        assert!(queue.push(b"xy").await);
        queue.close(true);
        queue.close(true);
        assert_eq!(lock(&queue.state).used, 0);
        assert!(queue.pop().is_none());
        assert!(!queue.push(b"z").await);
        assert!(queue.push(&[]).await);
        let queue = FileQueue::new(2);
        assert!(!queue.push(b"oversized").await);
        assert_eq!(lock(&queue.state).used, 0);
    }

    #[tokio::test]
    async fn file_queue_wakes_producer_and_keeps_order_when_closing() {
        let queue = FileQueue::new(2);
        assert!(queue.push(b"ab").await);
        let worker_queue = queue.clone();
        let worker = thread::spawn(move || {
            let first = worker_queue.pop().unwrap();
            assert_eq!(first, b"ab");
            worker_queue.release(first.len());
            let second = worker_queue.pop().unwrap();
            assert_eq!(second, b"cd");
            worker_queue.release(second.len());
            assert!(worker_queue.pop().is_none());
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(1), queue.push(b"cd"))
                .await
                .unwrap()
        );
        queue.close(false);
        worker.join().unwrap();
        assert_eq!(lock(&queue.state).used, 0);
    }

    enum WriteAction {
        Count(usize),
        Error(io::ErrorKind),
    }

    struct ScriptedWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        actions: VecDeque<WriteAction>,
        flush_error: bool,
    }

    impl Write for ScriptedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let count = match self.actions.pop_front() {
                Some(WriteAction::Count(count)) => count.min(bytes.len()),
                Some(WriteAction::Error(kind)) => return Err(io::Error::new(kind, "SECRET_SINK")),
                None => bytes.len(),
            };
            lock(&self.bytes).extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.flush_error {
                Err(io::Error::other("SECRET_FLUSH"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn file_writer_retries_interrupted_and_partial_writes_without_reordering() {
        let queue = FileQueue::new(20);
        queue.push(&[0, 255, 128]).await;
        queue.push(b"xyz").await;
        queue.close(false);
        let output = Arc::new(Mutex::new(Vec::new()));
        let state = state(20);
        write_file(
            ScriptedWriter {
                bytes: output.clone(),
                actions: [
                    WriteAction::Error(io::ErrorKind::Interrupted),
                    WriteAction::Count(1),
                    WriteAction::Count(1),
                ]
                .into(),
                flush_error: false,
            },
            &queue,
            &state,
        );
        assert_eq!(*lock(&output), [0, 255, 128, b'x', b'y', b'z']);
        assert_eq!(lock(&state).summary.file_bytes_written, 6);
        assert!(lock(&state).summary.writer_finished);
        assert!(lock(&state).summary.issue.is_none());
        assert_eq!(lock(&queue.state).used, 0);
    }

    #[tokio::test]
    async fn file_writer_preserves_partial_count_on_zero_error_and_flush_failure() {
        for action in [
            WriteAction::Count(0),
            WriteAction::Error(io::ErrorKind::BrokenPipe),
        ] {
            let queue = FileQueue::new(20);
            queue.push(b"abcdef").await;
            queue.push(b"queued").await;
            queue.close(false);
            let output = Arc::new(Mutex::new(Vec::new()));
            let state = state(20);
            write_file(
                ScriptedWriter {
                    bytes: output.clone(),
                    actions: [WriteAction::Count(2), action].into(),
                    flush_error: false,
                },
                &queue,
                &state,
            );
            assert_eq!(*lock(&output), b"ab");
            assert_eq!(lock(&state).summary.file_bytes_written, 2);
            assert!(lock(&state).summary.writer_finished);
            assert!(matches!(
                lock(&state).summary.issue,
                Some(CaptureIssue::WriteError(_))
            ));
            assert!(!format!("{:?}", lock(&state).summary.issue).contains("SECRET"));
            assert_eq!(lock(&queue.state).used, 0);
        }
        let queue = FileQueue::new(3);
        queue.push(b"abc").await;
        queue.close(false);
        let state = state(3);
        write_file(
            ScriptedWriter {
                bytes: Arc::new(Mutex::new(Vec::new())),
                actions: VecDeque::new(),
                flush_error: true,
            },
            &queue,
            &state,
        );
        assert_eq!(lock(&state).summary.file_bytes_written, 3);
        assert!(matches!(
            lock(&state).summary.issue,
            Some(CaptureIssue::WriteError(_))
        ));
    }

    #[tokio::test]
    async fn file_written_counter_overflow_is_explicit() {
        let queue = FileQueue::new(1);
        queue.push(b"x").await;
        queue.close(false);
        let state = state(1);
        lock(&state).summary.file_bytes_written = u64::MAX;
        write_file(Vec::new(), &queue, &state);
        assert_eq!(
            lock(&state).summary.issue,
            Some(CaptureIssue::CounterOverflow)
        );
    }

    #[tokio::test]
    async fn worker_permits_are_limited_and_released_by_real_ownership() {
        let _serial = CAPTURE_TESTS.lock().await;
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let mut permits = Vec::new();
        for _ in baseline..MAX_FILE_WORKERS {
            permits.push(WorkerPermit::acquire().unwrap());
        }
        assert_eq!(
            WorkerPermit::acquire().err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(permits.pop());
        permits.push(WorkerPermit::acquire().unwrap());
        drop(permits);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    }

    #[tokio::test]
    async fn prepare_resolves_invocation_directory_and_never_overwrites_capture() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let policy = CapturePolicy {
            capture_dir: Some("logs".into()),
            ..Default::default()
        };
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let session = OutputSession::prepare(&policy, &dir.0).unwrap();
        assert_eq!(
            lock(&session.state[0]).summary.file_path,
            Some(dir.0.join("logs/stdout.bin"))
        );
        assert_eq!(
            lock(&session.state[1]).summary.file_path,
            Some(dir.0.join("logs/stderr.bin"))
        );
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline + 2);
        drop(session);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        std::fs::write(dir.0.join("logs/stdout.bin"), b"keep stdout").unwrap();
        std::fs::write(dir.0.join("logs/stderr.bin"), b"keep stderr").unwrap();
        assert_eq!(
            OutputSession::prepare(&policy, &dir.0)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read(dir.0.join("logs/stdout.bin")).unwrap(),
            b"keep stdout"
        );
        assert_eq!(
            std::fs::read(dir.0.join("logs/stderr.bin")).unwrap(),
            b"keep stderr"
        );
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    }

    #[tokio::test]
    async fn prepare_rejects_zero_read_chunk_and_bad_directory() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let policy = CapturePolicy {
            read_chunk_bytes: 0,
            ..Default::default()
        };
        assert_eq!(
            OutputSession::prepare(&policy, &dir.0)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        std::fs::write(dir.0.join("file"), b"existing").unwrap();
        let policy = CapturePolicy {
            capture_dir: Some("file/subdir".into()),
            ..Default::default()
        };
        assert!(OutputSession::prepare(&policy, &dir.0).is_err());
        assert_eq!(std::fs::read(dir.0.join("file")).unwrap(), b"existing");
    }

    #[tokio::test]
    async fn second_file_worker_permit_failure_releases_first_reservation() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        assert!(baseline < MAX_FILE_WORKERS - 1);
        let mut held = Vec::new();
        for _ in baseline..MAX_FILE_WORKERS - 1 {
            held.push(WorkerPermit::acquire().unwrap());
        }
        let before = FILE_WORKERS.load(Ordering::Acquire);
        assert_eq!(before, MAX_FILE_WORKERS - 1);
        let path = dir.0.join("not-created");
        let policy = CapturePolicy {
            capture_dir: Some(path.clone()),
            ..Default::default()
        };
        // stdout can reserve the final permit; stderr cannot. Failed prepare
        // must release stdout's reservation before returning to its caller.
        let error = OutputSession::prepare(&policy, Path::new("/"))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), before);
        assert!(!path.exists());
        drop(held);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    }

    #[tokio::test]
    async fn second_capture_file_failure_releases_permits_and_preserves_existing_stderr() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let existing = dir.0.join("stderr.bin");
        std::fs::write(&existing, b"keep existing stderr").unwrap();
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            ..Default::default()
        };
        let error = OutputSession::prepare(&policy, Path::new("/"))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        assert_eq!(std::fs::read(existing).unwrap(), b"keep existing stderr");
        // Preparation is nontransactional: the newly created stdout artifact
        // remains, while the pre-existing stderr file is never overwritten.
        assert!(std::fs::read(dir.0.join("stdout.bin")).unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_pipe_capture_preserves_binary_bytes_and_bounded_tails() {
        let policy = CapturePolicy {
            read_chunk_bytes: 2,
            tail_bytes_per_stream: 3,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
        assert!(
            session
                .begin(ended_pipe(&[0, 255, 128, 13, 10]), ended_pipe(b"abcdef"))
                .unwrap()
                .is_none()
        );
        let (stdout, stderr) = session.finish(Duration::from_secs(1)).await;
        assert_eq!(stdout.bytes_read, 5);
        assert_eq!(stdout.tail, [128, 13, 10]);
        assert_eq!(stderr.bytes_read, 6);
        assert_eq!(stderr.tail, b"def");
        for summary in [&stdout, &stderr] {
            assert!(summary.eof);
            assert!(summary.tail_truncated);
            assert!(summary.writer_finished);
            assert!(summary.issue.is_none());
        }
        assert!(session.failure().is_none());
        let (again, _) = session.finish(Duration::ZERO).await;
        assert_eq!(again.tail, stdout.tail);
        assert_eq!(
            session
                .begin(ended_pipe(b""), ended_pipe(b""))
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn file_capture_preserves_raw_bytes_exact_quota_and_reports_overflow() {
        let _serial = CAPTURE_TESTS.lock().await;
        for quota in [0, 3, 4, 5] {
            let dir = TestDirectory::new();
            let policy = CapturePolicy {
                capture_dir: Some(dir.0.clone()),
                read_chunk_bytes: 3,
                tail_bytes_per_stream: 4,
                file_quota_bytes_per_stream: quota,
                ..Default::default()
            };
            let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
            session
                .begin(ended_pipe(&[0, 255, 128, 10]), ended_pipe(b"abc"))
                .unwrap();
            let (stdout, stderr) = session.finish(Duration::from_secs(1)).await;
            assert_eq!(stdout.bytes_read, 4);
            assert_eq!(stderr.bytes_read, 3);
            assert_eq!(stdout.tail, [0, 255, 128, 10]);
            assert_eq!(stdout.file_bytes_written, quota.min(4));
            assert_eq!(stderr.file_bytes_written, quota.min(3));
            assert_eq!(
                std::fs::read(dir.0.join("stdout.bin")).unwrap(),
                [0, 255, 128, 10][..quota.min(4) as usize]
            );
            assert_eq!(
                std::fs::read(dir.0.join("stderr.bin")).unwrap(),
                b"abc"[..quota.min(3) as usize]
            );
            assert_eq!(
                stdout.issue,
                (quota < 4).then_some(CaptureIssue::FileQuotaExceeded)
            );
            assert_eq!(
                stderr.issue,
                (quota < 3).then_some(CaptureIssue::FileQuotaExceeded)
            );
            assert!(stdout.eof && stderr.eof && stdout.writer_finished && stderr.writer_finished);
            assert_eq!(session.failure().is_some(), quota < 4);
        }
    }

    #[tokio::test]
    async fn undrained_pipe_expires_without_waiting_for_eof() {
        let mut session =
            OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
        let (read, _held_writer) = pipe();
        session.begin(read, ended_pipe(b"stderr bytes")).unwrap();
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(1),
            session.finish(Duration::from_millis(5)),
        )
        .await
        .unwrap();
        assert!(!stdout.eof);
        assert_eq!(stdout.issue, Some(CaptureIssue::DrainExpired));
        assert_eq!(stderr.tail, b"stderr bytes");
        assert!(stderr.eof);
        assert_eq!(session.failure(), Some(CaptureIssue::DrainExpired));
    }

    #[tokio::test]
    async fn session_drop_aborts_pipe_readers_and_closes_display() {
        let policy = CapturePolicy {
            mode: OutputMode::StreamAndCapture,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
        let (read, _writer) = pipe();
        let mut receiver = session.begin(read, ended_pipe(b"")).unwrap().unwrap();
        drop(session);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn stream_slow_consumer_is_bounded_and_losses_are_visible() {
        let policy = CapturePolicy {
            mode: OutputMode::StreamAndCapture,
            read_chunk_bytes: 3,
            stream_queue_bytes: 3,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
        let _receiver = session
            .begin(ended_pipe(b"abcdef"), ended_pipe(b"xyz"))
            .unwrap()
            .unwrap();
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(1),
            session.finish(Duration::from_millis(10)),
        )
        .await
        .unwrap();
        assert_eq!(stdout.bytes_read + stderr.bytes_read, 9);
        assert_eq!(stdout.stream_dropped_bytes + stderr.stream_dropped_bytes, 9);
        assert_eq!(
            stdout.stream_unconfirmed_bytes + stderr.stream_unconfirmed_bytes,
            0
        );
        assert!(stdout.issue.is_none() && stderr.issue.is_none());
        assert!(stdout.eof && stderr.eof);
    }

    #[tokio::test]
    async fn stream_inflight_unacknowledged_bytes_remain_explicit() {
        let policy = CapturePolicy {
            mode: OutputMode::StreamAndCapture,
            read_chunk_bytes: 3,
            stream_queue_bytes: 6,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
        let mut receiver = session
            .begin(ended_pipe(b"abc"), ended_pipe(b""))
            .unwrap()
            .unwrap();
        let mut chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        chunk.mark_displayed(1);
        let (stdout, _) = session.finish(Duration::from_millis(5)).await;
        assert_eq!(stdout.stream_dropped_bytes, 0);
        assert_eq!(stdout.stream_unconfirmed_bytes, 2);
        assert!(stdout.issue.is_none());
        drop(chunk);
        assert_eq!(lock(&receiver.queue.state).dropped[0], 2);
        // The returned result is an immutable snapshot, not a live view.
        assert_eq!(stdout.stream_unconfirmed_bytes, 2);
    }

    #[tokio::test]
    async fn acknowledged_held_chunk_has_no_loss_or_capture_error_after_finalization() {
        let policy = CapturePolicy {
            mode: OutputMode::StreamAndCapture,
            read_chunk_bytes: 3,
            stream_queue_bytes: 3,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
        let mut receiver = session
            .begin(ended_pipe(b"abc"), ended_pipe(b""))
            .unwrap()
            .unwrap();
        let mut chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        chunk.mark_displayed(chunk.bytes().len());
        tokio::time::timeout(Duration::from_secs(1), async {
            while !session
                .readers
                .iter()
                .all(|reader| reader.as_ref().is_some_and(JoinHandle::is_finished))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // The chunk still reserves its memory budget. No latency claim is
        // made, but complete acknowledgment must prevent false display loss.
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(1),
            session.finish(Duration::from_millis(5)),
        )
        .await
        .unwrap();
        assert_eq!(stdout.bytes_read, 3);
        assert_eq!(stdout.tail, b"abc");
        for summary in [&stdout, &stderr] {
            assert_eq!(summary.stream_dropped_bytes, 0);
            assert_eq!(summary.stream_unconfirmed_bytes, 0);
            assert!(summary.issue.is_none());
            assert!(summary.eof && summary.writer_finished);
        }
        chunk.report_error(io::Error::from(io::ErrorKind::BrokenPipe));
        assert!(lock(&receiver.queue.state).error.is_none());
        drop(chunk);
        receiver.report_error(
            OutputStream::Stdout,
            io::Error::from(io::ErrorKind::BrokenPipe),
        );
        let (again, _) = session.finish(Duration::ZERO).await;
        assert_eq!(again.tail, stdout.tail);
        assert_eq!(again.stream_dropped_bytes, stdout.stream_dropped_bytes);
        assert_eq!(
            again.stream_unconfirmed_bytes,
            stdout.stream_unconfirmed_bytes
        );
        assert!(again.issue.is_none());
        assert!(session.failure().is_none());
    }

    #[tokio::test]
    async fn stream_error_propagates_through_session_failure_and_snapshot() {
        let policy = CapturePolicy {
            mode: OutputMode::StreamAndCapture,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new(".")).unwrap();
        let mut receiver = session
            .begin(ended_pipe(b""), ended_pipe(b"error"))
            .unwrap()
            .unwrap();
        let chunk = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        chunk.report_error(io::Error::from(io::ErrorKind::BrokenPipe));
        drop(chunk);
        assert!(matches!(
            session.failure(),
            Some(CaptureIssue::StreamError(_))
        ));
        let (stdout, stderr) = session.finish(Duration::from_secs(1)).await;
        assert!(stdout.issue.is_none());
        assert!(matches!(stderr.issue, Some(CaptureIssue::StreamError(_))));
        assert_eq!(stderr.stream_dropped_bytes, 5);
        receiver.report_error(OutputStream::Stderr, io::Error::other("late failure"));
        assert_eq!(
            lock(&receiver.queue.state).error.as_ref().unwrap().1,
            stderr.issue.unwrap()
        );
    }

    #[tokio::test]
    async fn nonblocking_pipe_handles_real_reader_error() {
        let (read, write) = pipe();
        drop(read);
        let pipe = nonblocking_pipe(write).unwrap();
        let state = state(3);
        tokio::time::timeout(
            Duration::from_secs(1),
            read_pipe(pipe, state.clone(), None, None, OutputStream::Stdout, 3, 10),
        )
        .await
        .unwrap();
        assert!(!lock(&state).summary.eof);
        assert!(matches!(
            lock(&state).summary.issue,
            Some(CaptureIssue::ReadError(_))
        ));
    }

    struct BlockedWriter {
        entered: Option<tokio::sync::oneshot::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                self.release.recv().unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn blocked_file_sink_has_bounded_drain_and_keeps_worker_permit() {
        let _serial = CAPTURE_TESTS.lock().await;
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let permit = WorkerPermit::acquire().unwrap();
        let mut session =
            OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
        session.started = true;
        for state in &session.state {
            lock(state).summary.eof = true;
        }
        lock(&session.state[0]).summary.writer_finished = false;
        let queue = FileQueue::new(3);
        queue.push(b"abc").await;
        queue.close(false);
        let worker_queue = queue.clone();
        let worker_state = session.state[0].clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            let _permit = permit;
            write_file(
                BlockedWriter {
                    entered: Some(entered_tx),
                    release: release_rx,
                },
                &worker_queue,
                &worker_state,
            );
        });
        session.workers[0] = Some(FileWorker { queue, thread });
        tokio::time::timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let (stdout, _) = tokio::time::timeout(
            Duration::from_secs(1),
            session.finish(Duration::from_millis(5)),
        )
        .await
        .unwrap();
        assert!(!stdout.writer_finished);
        assert_eq!(stdout.issue, Some(CaptureIssue::DrainExpired));
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline + 1);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while FILE_WORKERS.load(Ordering::Acquire) != baseline {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(lock(&session.state[0]).summary.file_bytes_written, 3);
    }

    #[tokio::test]
    async fn adversarial_reader_panic_closes_its_file_queue_without_orphaning_workers() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            // Capacity overflow is deterministic and attempts no physical
            // allocation. This executes the production reader's real panic.
            read_chunk_bytes: usize::MAX,
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        session.begin(ended_pipe(b""), ended_pipe(b"")).unwrap();
        let readers_completed = tokio::time::timeout(Duration::from_secs(1), async {
            while !session
                .readers
                .iter()
                .all(|reader| reader.as_ref().is_some_and(JoinHandle::is_finished))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let detected_failure = session.failure();
        let producer_queues_closed: Vec<_> = session
            .workers
            .iter()
            .flatten()
            .map(|worker| lock(&worker.queue.state).closed)
            .collect();

        // No producer survives and both files are empty. Observe the actual
        // drain behavior as well as queue state: an open orphaned queue makes
        // these idle workers wait for the large drain deadline. Cancelling this
        // finish future leaves the session available for independent cleanup.
        let drain_started = tokio::time::Instant::now();
        let finished_within_idle_budget = tokio::time::timeout(
            Duration::from_millis(100),
            session.finish(Duration::from_secs(30)),
        )
        .await
        .is_ok();
        let idle_drain_elapsed = drain_started.elapsed();

        // Cleanup is independent of the asserted producer behavior. Manually
        // close both queues before any assertion; these empty regular files
        // have no blocked write. finish can then join their actual OS workers,
        // and the test does not leave permits behind even when the probe fails.
        for worker in session.workers.iter().flatten() {
            worker.queue.close(true);
        }
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(2),
            session.finish(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        drop(session);
        assert!(stdout.writer_finished && stderr.writer_finished);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        assert!(readers_completed, "real reader panic did not complete");
        assert!(matches!(detected_failure, Some(CaptureIssue::ReadError(_))));
        assert!(
            finished_within_idle_budget,
            "empty capture workers failed to finish after their producers stopped: queues_closed={producer_queues_closed:?}, elapsed={idle_drain_elapsed:?}; idle workers remain blocked until a separate drain deadline or manual queue closure"
        );
        assert_eq!(
            producer_queues_closed,
            [true, true],
            "completed readers left producer queues open; workers cannot discover EOF and retain their permits until a separate drain deadline closes them"
        );
    }

    #[tokio::test]
    async fn real_reader_error_closes_its_file_queue_and_releases_workers() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        let (invalid_reader, write_end) = pipe();
        drop(invalid_reader);
        // A write-only pipe endpoint makes the real libc::read fail with
        // EBADF, contrasting the normal error epilogue with a task unwind.
        session.begin(write_end, ended_pipe(b"")).unwrap();
        let readers_completed = tokio::time::timeout(Duration::from_secs(1), async {
            while !session
                .readers
                .iter()
                .all(|reader| reader.as_ref().is_some_and(JoinHandle::is_finished))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let detected_failure = session.failure();
        let producer_queues_closed: Vec<_> = session
            .workers
            .iter()
            .flatten()
            .map(|worker| lock(&worker.queue.state).closed)
            .collect();
        // As in the panic probe, explicit queue closure precedes assertions
        // and independently prevents leaked test workers or global permits.
        for worker in session.workers.iter().flatten() {
            worker.queue.close(true);
        }
        let (stdout, stderr) = tokio::time::timeout(
            Duration::from_secs(2),
            session.finish(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        drop(session);
        assert!(stdout.writer_finished && stderr.writer_finished);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        assert!(readers_completed, "real read error did not complete");
        assert!(matches!(detected_failure, Some(CaptureIssue::ReadError(_))));
        assert_eq!(producer_queues_closed, [true, true]);
    }

    #[tokio::test]
    async fn aborted_readers_close_file_queues_before_first_poll_and_after_reading() {
        let _serial = CAPTURE_TESTS.lock().await;
        for after_reading in [false, true] {
            let dir = TestDirectory::new();
            let baseline = FILE_WORKERS.load(Ordering::Acquire);
            let policy = CapturePolicy {
                capture_dir: Some(dir.0.clone()),
                read_chunk_bytes: 3,
                ..Default::default()
            };
            let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
            let (stdout, stdout_writer) = pipe();
            let (stderr, _stderr_writer) = pipe();
            let mut stdout_writer = File::from(stdout_writer);
            session.begin(stdout, stderr).unwrap();
            let read_completed = if after_reading {
                stdout_writer.write_all(b"abc").unwrap();
                tokio::time::timeout(Duration::from_secs(1), async {
                    while lock(&session.state[0]).summary.bytes_read != 3 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .is_ok()
            } else {
                // The current-thread test runtime has not yielded after begin,
                // so neither read_pipe future has been polled in this branch.
                true
            };
            for reader in session.readers.iter().flatten() {
                reader.abort();
            }
            // OutputSession::finish remains the sole consumer of JoinHandles.
            // Observe task termination here without polling a completed handle
            // twice; cancellation drops the producer before is_finished.
            let all_stopped = tokio::time::timeout(Duration::from_secs(1), async {
                while !session
                    .readers
                    .iter()
                    .flatten()
                    .all(JoinHandle::is_finished)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok();
            let producer_queues_closed: Vec<_> = session
                .workers
                .iter()
                .flatten()
                .map(|worker| lock(&worker.queue.state).closed)
                .collect();
            let detected_failure = session.failure();
            // Independent cleanup precedes assertions, retaining the worker
            // permit only until its actual thread can be joined.
            for worker in session.workers.iter().flatten() {
                worker.queue.close(false);
            }
            let (stdout, stderr) = tokio::time::timeout(
                Duration::from_secs(2),
                session.finish(Duration::from_secs(1)),
            )
            .await
            .unwrap();
            drop(session);
            assert!(read_completed && all_stopped);
            assert_eq!(producer_queues_closed, [true, true]);
            assert!(matches!(detected_failure, Some(CaptureIssue::ReadError(_))));
            assert!(stdout.writer_finished && stderr.writer_finished);
            assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
            assert!(!stdout.eof && !stderr.eof);
            let expected = if after_reading {
                b"abc".as_slice()
            } else {
                b""
            };
            assert_eq!(std::fs::read(dir.0.join("stdout.bin")).unwrap(), expected);
            assert_eq!(stdout.file_bytes_written, expected.len() as u64);
            assert!(std::fs::read(dir.0.join("stderr.bin")).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn dropping_unpolled_file_session_closes_queues_and_releases_actual_workers() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        let (stdout, _stdout_writer) = pipe();
        let (stderr, _stderr_writer) = pipe();
        session.begin(stdout, stderr).unwrap();
        let readers: Vec<_> = session
            .readers
            .iter()
            .flatten()
            .map(JoinHandle::abort_handle)
            .collect();
        let queues: Vec<_> = session
            .workers
            .iter()
            .flatten()
            .map(|worker| worker.queue.clone())
            .collect();
        let snapshot = session.snapshotter();
        // No yield: OutputSession itself must close its workers' queues even
        // if the Tokio tasks have not begun executing read_pipe's body.
        drop(session);
        assert!(queues.iter().all(|queue| lock(&queue.state).closed));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !readers.iter().all(tokio::task::AbortHandle::is_finished)
                || FILE_WORKERS.load(Ordering::Acquire) != baseline
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (stdout, stderr) = snapshot.capture();
        assert!(stdout.writer_finished && stderr.writer_finished);
        assert!(!stdout.eof && !stderr.eof);
        assert_eq!(stdout.bytes_read + stderr.bytes_read, 0);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    }

    #[tokio::test]
    async fn worker_or_reader_panics_become_capture_failure() {
        let mut session =
            OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
        session.started = true;
        let reader = tokio::spawn(async { panic!("injected reader panic") });
        while !reader.is_finished() {
            tokio::task::yield_now().await;
        }
        session.readers[0] = Some(reader);
        assert!(matches!(
            session.failure(),
            Some(CaptureIssue::ReadError(_))
        ));
        let (stdout, _) = session.finish(Duration::ZERO).await;
        assert!(matches!(stdout.issue, Some(CaptureIssue::ReadError(_))));

        let mut session =
            OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
        session.started = true;
        lock(&session.state[1]).summary.writer_finished = false;
        let thread = thread::spawn(|| panic!("injected writer panic"));
        while !thread.is_finished() {
            tokio::task::yield_now().await;
        }
        session.workers[1] = Some(FileWorker {
            queue: FileQueue::new(3),
            thread,
        });
        assert!(matches!(
            session.failure(),
            Some(CaptureIssue::WriteError(_))
        ));
        let (_, stderr) = session.finish(Duration::ZERO).await;
        assert!(matches!(stderr.issue, Some(CaptureIssue::WriteError(_))));
    }

    #[tokio::test]
    async fn panicked_file_writer_closes_queue_and_releases_inflight_reservation() {
        struct PanicWriter;
        impl Write for PanicWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                panic!("injected actual file writer unwind");
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let _serial = CAPTURE_TESTS.lock().await;
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let mut session =
            OutputSession::prepare(&CapturePolicy::default(), Path::new(".")).unwrap();
        session.started = true;
        lock(&session.state[0]).summary.writer_finished = false;
        let queue = FileQueue::new(3);
        let reader = tokio::spawn(read_pipe(
            nonblocking_pipe(ended_pipe(b"abcdef")).unwrap(),
            session.state[0].clone(),
            Some(FileProducer {
                queue: queue.clone(),
            }),
            None,
            OutputStream::Stdout,
            3,
            20,
        ));
        session.readers[0] = Some(reader);
        let permit = WorkerPermit::acquire().unwrap();
        let worker_queue = queue.clone();
        let state = session.state[0].clone();
        let worker = thread::spawn(move || {
            let _permit = permit;
            write_file(PanicWriter, &worker_queue, &state);
        });
        session.workers[0] = Some(FileWorker {
            queue: queue.clone(),
            thread: worker,
        });
        let worker_finished = tokio::time::timeout(Duration::from_secs(1), async {
            while !session.workers[0].as_ref().unwrap().thread.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        let detected_failure = session.failure();
        let (closed_after_unwind, reserved_after_unwind) = {
            let state = lock(&queue.state);
            (state.closed, state.used)
        };
        let finished_without_orphan_wait = tokio::time::timeout(
            Duration::from_millis(100),
            session.finish(Duration::from_secs(30)),
        )
        .await
        .is_ok();

        // Independent cleanup precedes assertions even when the consumer left
        // its queue open: the real producer task and worker handle are retained.
        queue.close(true);
        let (stdout, _) = tokio::time::timeout(
            Duration::from_secs(2),
            session.finish(Duration::from_secs(1)),
        )
        .await
        .unwrap();
        drop(session);
        assert!(worker_finished);
        assert!(matches!(
            detected_failure,
            Some(CaptureIssue::WriteError(_))
        ));
        assert!(matches!(stdout.issue, Some(CaptureIssue::WriteError(_))));
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        assert!(
            closed_after_unwind,
            "dead file consumer left its producer queue open"
        );
        assert_eq!(
            reserved_after_unwind, 0,
            "unwound writer retained its dropped chunk reservation"
        );
        assert!(
            finished_without_orphan_wait,
            "producer waited for a dead file consumer until drain expiration"
        );
    }

    #[tokio::test]
    async fn file_writer_unwind_preserves_partial_count_and_first_error_at_write_and_flush() {
        struct PanicWriter {
            flush_panics: bool,
            writes: usize,
        }
        impl Write for PanicWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                if !self.flush_panics && self.writes > 1 {
                    panic!("SECRET_WRITE_PANIC");
                }
                Ok(if self.flush_panics { bytes.len() } else { 2 })
            }
            fn flush(&mut self) -> io::Result<()> {
                panic!("SECRET_FLUSH_PANIC");
            }
        }

        for flush_panics in [false, true] {
            for prior_issue in [None, Some(CaptureIssue::FileQuotaExceeded)] {
                let queue = FileQueue::new(20);
                assert!(queue.push(b"abcdef").await);
                assert!(queue.push(b"xyz").await);
                queue.close(false);
                let state = state(20);
                lock(&state).summary.issue = prior_issue.clone();
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    write_file(
                        PanicWriter {
                            flush_panics,
                            writes: 0,
                        },
                        &queue,
                        &state,
                    );
                }))
                .is_err();
                assert!(panicked);
                let state = lock(&state);
                assert_eq!(
                    state.summary.file_bytes_written,
                    if flush_panics { 9 } else { 2 }
                );
                assert!(!state.summary.writer_finished);
                assert_eq!(
                    state.summary.issue,
                    prior_issue.or_else(|| Some(CaptureIssue::WriteError(
                        "capture worker stopped unexpectedly".into(),
                    )))
                );
                assert!(!format!("{:?}", state.summary.issue).contains("SECRET"));
                let queued = lock(&queue.state);
                assert!(queued.closed && queued.chunks.is_empty());
                assert_eq!(queued.used, 0);
            }
        }
    }

    #[tokio::test]
    async fn successful_file_and_display_capture_finishes_at_eof_for_small_read_partitions() {
        let _serial = CAPTURE_TESTS.lock().await;
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let stdout_bytes: Vec<_> = (0..2003).map(|index| index as u8).collect();
        let stderr_bytes: Vec<_> = (0..1999).map(|index| (255 - index % 256) as u8).collect();
        for read_chunk_bytes in [1, 3, 32, 32 * 1024] {
            let dir = TestDirectory::new();
            let policy = CapturePolicy {
                capture_dir: Some(dir.0.clone()),
                read_chunk_bytes,
                tail_bytes_per_stream: 0,
                mode: OutputMode::StreamAndCapture,
                stream_queue_bytes: 4096,
                ..Default::default()
            };
            let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
            let mut receiver = session
                .begin(ended_pipe(&stdout_bytes), ended_pipe(&stderr_bytes))
                .unwrap()
                .unwrap();
            let consumer = tokio::spawn(async move {
                let mut displayed = [Vec::new(), Vec::new()];
                while let Some(mut chunk) = receiver.recv().await {
                    displayed[chunk.stream.index()].extend_from_slice(chunk.bytes());
                    chunk.mark_displayed(chunk.bytes().len());
                }
                displayed
            });
            let (stdout, stderr) = tokio::time::timeout(
                Duration::from_secs(3),
                session.finish(Duration::from_secs(2)),
            )
            .await
            .unwrap();
            let displayed = tokio::time::timeout(Duration::from_secs(1), consumer)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(displayed, [stdout_bytes.clone(), stderr_bytes.clone()]);
            for (summary, expected) in [(&stdout, &stdout_bytes), (&stderr, &stderr_bytes)] {
                assert!(summary.eof && summary.writer_finished);
                assert!(summary.issue.is_none());
                assert!(summary.tail.is_empty() && summary.tail_truncated);
                assert_eq!(summary.bytes_read, expected.len() as u64);
                assert_eq!(summary.file_bytes_written, expected.len() as u64);
                assert_eq!(
                    summary.stream_dropped_bytes + summary.stream_unconfirmed_bytes,
                    0
                );
                assert_eq!(
                    std::fs::read(summary.file_path.as_ref().unwrap()).unwrap(),
                    *expected
                );
            }
            assert!(session.failure().is_none());
            let (frozen, _) = session.finish(Duration::ZERO).await;
            assert!(frozen.eof && frozen.writer_finished && frozen.issue.is_none());
            assert_eq!(frozen.bytes_read, stdout.bytes_read);
            assert_eq!(frozen.file_bytes_written, stdout.file_bytes_written);
            drop(session);
            assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
        }
    }

    #[tokio::test]
    async fn unstarted_session_finish_releases_prepared_files_without_workers() {
        let _serial = CAPTURE_TESTS.lock().await;
        let dir = TestDirectory::new();
        let baseline = FILE_WORKERS.load(Ordering::Acquire);
        let policy = CapturePolicy {
            capture_dir: Some(dir.0.clone()),
            ..Default::default()
        };
        let mut session = OutputSession::prepare(&policy, Path::new("/")).unwrap();
        let (stdout, stderr) = session.finish(Duration::ZERO).await;
        assert!(stdout.writer_finished && stderr.writer_finished);
        assert_eq!(FILE_WORKERS.load(Ordering::Acquire), baseline);
    }
}
