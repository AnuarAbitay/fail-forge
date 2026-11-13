//! Exact percentage thresholds and conservative bounds for mutation quality gates.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ConfigError;

/// A percentage in hundredths, avoiding rounded floating point comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScoreThreshold(u16);

impl ScoreThreshold {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let invalid = || {
            ConfigError::new(
                "minimum score must be between 0 and 100, with at most two decimal places",
            )
        };
        if text.is_empty() || text.len() > 6 {
            return Err(invalid());
        }
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        if whole.is_empty()
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() > 2
            || text.ends_with('.')
        {
            return Err(invalid());
        }
        let whole: u16 = whole.parse().map_err(|_| invalid())?;
        let fraction: u16 = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<u16>().map_err(|_| invalid())?
                * if fraction.len() == 1 { 10 } else { 1 }
        };
        let units = whole.checked_mul(100).and_then(|n| n.checked_add(fraction));
        units.filter(|n| *n <= 10_000).map(Self).ok_or_else(invalid)
    }

    pub fn from_value(value: &Value) -> Result<Self, ConfigError> {
        match value {
            Value::Number(number) => Self::parse(&number.to_string()),
            _ => Err(ConfigError::new(
                "minimum score must be a number between 0 and 100",
            )),
        }
    }

    pub fn basis_points(self) -> u16 {
        self.0
    }

    pub fn percent(self) -> f64 {
        f64::from(self.0) / 100.0
    }

    pub fn passes(self, killed: u64, survived: u64) -> bool {
        let denominator = u128::from(killed) + u128::from(survived);
        denominator != 0 && 10_000 * u128::from(killed) >= u128::from(self.0) * denominator
    }

    /// Assume every remaining candidate is killed. Only a maximum strictly below
    /// the threshold proves failure; equality can still satisfy it.
    pub fn unreachable(self, killed: u64, survived: u64, remaining: u64) -> bool {
        let maximum_killed = u128::from(killed) + u128::from(remaining);
        let denominator = maximum_killed + u128::from(survived);
        denominator != 0 && 10_000 * maximum_killed < u128::from(self.0) * denominator
    }
}

impl Serialize for ScoreThreshold {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.percent())
    }
}

impl<'de> Deserialize<'de> for ScoreThreshold {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_value(&Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// The effective gate attached to one independently scored mutation family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityGate {
    pub min_score: Option<ScoreThreshold>,
    pub stop_when_unreachable: bool,
}

impl<'de> Deserialize<'de> for QualityGate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Input {
            #[serde(default)]
            min_score: Option<ScoreThreshold>,
            #[serde(default)]
            stop_when_unreachable: bool,
        }
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "quality gate requires a named object",
            ));
        }
        let input: Input = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        let result = Self {
            min_score: input.min_score,
            stop_when_unreachable: input.stop_when_unreachable,
        };
        result.validate().map_err(serde::de::Error::custom)?;
        Ok(result)
    }
}

impl QualityGate {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.stop_when_unreachable && self.min_score.is_none() {
            return Err(ConfigError::new(
                "stop_when_unreachable requires a minimum score",
            ));
        }
        Ok(())
    }

    /// Projection used in an individual campaign's effective configuration.
    pub fn to_value(self) -> Value {
        serde_json::json!({
            "min_score":self.min_score,
            "stop_when_unreachable":self.stop_when_unreachable
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thresholds_are_bounded_decimal_numbers_with_exact_roundtrips() {
        for (text, units) in [
            ("0", 0),
            ("0.01", 1),
            ("0.1", 10),
            ("79.99", 7999),
            ("80", 8000),
            ("100.00", 10000),
        ] {
            let threshold = ScoreThreshold::parse(text).unwrap();
            assert_eq!(threshold.basis_points(), units);
            assert_eq!(
                serde_json::from_value::<ScoreThreshold>(json!(threshold)).unwrap(),
                threshold
            );
        }
        for text in [
            "", "-1", "+1", "101", "100.01", "80.001", "1e2", "NaN", "inf", " 80", "80 ", ".5",
            "80.", "0.0.1", "65536",
        ] {
            assert!(ScoreThreshold::parse(text).is_err(), "{text}");
        }
        for value in [
            Value::Null,
            json!("80"),
            json!(true),
            json!([]),
            json!({}),
            json!(80.001),
            json!(-0.01),
        ] {
            assert!(ScoreThreshold::from_value(&value).is_err(), "{value}");
        }
    }

    #[test]
    fn comparison_uses_counters_without_display_rounding_or_overflow() {
        let threshold = ScoreThreshold::parse("80").unwrap();
        assert!(threshold.passes(80, 20));
        assert!(!threshold.passes(79, 21));
        // This would display as 80.00% but is still below the threshold.
        assert!(!threshold.passes(79_999, 20_001));
        assert!(!threshold.passes(0, 0));
        assert!(!ScoreThreshold::parse("0").unwrap().passes(0, 0));
        assert!(
            ScoreThreshold::parse("50")
                .unwrap()
                .passes(u64::MAX, u64::MAX)
        );
    }

    #[test]
    fn upper_bound_only_stops_when_recovery_is_impossible() {
        let threshold = ScoreThreshold::parse("80").unwrap();
        assert!(!threshold.unreachable(5, 5, 90));
        assert!(!threshold.unreachable(60, 20, 20));
        assert!(threshold.unreachable(60, 25, 15));
        assert!(!threshold.unreachable(0, 0, 0));
        assert!(threshold.unreachable(u64::MAX, u64::MAX, u64::MAX));
        assert!(ScoreThreshold::parse("100").unwrap().unreachable(0, 1, 99));
    }

    #[test]
    fn runtime_gates_are_named_strict_objects_and_stop_requires_a_threshold() {
        let value = json!({"minScore":80.0,"stopWhenUnreachable":true});
        let gate: QualityGate = serde_json::from_value(value.clone()).unwrap();
        gate.validate().unwrap();
        assert_eq!(json!(gate), value);
        assert!(
            QualityGate {
                min_score: None,
                stop_when_unreachable: true
            }
            .validate()
            .is_err()
        );
        assert!(serde_json::from_value::<QualityGate>(json!({"minScore":80,"extra":1})).is_err());
        assert!(serde_json::from_value::<QualityGate>(json!([80, true])).is_err());
        assert!(
            serde_json::from_value::<QualityGate>(json!({"stopWhenUnreachable":true})).is_err()
        );
    }
}
