use super::{error_description, lock};
use crate::result::CaptureIssue;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    pub(super) fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
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
    pub(super) queue: Arc<StreamQueue>,
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

pub(super) struct StreamQueueState {
    pub(super) chunks: VecDeque<(OutputStream, Vec<u8>)>,
    pub(super) used: usize,
    pub(super) dropped: [u64; 2],
    pub(super) unconfirmed: [u64; 2],
    pub(super) closed: bool,
    pub(super) finalized: bool,
    pub(super) receiver_alive: bool,
    pub(super) error: Option<(OutputStream, CaptureIssue)>,
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

pub(super) struct StreamQueue {
    pub(super) state: Mutex<StreamQueueState>,
    limit: usize,
    ready: Notify,
    condition: Condvar,
}

impl StreamQueue {
    pub(super) fn new(limit: usize) -> Arc<Self> {
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

    pub(super) fn push(&self, stream: OutputStream, bytes: &[u8]) {
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

    #[cfg(all(test, unix))]
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

    pub(super) fn close(&self, discard: bool) {
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

#[cfg(all(test, unix))]
mod tests;
