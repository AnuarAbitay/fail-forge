use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const PROTOCOL_VERSION: u32 = 1;
/// Maximum JSON bytes, excluding the final LF delimiter.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_JSON_INTEGER: u64 = (1_u64 << 53) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterCommand {
    Handshake,
    Discover,
    Validate,
}

impl AdapterCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::Discover => "discover",
            Self::Validate => "validate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseKind {
    Handshake,
    Discover,
    Validate,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub command: AdapterCommand,
    pub payload: Value,
}

impl RequestEnvelope {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_header(self.protocol_version, &self.request_id)?;
        if !self.payload.is_object() {
            return Err(ProtocolError::InvalidRequestPayload);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterFailure {
    pub code: String,
    pub message: String,
    #[serde(
        default,
        deserialize_with = "present_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub kind: ResponseKind,
    #[serde(
        default,
        deserialize_with = "present_payload",
        skip_serializing_if = "Option::is_none"
    )]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AdapterFailure>,
}

fn present_payload<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

impl ResponseEnvelope {
    pub fn validate_for(
        &self,
        request_id: &str,
        command: AdapterCommand,
    ) -> Result<(), ProtocolError> {
        validate_header(self.protocol_version, &self.request_id)?;
        if self.request_id != request_id {
            return Err(ProtocolError::RequestIdMismatch);
        }
        let expected = match command {
            AdapterCommand::Handshake => ResponseKind::Handshake,
            AdapterCommand::Discover => ResponseKind::Discover,
            AdapterCommand::Validate => ResponseKind::Validate,
        };
        if self.kind == ResponseKind::Error {
            if self.payload.is_some()
                || self.error.as_ref().is_none_or(|error| {
                    error.code.is_empty()
                        || error.message.is_empty()
                        || error
                            .details
                            .as_ref()
                            .is_some_and(|details| !details.is_object())
                })
            {
                return Err(ProtocolError::InvalidResponseBody);
            }
        } else if self.kind != expected {
            return Err(ProtocolError::ResponseKindMismatch);
        } else if self
            .payload
            .as_ref()
            .is_none_or(|payload| !payload.is_object())
            || self.error.is_some()
        {
            return Err(ProtocolError::InvalidResponseBody);
        }
        Ok(())
    }
}

fn validate_header(version: u32, request_id: &str) -> Result<(), ProtocolError> {
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    if request_id.is_empty() || request_id.len() > 256 || request_id.contains('\0') {
        return Err(ProtocolError::InvalidRequestId);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationFamily {
    Classic,
    FailureMode,
    LanguageSpecific,
}

impl MutationFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::FailureMode => "failure_mode",
            Self::LanguageSpecific => "language_specific",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationState {
    Unvalidated,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ByteEdit {
    pub offset_bytes: u64,
    pub length_bytes: u64,
    pub expected: String,
    pub replacement: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Symbol {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mutant {
    pub id: String,
    pub operator_id: String,
    pub operator_version: u32,
    pub family: MutationFamily,
    pub file: String,
    pub source_hash: String,
    pub components: Vec<String>,
    pub edit: ByteEdit,
    pub symbol: Symbol,
    pub validation: ValidationState,
}

impl Mutant {
    /// Compact UTF-8 JSON array shared with Dart; metadata and state are excluded.
    pub fn stable_id(&self) -> String {
        let canonical = serde_json::json!([
            PROTOCOL_VERSION,
            self.file,
            self.source_hash,
            self.operator_id,
            self.operator_version,
            self.family.as_str(),
            self.edit.offset_bytes,
            self.edit.length_bytes,
            self.edit.expected,
            self.edit.replacement,
        ]);
        // Value serialization has no custom serializer and a Vec sink cannot
        // produce an I/O error. Strings and integer fields are valid JSON.
        sha256_hex(&serde_json::to_vec(&canonical).expect("canonical JSON values serialize"))
    }

    pub fn validate_model(&self) -> Result<(), ProtocolError> {
        if self.operator_version != 1 {
            return Err(ProtocolError::OperatorVersionMismatch);
        }
        if self.operator_id.is_empty()
            || self.operator_id.len() > 128
            || self.operator_id.contains('\0')
        {
            return Err(ProtocolError::InvalidOperatorId);
        }
        if self.components.len() > 10_000
            || self
                .components
                .iter()
                .chain(self.symbol.class_name.iter())
                .chain(self.symbol.method_name.iter())
                .any(|name| name.is_empty() || name.len() > 512)
        {
            return Err(ProtocolError::InvalidMetadata);
        }
        if self.edit.offset_bytes > MAX_JSON_INTEGER || self.edit.length_bytes > MAX_JSON_INTEGER {
            return Err(ProtocolError::InvalidEditRange);
        }
        if !is_hash(&self.source_hash) || !is_hash(&self.id) {
            return Err(ProtocolError::InvalidHash);
        }
        if self.id != self.stable_id() {
            return Err(ProtocolError::MutantIdMismatch);
        }
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn is_hash(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("incompatible adapter protocol version")]
    VersionMismatch,
    #[error("request ID must be nonempty, at most 256 UTF-8 bytes, and contain no NUL")]
    InvalidRequestId,
    #[error("adapter request payload must be an object")]
    InvalidRequestPayload,
    #[error("adapter response request ID mismatch")]
    RequestIdMismatch,
    #[error("adapter response kind mismatch")]
    ResponseKindMismatch,
    #[error("adapter response must contain exactly its appropriate payload or structured error")]
    InvalidResponseBody,
    #[error("unsupported mutation operator version")]
    OperatorVersionMismatch,
    #[error("invalid mutation operator ID")]
    InvalidOperatorId,
    #[error("mutation components and symbols must be nonempty and at most 512 UTF-8 bytes")]
    InvalidMetadata,
    #[error("mutation byte offsets and lengths must be at most 2^53-1")]
    InvalidEditRange,
    #[error("hashes must be lowercase SHA-256 hex")]
    InvalidHash,
    #[error("mutant ID does not match its canonical patch")]
    MutantIdMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_envelopes_and_null_payload_presence() {
        let valid = r#"{"protocolVersion":1,"requestId":"1","kind":"handshake","payload":{}}"#;
        let response: ResponseEnvelope = serde_json::from_str(valid).unwrap();
        response
            .validate_for("1", AdapterCommand::Handshake)
            .unwrap();
        for invalid in [
            valid.replace("\"payload\":{}", "\"extra\":true"),
            valid.replace(
                "\"requestId\":\"1\"",
                "\"requestId\":\"1\",\"requestId\":\"2\"",
            ),
            valid.replace("handshake", "unknown"),
        ] {
            assert!(serde_json::from_str::<ResponseEnvelope>(&invalid).is_err());
        }
        let null: ResponseEnvelope =
            serde_json::from_str(&valid.replace("\"payload\":{}", "\"payload\":null")).unwrap();
        assert_eq!(null.payload, Some(Value::Null));
        assert_eq!(
            null.validate_for("1", AdapterCommand::Handshake),
            Err(ProtocolError::InvalidResponseBody)
        );
        let missing: ResponseEnvelope =
            serde_json::from_str(&valid.replace(",\"payload\":{}", "")).unwrap();
        assert_eq!(missing.payload, None);
        assert_eq!(
            missing.validate_for("1", AdapterCommand::Handshake),
            Err(ProtocolError::InvalidResponseBody)
        );
        assert!(matches!(
            response.validate_for("2", AdapterCommand::Handshake),
            Err(ProtocolError::RequestIdMismatch)
        ));
        assert!(matches!(
            response.validate_for("1", AdapterCommand::Validate),
            Err(ProtocolError::ResponseKindMismatch)
        ));
    }
    #[test]
    fn header_and_error_body_contract() {
        for (version, id, valid) in [
            (1, "1", true),
            (2, "1", false),
            (1, "", false),
            (1, "a\0b", false),
        ] {
            assert_eq!(
                RequestEnvelope {
                    protocol_version: version,
                    request_id: id.into(),
                    command: AdapterCommand::Discover,
                    payload: serde_json::json!({})
                }
                .validate()
                .is_ok(),
                valid
            );
        }
        let mut response = ResponseEnvelope {
            protocol_version: 1,
            request_id: "1".into(),
            kind: ResponseKind::Error,
            payload: None,
            error: Some(AdapterFailure {
                code: "bad_request".into(),
                message: "invalid parameters".into(),
                details: None,
            }),
        };
        response
            .validate_for("1", AdapterCommand::Handshake)
            .unwrap();
        response.payload = Some(Value::Null);
        assert_eq!(
            response.validate_for("1", AdapterCommand::Handshake),
            Err(ProtocolError::InvalidResponseBody)
        );
        response.payload = None;
        response.error = None;
        assert_eq!(
            response.validate_for("1", AdapterCommand::Handshake),
            Err(ProtocolError::InvalidResponseBody)
        );
    }
    #[test]
    fn digest_matches_standard_sha256_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn requests_and_success_responses_require_object_payloads() {
        for payload in [
            Value::Null,
            serde_json::json!(false),
            serde_json::json!(1),
            serde_json::json!("value"),
            serde_json::json!([]),
        ] {
            let request = RequestEnvelope {
                protocol_version: 1,
                request_id: "1".into(),
                command: AdapterCommand::Handshake,
                payload: payload.clone(),
            };
            assert!(
                request.validate().is_err(),
                "non-object request payload accepted: {payload}"
            );
            let response = ResponseEnvelope {
                protocol_version: 1,
                request_id: "1".into(),
                kind: ResponseKind::Handshake,
                payload: Some(payload.clone()),
                error: None,
            };
            assert!(
                response
                    .validate_for("1", AdapterCommand::Handshake)
                    .is_err(),
                "non-object success payload accepted: {payload}"
            );
        }
        let request = RequestEnvelope {
            protocol_version: 1,
            request_id: "1".into(),
            command: AdapterCommand::Handshake,
            payload: serde_json::json!({}),
        };
        request.validate().unwrap();
        let response = ResponseEnvelope {
            protocol_version: 1,
            request_id: "1".into(),
            kind: ResponseKind::Handshake,
            payload: Some(serde_json::json!({})),
            error: None,
        };
        response
            .validate_for("1", AdapterCommand::Handshake)
            .unwrap();
    }
    #[test]
    fn structured_error_details_require_an_object_when_present() {
        for details in [
            Value::Null,
            serde_json::json!(false),
            serde_json::json!(1),
            serde_json::json!("value"),
            serde_json::json!([]),
        ] {
            let encoded = serde_json::json!({"protocolVersion":1,"requestId":"1","kind":"error","error":{"code":"invalid_request","message":"invalid parameters","details":details}});
            let response: ResponseEnvelope = serde_json::from_value(encoded).unwrap();
            assert_eq!(
                response.error.as_ref().unwrap().details,
                Some(details.clone())
            );
            assert_eq!(
                response.validate_for("1", AdapterCommand::Handshake),
                Err(ProtocolError::InvalidResponseBody),
                "non-object error details accepted: {details}"
            );
        }
        for details in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({"field":"mutant"})),
        ] {
            let response = ResponseEnvelope {
                protocol_version: 1,
                request_id: "1".into(),
                kind: ResponseKind::Error,
                payload: None,
                error: Some(AdapterFailure {
                    code: "invalid_request".into(),
                    message: "invalid parameters".into(),
                    details,
                }),
            };
            response
                .validate_for("1", AdapterCommand::Handshake)
                .unwrap();
            let roundtrip: ResponseEnvelope =
                serde_json::from_value(serde_json::to_value(&response).unwrap()).unwrap();
            assert_eq!(roundtrip, response);
        }
    }
    #[test]
    fn request_ids_and_metadata_limits_count_utf8_bytes() {
        for (id, valid) in [
            ("я".repeat(128), true),
            ("я".repeat(129), false),
            ("😀".repeat(64), true),
            ("😀".repeat(65), false),
        ] {
            let request = RequestEnvelope {
                protocol_version: 1,
                request_id: id,
                command: AdapterCommand::Handshake,
                payload: serde_json::json!({}),
            };
            assert_eq!(request.validate().is_ok(), valid);
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocol/fixtures/patch.json"
        )))
        .unwrap();
        let mut mutant: Mutant = serde_json::from_value(fixture["mutant"].clone()).unwrap();
        mutant.components = vec!["я".repeat(256)];
        mutant.symbol.method_name = Some("😀".repeat(128));
        mutant.validate_model().unwrap();
        mutant.components[0].push('я');
        assert_eq!(mutant.validate_model(), Err(ProtocolError::InvalidMetadata));
        mutant.components.clear();
        mutant.symbol.method_name = Some("😀".repeat(129));
        assert_eq!(mutant.validate_model(), Err(ProtocolError::InvalidMetadata));
        mutant.symbol.method_name = Some(String::new());
        assert_eq!(mutant.validate_model(), Err(ProtocolError::InvalidMetadata));
        mutant.symbol.method_name = None;
        mutant.components = vec!["label".into(); 10_001];
        assert_eq!(mutant.validate_model(), Err(ProtocolError::InvalidMetadata));
    }
    #[test]
    fn edit_integers_match_shared_json_precision_bounds() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocol/fixtures/patch.json"
        )))
        .unwrap();
        let original: Mutant = serde_json::from_value(fixture["mutant"].clone()).unwrap();
        for value in [0, MAX_JSON_INTEGER, MAX_JSON_INTEGER + 1, u64::MAX] {
            for offset in [true, false] {
                let mut mutant = original.clone();
                if offset {
                    mutant.edit.offset_bytes = value;
                } else {
                    mutant.edit.length_bytes = value;
                }
                mutant.id = mutant.stable_id();
                assert_eq!(
                    mutant.validate_model().is_ok(),
                    value <= MAX_JSON_INTEGER,
                    "offset={offset}, value={value}"
                );
            }
        }
        for field in ["offsetBytes", "lengthBytes"] {
            let mut encoded = fixture["mutant"].clone();
            encoded["edit"][field] = serde_json::json!(-1);
            assert!(serde_json::from_value::<Mutant>(encoded).is_err());
        }
    }
}
