//! Latched cancellation shared by synchronous input operations and process owners.
use tokio::sync::watch;

use crate::InputError;

#[derive(Clone, Debug)]
pub struct Cancellation(watch::Sender<i32>);

impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(0).0)
    }
}

impl Cancellation {
    /// The first signal wins. A watch value retains signals received before a
    /// synchronous snapshot or before a helper installs its signal handlers.
    pub fn request(&self, signum: i32) {
        self.0.send_if_modified(|current| {
            if *current != 0 {
                return false;
            }
            *current = if signum == libc::SIGTERM {
                libc::SIGTERM
            } else {
                libc::SIGINT
            };
            true
        });
    }

    pub fn signum(&self) -> Option<i32> {
        let signal = *self.0.borrow();
        (signal != 0).then_some(signal)
    }

    pub fn check(&self) -> Result<(), InputError> {
        match self.signum() {
            Some(signal) => Err(InputError::Cancelled(signal)),
            None => Ok(()),
        }
    }

    pub async fn cancelled(&self) -> i32 {
        let mut receiver = self.0.subscribe();
        loop {
            let signal = *receiver.borrow_and_update();
            if signal != 0 {
                return signal;
            }
            // self retains the sender throughout this method.
            let _ = receiver.changed().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_signal_is_shared_and_wakes_existing_and_late_waiters() {
        let cancel = Cancellation::default();
        assert_eq!(cancel.signum(), None);
        assert!(cancel.check().is_ok());
        let observer = cancel.clone();
        let wait = tokio::spawn(async move { observer.cancelled().await });
        cancel.request(libc::SIGTERM);
        cancel.request(libc::SIGINT);
        assert_eq!(wait.await.unwrap(), libc::SIGTERM);
        assert_eq!(cancel.cancelled().await, libc::SIGTERM);
        assert!(matches!(
            cancel.check(),
            Err(InputError::Cancelled(libc::SIGTERM))
        ));
    }

    #[test]
    fn unsupported_signals_use_the_interrupt_cancellation_code() {
        let cancel = Cancellation::default();
        cancel.request(0);
        assert_eq!(cancel.signum(), Some(libc::SIGINT));
    }
}
