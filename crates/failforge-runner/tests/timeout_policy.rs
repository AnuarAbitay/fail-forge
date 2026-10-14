use failforge_runner::{SpecError, TimeoutPolicy};
use std::time::Duration;

#[test]
fn default_timeout_policy_matches_contract() {
    let policy = TimeoutPolicy::default();

    assert_eq!(policy.timeout, None);
    assert_eq!(policy.grace_period, Duration::from_secs(2));
    assert_eq!(policy.forced_wait, Duration::from_secs(5));
    assert_eq!(policy.output_drain, Duration::from_secs(1));
}

#[test]
fn default_timeout_policy_is_valid() {
    let policy = TimeoutPolicy::default();

    assert_eq!(policy.validate(), Ok(()));
}

#[test]
fn zero_timeout_is_rejected() {
    let policy = TimeoutPolicy {
        timeout: Some(Duration::from_secs(0)),
        grace_period: Duration::from_secs(2),
        forced_wait: Duration::from_secs(5),
        output_drain: Duration::from_secs(1),
    };

    assert_eq!(policy.validate(), Err(SpecError::ZeroTimeout));
}

#[test]
fn positive_subsecond_timeout_is_accepted() {
    let policy = TimeoutPolicy {
        timeout: Some(Duration::from_nanos(1)),
        grace_period: Duration::from_secs(2),
        forced_wait: Duration::from_secs(5),
        output_drain: Duration::from_secs(1),
    };

    assert_eq!(policy.validate(), Ok(()));
}

#[test]
fn zero_grace_period_is_accepted() {
    let policy = TimeoutPolicy {
        timeout: Some(Duration::from_secs(1)),
        grace_period: Duration::from_secs(0),
        forced_wait: Duration::from_secs(5),
        output_drain: Duration::from_secs(1),
    };

    assert_eq!(policy.validate(), Ok(()));
}

#[test]
fn zero_timeout_with_zero_grace_is_rejected() {
    let policy = TimeoutPolicy {
        timeout: Some(Duration::ZERO),
        grace_period: Duration::ZERO,
        ..TimeoutPolicy::default()
    };

    assert_eq!(policy.validate(), Err(SpecError::ZeroTimeout));
}

#[test]
fn no_timeout_with_zero_grace_is_accepted() {
    let policy = TimeoutPolicy {
        timeout: None,
        grace_period: Duration::ZERO,
        ..TimeoutPolicy::default()
    };

    assert_eq!(policy.validate(), Ok(()));
}
