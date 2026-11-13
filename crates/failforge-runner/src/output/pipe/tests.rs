use super::super::test_support::{pipe, state};
use super::*;
use std::time::Duration;

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
    let summary = lock(&state).summary.clone();
    assert!(!summary.eof);
    assert_eq!(summary.bytes_read, 0);
    assert!(matches!(
        summary.issue,
        Some(CaptureIssue::ReadError(message))
            if message == error_description(&io::Error::from_raw_os_error(libc::EBADF))
    ));
}
