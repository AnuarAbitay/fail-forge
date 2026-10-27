//! Bounded native stream delivery, independent of SDK/process cleanup.
use crate::windows_output::Writer;
use failforge_runner::{OutputStream, StreamReceiver};
use std::io::{self, Write};
use std::time::Duration;
pub(crate) fn spawn(mut receiver: StreamReceiver) -> io::Result<()> {
    let mut stdout = Writer::stdio(false, Duration::from_secs(1))?;
    let mut stderr = Writer::stdio(true, Duration::from_secs(1))?;
    std::thread::Builder::new()
        .name("failforge-display".into())
        .spawn(move || {
            while let Some(mut chunk) = receiver.blocking_recv() {
                let writer = match chunk.stream {
                    OutputStream::Stdout => &mut stdout,
                    OutputStream::Stderr => &mut stderr,
                };
                let mut offset = 0;
                while offset < chunk.bytes().len() {
                    match writer.write(&chunk.bytes()[offset..]) {
                        Ok(0) => {
                            chunk.report_error(io::ErrorKind::WriteZero.into());
                            return;
                        }
                        Ok(count) => {
                            offset += count;
                            chunk.mark_displayed(count);
                        }
                        Err(error) => {
                            chunk.report_error(error);
                            return;
                        }
                    }
                }
            }
        })?;
    Ok(())
}
