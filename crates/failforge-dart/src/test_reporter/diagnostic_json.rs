//! Dart diagnostics may contain unpaired UTF-16 units. Keep them losslessly in
//! Rust strings without relaxing identities, keys or private context documents.
//!
//! U+FDD0 is an escape marker: a literal marker becomes two markers, and an
//! orphan code unit becomes marker + `s` + four lowercase hexadecimal digits.
//! This is injective, unlike U+FFFD replacement or a plain `\\ud83d` spelling.
//! Ordinary Unicode, valid pairs, literal backslashes and Windows paths retain
//! their existing spelling. Raw capture bytes are never changed.
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde_json::{Value, value::RawValue};

use super::{EvidenceError, MAX_EVENT_BYTES, Result, json_error, parse_json};

const MARKER: char = '\u{fdd0}';

fn fields(kind: Option<&str>) -> &'static [&'static str] {
    match kind {
        Some("error") => &["error", "stackTrace"],
        Some("print") => &["message"],
        _ => &[],
    }
}

pub(super) fn canonicalize(value: &mut Value, raw_length: usize) -> Result<()> {
    let selected = fields(value.get("type").and_then(Value::as_str));
    let mut growth = 0usize;
    for key in selected {
        if let Some(Value::String(text)) = value.get_mut(*key) {
            let markers = text
                .chars()
                .filter(|character| *character == MARKER)
                .count();
            growth = growth.saturating_add(markers.saturating_mul(MARKER.len_utf8()));
            if raw_length.saturating_add(growth) > MAX_EVENT_BYTES {
                return Err(EvidenceError::invalid("event_limit_exceeded"));
            }
            if markers != 0 {
                *text = text.replace(MARKER, &format!("{MARKER}{MARKER}"));
            }
        }
    }
    Ok(())
}

/// RawValue validates framing/escape syntax while borrowing field values. The
/// strict full-document parse below still owns numeric, depth, duplicate-key
/// and Unicode validation for every field outside the diagnostic allowlist.
pub(super) fn repair(bytes: &[u8]) -> Result<Value> {
    // A malformed strict parse can also mean invalid wire UTF-8, so enforce it
    // again before using RawValue / the intentionally permissive byte decoder.
    if bytes.len() > MAX_EVENT_BYTES {
        return Err(EvidenceError::invalid("event_limit_exceeded"));
    }
    std::str::from_utf8(bytes).map_err(|_| EvidenceError("malformed_reporter_output", true))?;
    let event: RawEvent<'_> = serde_json::from_slice(bytes).map_err(json_error)?;
    let kind = event
        .0
        .iter()
        .find(|(key, _)| key == "type")
        .and_then(|(_, raw)| serde_json::from_str::<String>(raw.get()).ok());
    let selected = fields(kind.as_deref());
    let mut repaired = String::with_capacity(bytes.len());
    repaired.push('{');
    for (index, (key, raw)) in event.0.iter().enumerate() {
        if index != 0 {
            repaired.push(',');
        }
        let name = serde_json::to_string(key).expect("JSON object key");
        repaired.push_str(&name);
        repaired.push(':');
        if selected.contains(&key.as_str()) && raw.get().starts_with('"') {
            let text: DiagnosticText = serde_json::from_str(raw.get()).map_err(json_error)?;
            let text = serde_json::to_string(&text.0).expect("canonical diagnostic text");
            repaired.push_str(&text);
        } else {
            repaired.push_str(raw.get());
        }
        if repaired.len() >= MAX_EVENT_BYTES {
            return Err(EvidenceError::invalid("event_limit_exceeded"));
        }
    }
    repaired.push('}');
    // Do not call parse_event: canonical diagnostic markers must be escaped
    // exactly once. Private contexts also reread these strings without recoding.
    parse_json(repaired.as_bytes(), MAX_EVENT_BYTES)
}

struct RawEvent<'a>(Vec<(String, &'a RawValue)>);

impl<'de> Deserialize<'de> for RawEvent<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        deserializer.deserialize_map(RawEventVisitor)
    }
}

struct RawEventVisitor;

impl<'de> Visitor<'de> for RawEventVisitor {
    type Value = RawEvent<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a reporter event object")
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> std::result::Result<Self::Value, A::Error> {
        let mut fields = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            fields.push((key, map.next_value::<&'de RawValue>()?));
        }
        Ok(RawEvent(fields))
    }
}

struct DiagnosticText(String);

impl<'de> Deserialize<'de> for DiagnosticText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        // serde_json explicitly supports orphan surrogate escapes through its
        // byte-string decoder, returning WTF-8 for those code units.
        deserializer.deserialize_bytes(DiagnosticVisitor)
    }
}

struct DiagnosticVisitor;

impl<'de> Visitor<'de> for DiagnosticVisitor {
    type Value = DiagnosticText;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a Dart diagnostic string")
    }

    fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> std::result::Result<Self::Value, E> {
        encode_wtf8(bytes).map(DiagnosticText).map_err(E::custom)
    }
}

fn encode_wtf8(mut bytes: &[u8]) -> std::result::Result<String, &'static str> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len());
    while !bytes.is_empty() {
        let valid = match std::str::from_utf8(bytes) {
            Ok(text) => {
                append_scalars(&mut output, text);
                break;
            }
            Err(error) => error.valid_up_to(),
        };
        let prefix = std::str::from_utf8(&bytes[..valid]).expect("validated UTF-8 prefix");
        append_scalars(&mut output, prefix);
        let tail = &bytes[valid..];
        if tail.len() < 3
            || tail[0] != 0xed
            || !(0xa0..=0xbf).contains(&tail[1])
            || !(0x80..=0xbf).contains(&tail[2])
        {
            return Err("invalid diagnostic Unicode");
        }
        let unit = (u16::from(tail[0] & 0x0f) << 12)
            | (u16::from(tail[1] & 0x3f) << 6)
            | u16::from(tail[2] & 0x3f);
        output.push(MARKER);
        output.push('s');
        for shift in [12, 8, 4, 0] {
            output.push(HEX[usize::from((unit >> shift) & 0xf)] as char);
        }
        bytes = &tail[3..];
        if output.len() > MAX_EVENT_BYTES {
            return Err("event_limit_exceeded");
        }
    }
    if output.len() > MAX_EVENT_BYTES {
        return Err("event_limit_exceeded");
    }
    Ok(output)
}

fn append_scalars(output: &mut String, text: &str) {
    for character in text.chars() {
        output.push(character);
        if character == MARKER {
            output.push(MARKER);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse_event;
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_code_units_are_lossless_and_field_order_independent() {
        let raw = br#"{"error":"\ud83d","stackTrace":"\udc6d","type":"error"}"#;
        let parsed = parse_event(raw).unwrap();
        assert_eq!(parsed["error"], format!("{MARKER}sd83d"));
        assert_eq!(parsed["stackTrace"], format!("{MARKER}sdc6d"));
        assert_eq!(
            parse_json(raw, MAX_EVENT_BYTES).unwrap_err().0,
            "malformed_reporter_output"
        );
        let upper = parse_event(br#"{"type":"error","error":"\uD83D"}"#).unwrap();
        assert_eq!(upper["error"], parsed["error"]);
    }

    #[test]
    fn encoded_orphans_cannot_collide_with_literal_text_or_markers() {
        let orphan = parse_event(br#"{"type":"error","error":"\ud83d"}"#).unwrap();
        for original in [
            "\\ud83d".to_owned(),
            "�".to_owned(),
            format!("{MARKER}sd83d"),
        ] {
            let raw = serde_json::to_vec(&json!({"type":"error","error":original})).unwrap();
            let literal = parse_event(&raw).unwrap();
            assert_ne!(orphan["error"], literal["error"]);
        }
        let literal =
            json!({"type":"error","error":format!("a{MARKER}b"),"stackTrace":"C:\\src\\test.dart"});
        let encoded = parse_event(&serde_json::to_vec(&literal).unwrap()).unwrap();
        assert_eq!(encoded["error"], format!("a{MARKER}{MARKER}b"));
        assert_eq!(encoded["stackTrace"], literal["stackTrace"]);
        // Private context transport must preserve already-canonical text.
        assert_eq!(
            parse_json(&serde_json::to_vec(&encoded).unwrap(), MAX_EVENT_BYTES).unwrap(),
            encoded
        );
    }

    #[test]
    fn ordinary_unicode_pairs_and_print_diagnostics_remain_supported() {
        let paired = parse_event(br#"{"type":"error","error":"\ud83d\udc6d"}"#).unwrap();
        assert_eq!(paired["error"], "👭");
        let printed = parse_event(br#"{"type":"print","message":"\ud83d"}"#).unwrap();
        assert_eq!(printed["message"], format!("{MARKER}sd83d"));
        let escaped = parse_event(br#"{"type":"error","err\u006fr":"\ud83d"}"#).unwrap();
        assert_eq!(escaped["error"], printed["message"]);
    }

    #[test]
    fn repairing_diagnostics_never_relaxes_protocol_or_unknown_fields() {
        for raw in [
            br#"{"type":"error","error":"\ud83d","testID":"\ud800"}"#.as_slice(),
            br#"{"type":"error","error":"\ud83d","unknown":{"message":"\ud800"}}"#,
            br#"{"type":"error","error":"\ud83d","\ud800":"key"}"#,
            br#"{"type":"start","error":"\ud83d"}"#,
            br#"{"type":"error","message":"\ud83d"}"#,
            br#"{"error":"\ud83d","type":"\ud800"}"#,
        ] {
            assert_eq!(parse_event(raw).unwrap_err().0, "malformed_reporter_output");
        }
        assert_eq!(
            parse_event(br#"{"type":"error","error":"\ud83d","err\u006fr":"duplicate"}"#)
                .unwrap_err()
                .0,
            "duplicate_json_key"
        );
        for raw in [
            b"{\"type\":\"error\",\"error\":\"\xed\xa0\x80\"}".as_slice(),
            b"{\"type\":\"print\",\"message\":\"\xff\"}",
        ] {
            assert_eq!(parse_event(raw).unwrap_err().0, "malformed_reporter_output");
        }
        assert_eq!(
            parse_event(br#"{"type":"error","error":"\ud83d","unknown":18446744073709551616}"#)
                .unwrap_err()
                .0,
            "integer_limit_exceeded"
        );
        assert_eq!(
            parse_event(br#"{"type":"error","error":"\ud83d","unknown":{"n":1,"n":2}}"#)
                .unwrap_err()
                .0,
            "duplicate_json_key"
        );
    }

    #[test]
    fn diagnostic_repair_keeps_depth_and_event_limits() {
        let deep = format!(
            "{{\"type\":\"error\",\"error\":\"\\ud83d\",\"unknown\":{}0{}}}",
            "[".repeat(256),
            "]".repeat(256)
        );
        let error = parse_event(deep.as_bytes()).unwrap_err();
        assert_eq!(error.0, "malformed_reporter_output");
        assert!(!error.is_interrupted_candidate());
        let markers = MARKER.to_string().repeat(MAX_EVENT_BYTES / 4);
        let encoded = serde_json::to_vec(&json!({"type":"error","error":markers})).unwrap();
        assert_eq!(parse_event(&encoded).unwrap_err().0, "event_limit_exceeded");
        let fallback = format!("{{\"type\":\"error\",\"error\":\"{markers}\\ud83d\"}}");
        let error = parse_event(fallback.as_bytes()).unwrap_err();
        assert_eq!(error.0, "event_limit_exceeded");
        assert!(!error.is_interrupted_candidate());
    }
}
