//! Strict JSON decoding without changing serde_json behavior for other adapter
//! protocols. The linear token guard preserves integer `-0` and bounds integer
//! lossless conversion; the visitor rejects duplicate keys even in ignored
//! metadata. Integral tokens must not fall back to serde's rounded floats.

use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use super::{EvidenceError, Result};

const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn parse_event(bytes: &[u8]) -> Result<Value> {
    parse_json(bytes, MAX_EVENT_BYTES)
}

/// Shared strict decoder for bounded private native context documents. The
/// caller chooses the framing limit; integer, depth and duplicate-key checks
/// remain identical to reporter parsing.
pub(crate) fn parse_json(bytes: &[u8], maximum_bytes: usize) -> Result<Value> {
    if bytes.len() > maximum_bytes {
        return Err(EvidenceError::invalid("event_limit_exceeded"));
    }
    std::str::from_utf8(bytes).map_err(|_| EvidenceError("malformed_reporter_output", true))?;
    let guarded = guard_numbers(bytes)?;
    let mut parser = serde_json::Deserializer::from_slice(&guarded);
    let value = UniqueValue::deserialize(&mut parser).map_err(json_error)?;
    parser.end().map_err(json_error)?;
    Ok(value.0)
}

fn json_error(error: serde_json::Error) -> EvidenceError {
    let message = error.to_string();
    if message.starts_with("duplicate_json_key") {
        EvidenceError::invalid("duplicate_json_key")
    } else if message.starts_with("non_json_number") || message.starts_with("number out of range") {
        EvidenceError::invalid("non_json_number")
    } else {
        EvidenceError(
            "malformed_reporter_output",
            !message.starts_with("recursion limit exceeded"),
        )
    }
}

fn boundary(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\n' | b'\r' | b'\t' | b',' | b'[' | b']' | b'{' | b'}' | b':'
    )
}

fn guard_numbers(bytes: &[u8]) -> Result<Cow<'_, [u8]>> {
    let mut output = Cow::Borrowed(bytes);
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"' {
            cursor += 1;
            while cursor < bytes.len() {
                match bytes[cursor] {
                    b'\\' => cursor = (cursor + 2).min(bytes.len()),
                    b'"' => {
                        cursor += 1;
                        break;
                    }
                    _ => cursor += 1,
                }
            }
            continue;
        }
        if cursor > 0 && !boundary(bytes[cursor - 1]) {
            cursor += 1;
            continue;
        }
        let tail = &bytes[cursor..];
        for constant in [b"NaN".as_slice(), b"Infinity", b"-Infinity"] {
            if tail.starts_with(constant)
                && tail.get(constant.len()).is_none_or(|byte| boundary(*byte))
            {
                return Err(EvidenceError::invalid("non_json_number"));
            }
        }
        let start = cursor;
        let digits_start = cursor + usize::from(bytes[cursor] == b'-');
        cursor = digits_start;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if cursor == digits_start {
            cursor = start + 1;
            continue;
        }
        let digits = cursor - digits_start;
        // A float must remain a float even when mathematically integral. Other
        // malformed number spellings are left to serde's grammar validation.
        let integer = bytes.get(cursor).is_none_or(|byte| boundary(*byte))
            && (digits == 1 || bytes[digits_start] != b'0');
        if integer {
            // serde_json falls back to f64 for integral tokens outside its
            // signed/unsigned integer range. That can make distinct metadata
            // identities equal, so reject before any lossy conversion occurs.
            let maximum = if digits_start == start {
                b"18446744073709551615".as_slice()
            } else {
                b"9223372036854775808".as_slice()
            };
            let magnitude = &bytes[digits_start..cursor];
            if magnitude.len() > maximum.len()
                || (magnitude.len() == maximum.len() && magnitude > maximum)
            {
                return Err(EvidenceError::invalid("integer_limit_exceeded"));
            }
            if digits_start != start && digits == 1 && bytes[digits_start] == b'0' {
                output.to_mut()[start] = b' ';
            }
        }
    }
    Ok(output)
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a strict JSON value")
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(value.into())))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Self::Value, E> {
        Number::from_f64(value)
            .map(|number| UniqueValue(Value::Number(number)))
            .ok_or_else(|| E::custom("non_json_number"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }
    fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut sequence: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut object: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            let value = object.next_value::<UniqueValue>()?;
            if values.insert(key, value.0).is_some() {
                return Err(de::Error::custom("duplicate_json_key"));
            }
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}
