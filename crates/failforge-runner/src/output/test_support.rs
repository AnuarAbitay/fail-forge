use super::capture::{CaptureState, TailBuffer};
use super::stream::{StreamQueue, StreamReceiver};
use crate::result::CaptureSummary;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub(super) static CAPTURE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
pub(super) static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

pub(super) struct TestDirectory(pub(super) PathBuf);

impl TestDirectory {
    pub(super) fn new() -> Self {
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

pub(super) fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [-1; 2];
    // SAFETY: fds points to storage for the two descriptors pipe writes.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: a successful pipe returned two fresh descriptors, and these
    // OwnedFd values are their only owners.
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

pub(super) fn ended_pipe(bytes: &[u8]) -> OwnedFd {
    let (read, write) = pipe();
    let mut file = File::from(write);
    file.write_all(bytes).unwrap();
    drop(file);
    read
}

pub(super) fn state(limit: usize) -> Arc<Mutex<CaptureState>> {
    Arc::new(Mutex::new(CaptureState {
        summary: CaptureSummary::default(),
        tail: TailBuffer::new(limit),
        sha256: Sha256::new(),
    }))
}

pub(super) fn stream_receiver(queue: &Arc<StreamQueue>) -> StreamReceiver {
    StreamReceiver {
        queue: queue.clone(),
    }
}
