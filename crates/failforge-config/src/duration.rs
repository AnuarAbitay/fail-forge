//! Dart's default per-test timeout is distinct from the native process deadline.

use crate::ConfigError;
use serde_json::{Value, json};

pub const DEFAULT_RUNNER_TIMEOUT: &str = "60s";
pub const DEFAULT_TEST_TIMEOUT: &str = "30s";
pub const MINIMUM_HEADROOM_MS: u64 = 5_000;
pub const MAX_TEST_TIMEOUT_MS: u64 = (1_u64 << 53) / 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DartTestTimeoutPolicy {
    pub runner_timeout_ms: u64,
    pub test_timeout_ms: u64,
}

impl DartTestTimeoutPolicy {
    pub fn new(runner_timeout_ms: u64, test_timeout_ms: u64) -> Result<Self, ConfigError> {
        for (value, name) in [
            (runner_timeout_ms, "runner timeout"),
            (test_timeout_ms, "test timeout"),
        ] {
            if value == 0 {
                return Err(ConfigError::new(format!(
                    "{name} must be a positive unsigned 64-bit millisecond count"
                )));
            }
        }
        if test_timeout_ms > MAX_TEST_TIMEOUT_MS {
            return Err(ConfigError::new(format!(
                "test timeout exceeds the exact Dart timeout range ({MAX_TEST_TIMEOUT_MS}ms)"
            )));
        }
        if runner_timeout_ms
            .checked_sub(test_timeout_ms)
            .is_none_or(|gap| gap < MINIMUM_HEADROOM_MS)
        {
            return Err(ConfigError::new(
                "runner timeout must exceed test timeout by at least 5000ms (5s) of headroom",
            ));
        }
        Ok(Self {
            runner_timeout_ms,
            test_timeout_ms,
        })
    }

    pub fn runner_timeout(&self) -> String {
        format!("{}ms", self.runner_timeout_ms)
    }
    pub fn test_timeout(&self) -> String {
        format!("{}ms", self.test_timeout_ms)
    }
    pub fn to_value(&self) -> Value {
        json!({"runnerTimeout": self.runner_timeout(), "testTimeout": self.test_timeout(),
               "minimumHeadroomMs": MINIMUM_HEADROOM_MS, "scope": "dart_default_per_test",
               "configurationMayOverride": true})
    }
}

pub fn milliseconds(value: &Value, name: &str) -> Result<u64, ConfigError> {
    let grammar_error = || {
        ConfigError::new(format!(
            "{name} must be an unsigned ASCII integer followed by ms, s, or m"
        ))
    };
    let text = value.as_str().ok_or_else(grammar_error)?;
    let (digits, unit) = if let Some(digits) = text.strip_suffix("ms") {
        (digits, 1_u64)
    } else if let Some(digits) = text.strip_suffix('s') {
        (digits, 1_000)
    } else if let Some(digits) = text.strip_suffix('m') {
        (digits, 60_000)
    } else {
        return Err(grammar_error());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(grammar_error());
    }
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Err(ConfigError::new(format!(
            "{name} must be greater than zero"
        )));
    }
    let maximum = (u64::MAX / unit).to_string();
    if digits.len() > maximum.len() || (digits.len() == maximum.len() && digits > maximum.as_str())
    {
        return Err(ConfigError::new(format!(
            "{name} exceeds the unsigned 64-bit millisecond range"
        )));
    }
    let magnitude = digits.parse::<u64>().map_err(|_| grammar_error())?;
    Ok(magnitude * unit)
}

pub fn resolve_timeout_policy(
    outer: &Value,
    inner: &Value,
) -> Result<DartTestTimeoutPolicy, ConfigError> {
    DartTestTimeoutPolicy::new(
        milliseconds(outer, "runner timeout")?,
        milliseconds(inner, "test timeout")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_units_leading_zeroes_and_exact_headroom() {
        let value = resolve_timeout_policy(&json!("65000ms"), &json!("0001m")).unwrap();
        assert_eq!(value.runner_timeout(), "65000ms");
        assert_eq!(value.test_timeout(), "60000ms");
        assert_eq!(
            value.to_value(),
            json!({"runnerTimeout":"65000ms", "testTimeout":"60000ms",
            "minimumHeadroomMs":5000, "scope":"dart_default_per_test", "configurationMayOverride":true})
        );
        assert!(resolve_timeout_policy(&json!("34999ms"), &json!("30s")).is_err());
        assert!(resolve_timeout_policy(&json!("1ms"), &json!("30s")).is_err());
        assert_eq!(milliseconds(&json!("0000001s"), "duration").unwrap(), 1000);
    }

    #[test]
    fn rejects_grammar_types_zero_and_overflow_without_large_integer_parsing() {
        for bad in [
            json!("0ms"),
            json!("-1s"),
            json!("+1s"),
            json!("1.5s"),
            json!("1S"),
            json!("1h"),
            json!(" 1s"),
            json!("1s\n"),
            json!("١s"),
            json!(""),
            json!(true),
            json!(30),
            Value::Null,
            json!("18446744073709551616ms"),
            json!(format!("{}s", "9".repeat(10_000))),
        ] {
            assert!(milliseconds(&bad, "duration").is_err(), "{bad}");
        }
        assert_eq!(
            milliseconds(&json!(format!("{}ms", u64::MAX)), "duration").unwrap(),
            u64::MAX
        );
        assert!(milliseconds(&json!(format!("{}s", u64::MAX / 1000 + 1)), "duration").is_err());
        assert!(DartTestTimeoutPolicy::new(0, 1).is_err());
        assert!(DartTestTimeoutPolicy::new(5000, 0).is_err());
    }

    #[test]
    fn preserves_exact_dart_range_and_full_native_u64_range() {
        let exact =
            DartTestTimeoutPolicy::new(MAX_TEST_TIMEOUT_MS + 5000, MAX_TEST_TIMEOUT_MS).unwrap();
        assert_eq!(exact.test_timeout_ms, MAX_TEST_TIMEOUT_MS);
        assert!(DartTestTimeoutPolicy::new(u64::MAX, MAX_TEST_TIMEOUT_MS + 1).is_err());
        assert_eq!(
            DartTestTimeoutPolicy::new(u64::MAX, 1)
                .unwrap()
                .runner_timeout_ms,
            u64::MAX
        );
    }
}
