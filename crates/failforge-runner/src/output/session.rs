use super::capture::{CaptureState, TailBuffer};
use super::file::{FileProducer, FileQueue, FileWorker, PreparedFile, WorkerPermit, write_file};
use super::pipe::{nonblocking_pipe, read_pipe};
use super::stream::{OutputStream, StreamQueue, StreamReceiver};
use super::{error_description, lock};
use crate::platform::OutputPipe;
use crate::result::{CaptureIssue, CaptureSummary};
use crate::spec::{CapturePolicy, OutputMode};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::task::JoinHandle;

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
    pub(super) state: [Arc<Mutex<CaptureState>>; 2],
    pub(super) stream: Option<Arc<StreamQueue>>,
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
                sha256: Sha256::new(),
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
                #[cfg(unix)]
                let mut options = OpenOptions::new();
                #[cfg(unix)]
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                #[cfg(unix)]
                let file = options.open(&path)?;
                #[cfg(windows)]
                let file = crate::security_windows::create_private_file(&path)?;
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
        stdout: OutputPipe,
        stderr: OutputPipe,
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

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, unix))]
mod drain_tests;
