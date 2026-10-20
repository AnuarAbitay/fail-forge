//! A bounded queue that stops admission and drains every owned task on failure.
//!
//! Tasks return only after their own children and temporary copies are cleaned.
//! Cancellation is cooperative: this layer never aborts or drops active tasks.
//! The sink observes completion order and must accept an already sealed result
//! even if cancellation was requested before the controller received it.
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};

use failforge_dart::namespace::NamespaceError;
use failforge_workspace::Cancellation;
use tokio::task::JoinSet;

#[derive(Debug, Default)]
pub(crate) struct QueueSummary {
    /// Tasks admitted to the owned task set.
    pub dispatched: usize,
    /// Joined tasks, including failed or panicked tasks.
    pub joined: usize,
    /// Successfully sealed task results, including results after sink failure.
    pub completed: usize,
    /// Results accepted by the sink without error.
    pub emitted: usize,
    pub error: Option<NamespaceError>,
}

fn invalid(message: &str) -> NamespaceError {
    NamespaceError::Invalid(message.into())
}

fn signal(error: &NamespaceError) -> i32 {
    match error {
        NamespaceError::Cancelled(signal) => *signal,
        _ => libc::SIGINT,
    }
}

struct PanicCancellation(Cancellation);
impl Drop for PanicCancellation {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.request(libc::SIGINT);
        }
    }
}

impl QueueSummary {
    fn fail(&mut self, error: NamespaceError, cancel: &Cancellation) {
        let signal = signal(&error);
        if self.error.is_none()
            || matches!(self.error, Some(NamespaceError::Cancelled(_)))
                && !matches!(error, NamespaceError::Cancelled(_))
        {
            self.error = Some(error);
        }
        cancel.request(signal);
    }
}

pub(crate) async fn run<I, T, F, Fut, S>(
    items: impl IntoIterator<Item = I>,
    workers: usize,
    cancel: &Cancellation,
    mut start: F,
    mut sink: S,
) -> QueueSummary
where
    T: Send + 'static,
    F: FnMut(usize, I) -> Fut,
    Fut: Future<Output = Result<T, NamespaceError>> + Send + 'static,
    S: FnMut(usize, T) -> Result<(), NamespaceError>,
{
    let mut summary = QueueSummary::default();
    if workers == 0 {
        summary.fail(invalid("family worker count must be positive"), cancel);
        return summary;
    }
    let mut pending = items.into_iter().enumerate();
    let mut active = JoinSet::new();
    let mut sink_available = true;
    loop {
        while active.len() < workers && summary.error.is_none() && cancel.signum().is_none() {
            let Some((index, item)) = pending.next() else {
                break;
            };
            // A synchronous factory failure must still drain previously owned
            // tasks instead of unwinding through JoinSet's aborting destructor.
            let future = match catch_unwind(AssertUnwindSafe(|| start(index, item))) {
                Ok(future) => future,
                Err(_) => {
                    summary.fail(invalid("native family task factory panicked"), cancel);
                    break;
                }
            };
            let stopping = cancel.clone();
            active.spawn(async move {
                let _panic = PanicCancellation(stopping.clone());
                let result = future.await;
                if let Err(error) = &result {
                    // A queued failure must stop admission even if another
                    // already completed success is joined before this result.
                    stopping.request(signal(error));
                }
                (index, result)
            });
            summary.dispatched += 1;
        }
        let Some(joined) = active.join_next().await else {
            break;
        };
        summary.joined += 1;
        match joined {
            Ok((index, Ok(result))) => {
                summary.completed += 1;
                if sink_available {
                    let emitted = catch_unwind(AssertUnwindSafe(|| sink(index, result)))
                        .unwrap_or_else(|_| Err(invalid("native family result sink panicked")));
                    match emitted {
                        Ok(()) => summary.emitted += 1,
                        Err(error) => {
                            sink_available = false;
                            summary.fail(error, cancel);
                        }
                    }
                }
            }
            Ok((_, Err(error))) => summary.fail(error, cancel),
            Err(error) => summary.fail(
                invalid(if error.is_panic() {
                    "native family worker panicked"
                } else {
                    "native family worker was unexpectedly aborted"
                }),
                cancel,
            ),
        }
    }
    if let Some(signal) = cancel.signum() {
        summary.fail(NamespaceError::Cancelled(signal), cancel);
    }
    summary
}

#[cfg(test)]
#[path = "family_queue_tests.rs"]
mod tests;
