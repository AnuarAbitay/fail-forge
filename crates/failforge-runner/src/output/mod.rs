//! Byte-oriented output capture. Readers never decode child output.

mod capture;
mod file;
mod pipe;
mod session;
mod stream;

pub(crate) use session::{OutputSession, OutputSnapshot};
pub use stream::{OutputStream, StreamChunk, StreamReceiver};

use std::io;
use std::sync::{Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn error_description(error: &io::Error) -> String {
    format!("{:?}, OS error {:?}", error.kind(), error.raw_os_error())
}

#[cfg(all(test, unix))]
mod test_support;
