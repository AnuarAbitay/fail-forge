use crate::result::{CaptureIssue, CaptureSummary};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

pub(super) struct CaptureState {
    pub(super) summary: CaptureSummary,
    pub(super) tail: TailBuffer,
    pub(super) sha256: Sha256,
}

impl CaptureState {
    pub(super) fn record(&mut self, bytes: &[u8]) {
        // Hash the bytes read from the pipe before any file queue or display
        // sink can drop, truncate, or replace them.
        self.sha256.update(bytes);
        match self.summary.bytes_read.checked_add(bytes.len() as u64) {
            Some(total) => self.summary.bytes_read = total,
            None => {
                self.summary.bytes_read = u64::MAX;
                self.issue(CaptureIssue::CounterOverflow);
            }
        }
        self.tail.push(bytes);
    }

    pub(super) fn issue(&mut self, issue: CaptureIssue) {
        if self.summary.issue.is_none() {
            self.summary.issue = Some(issue);
        }
    }

    pub(super) fn snapshot(&self) -> CaptureSummary {
        let mut summary = self.summary.clone();
        summary.tail = self.tail.snapshot();
        summary.tail_truncated = self.tail.truncated;
        summary.sha256 = Some(self.sha256.clone().finalize().into());
        summary
    }
}

/// The retained suffix of one child stream. The byte limit is independent of
/// the number or shape of reads, including a read larger than the entire tail.
pub(super) struct TailBuffer {
    bytes: VecDeque<u8>,
    limit: usize,
    truncated: bool,
}

impl TailBuffer {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            limit,
            truncated: false,
        }
    }

    pub(super) fn push(&mut self, bytes: &[u8]) {
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

    pub(super) fn snapshot(&self) -> Vec<u8> {
        self.bytes.iter().copied().collect()
    }
}

#[cfg(all(test, unix))]
mod tests;
