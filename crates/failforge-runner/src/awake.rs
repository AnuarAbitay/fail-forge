//! Elapsed time that stops while the system sleeps or hibernates.
//!
//! Windows `Instant` keeps counting through sleep, so a command deadline set
//! before sleep expired as soon as the machine woke. Interrupt time has a
//! biased form, which includes sleep, and an unbiased form, which does not.
//! Linux `CLOCK_MONOTONIC` and macOS `CLOCK_UPTIME_RAW` already stop during
//! sleep, so there `Instant` is awake time and no suspension is observed.
use std::time::Duration;

/// A point in awake time, with the total time at the same moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwakeInstant {
    awake: Duration,
    total: Duration,
}

impl AwakeInstant {
    pub fn now() -> Self {
        let (awake, total) = platform::now();
        Self { awake, total }
    }

    /// Time the system was running since `self`.
    pub fn awake_elapsed(&self) -> Duration {
        Self::now().awake_since(self)
    }

    /// Time the system spent asleep or hibernated since `self`.
    pub fn suspended_elapsed(&self) -> Duration {
        Self::now().suspended_since(self)
    }

    fn awake_since(&self, earlier: &Self) -> Duration {
        self.awake.saturating_sub(earlier.awake)
    }

    fn suspended_since(&self, earlier: &Self) -> Duration {
        self.total
            .saturating_sub(earlier.total)
            .saturating_sub(self.awake_since(earlier))
    }
}

/// The awake-time limit of [`awake_timeout`] passed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwakeTimeout;

/// The longest wall-clock wait before awake time is read again.
const RECHECK: Duration = Duration::from_secs(1);

/// Like `tokio::time::timeout`, but the limit counts only awake time. Tokio
/// timers follow `Instant`, so on Windows a request pending across sleep was
/// timed out on wake although the other side had no time to answer.
pub async fn awake_timeout<F: Future>(
    limit: Duration,
    future: F,
) -> Result<F::Output, AwakeTimeout> {
    let start = AwakeInstant::now();
    awake_timeout_with(limit, future, || start.awake_elapsed()).await
}

async fn awake_timeout_with<F: Future>(
    limit: Duration,
    future: F,
    elapsed: impl Fn() -> Duration,
) -> Result<F::Output, AwakeTimeout> {
    tokio::pin!(future);
    loop {
        let remaining = limit.saturating_sub(elapsed());
        tokio::select! {
            biased;
            output = &mut future => return Ok(output),
            () = tokio::time::sleep(remaining.min(RECHECK)) => {
                if elapsed() >= limit {
                    return Err(AwakeTimeout);
                }
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;
    use windows_sys::Win32::System::WindowsProgramming::{
        QueryInterruptTimePrecise, QueryUnbiasedInterruptTimePrecise,
    };

    /// Both clocks count 100 ns interrupt-time units since boot.
    pub(super) fn now() -> (Duration, Duration) {
        let (mut unbiased, mut biased) = (0_u64, 0_u64);
        // SAFETY: each call writes one u64 through a valid, exclusive pointer.
        unsafe {
            QueryUnbiasedInterruptTimePrecise(&mut unbiased);
            QueryInterruptTimePrecise(&mut biased);
        }
        let units = |value: u64| Duration::from_nanos(value.saturating_mul(100));
        (units(unbiased), units(biased))
    }
}

#[cfg(unix)]
mod platform {
    use std::sync::LazyLock;
    use std::time::{Duration, Instant};

    static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

    pub(super) fn now() -> (Duration, Duration) {
        let awake = ORIGIN.elapsed();
        (awake, awake)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(awake: u64, total: u64) -> AwakeInstant {
        AwakeInstant {
            awake: Duration::from_secs(awake),
            total: Duration::from_secs(total),
        }
    }

    #[test]
    fn sleep_counts_as_suspended_rather_than_awake_time() {
        let start = at(100, 100);
        let woke = at(130, 10_930);
        assert_eq!(woke.awake_since(&start), Duration::from_secs(30));
        assert_eq!(woke.suspended_since(&start), Duration::from_secs(10_800));
        let busy = at(130, 130);
        assert_eq!(busy.awake_since(&start), Duration::from_secs(30));
        assert_eq!(busy.suspended_since(&start), Duration::ZERO);
    }

    #[test]
    fn readings_out_of_order_never_underflow() {
        let start = at(100, 200);
        let earlier = at(90, 150);
        assert_eq!(earlier.awake_since(&start), Duration::ZERO);
        assert_eq!(earlier.suspended_since(&start), Duration::ZERO);
        assert_eq!(at(110, 205).suspended_since(&start), Duration::ZERO);
    }

    #[tokio::test]
    async fn awake_timeout_returns_output_or_expires_after_awake_time() {
        let limit = Duration::from_millis(50);
        assert_eq!(awake_timeout(limit, async { 7 }).await, Ok(7));
        let begin = std::time::Instant::now();
        assert_eq!(
            awake_timeout(limit, std::future::pending::<()>()).await,
            Err(AwakeTimeout)
        );
        assert!(begin.elapsed() >= Duration::from_millis(40));
        // A zero limit still polls the future once, like tokio::time::timeout.
        assert_eq!(awake_timeout(Duration::ZERO, async { 1 }).await, Ok(1));
        assert_eq!(
            awake_timeout(Duration::ZERO, std::future::pending::<()>()).await,
            Err(AwakeTimeout)
        );
    }

    #[tokio::test]
    async fn wall_time_without_awake_time_never_expires_the_limit() {
        // A frozen awake clock models the machine sleeping: wall time passes,
        // timers fire, and the limit must still not be reached.
        let pending = awake_timeout_with(
            Duration::from_millis(10),
            std::future::pending::<()>(),
            || Duration::ZERO,
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(300), pending)
                .await
                .is_err()
        );
        // Once awake time catches up, the limit expires at the next check.
        let ticks = std::cell::Cell::new(0_u64);
        let woke = awake_timeout_with(Duration::from_secs(5), std::future::pending::<()>(), || {
            ticks.set(ticks.get() + 1);
            if ticks.get() > 2 {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            }
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), woke).await,
            Ok(Err(AwakeTimeout))
        );
    }

    #[test]
    fn live_clock_advances_and_reports_no_sleep_during_a_short_wait() {
        let start = AwakeInstant::now();
        std::thread::sleep(Duration::from_millis(50));
        let elapsed = start.awake_elapsed();
        assert!(elapsed >= Duration::from_millis(40), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
        assert!(start.suspended_elapsed() < Duration::from_millis(500));
    }
}
