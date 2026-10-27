//! Strict shared transcript validation, independent of the process transport.
use super::{FamilyApplicability, MAX_DISCOVERY_CANDIDATES, failure, inputs};
use crate::ProfileError;
use failforge_dart::{Mutant, ValidationState, read_and_apply_mutant, sha256_hex};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::ExitStatus;

pub(super) fn with_diagnostics(error: ProfileError, output: &Path, family: &str) -> ProfileError {
    if matches!(error, ProfileError::Cancelled(_)) {
        return error;
    }
    let printable = |text: &str| {
        text.chars()
            .map(|character| {
                if character.is_control() || matches!(character,
                    '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut message = format!(
        "applicability analysis failed for {family}: {}",
        printable(&error.to_string())
    );
    if let Ok(bytes) = inputs::read_regular_prefix(&output.join("adapter.stderr"), 4096) {
        let detail = printable(&String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]));
        if !detail.is_empty() {
            message.push_str("; adapter stderr: ");
            message.push_str(&detail);
        }
        if bytes.len() > 4096 {
            message.push_str(" [truncated]");
        }
    }
    failure(message)
}

pub(super) struct Transcript<'a> {
    package: &'a Path,
    family: &'a str,
    requested: &'a [String],
    source: &'a str,
    phase: u8,
    metadata: BTreeMap<String, String>,
    mutants: BTreeMap<String, Value>,
    seen: BTreeSet<String>,
    valid: BTreeMap<String, usize>,
    cleanup: Option<Value>,
    discovered: bool,
}

impl<'a> Transcript<'a> {
    pub(super) fn new(
        package: &'a Path,
        family: &'a str,
        requested: &'a [String],
        source: &'a str,
    ) -> Self {
        Self {
            package,
            family,
            requested,
            source,
            phase: 0,
            metadata: BTreeMap::new(),
            mutants: BTreeMap::new(),
            seen: BTreeSet::new(),
            valid: BTreeMap::new(),
            cleanup: None,
            discovered: false,
        }
    }

    pub(super) fn accept(&mut self, bytes: &[u8]) -> Result<(), ProfileError> {
        let event = inputs::parse_json(bytes)?;
        let object = event
            .as_object()
            .ok_or_else(|| failure("invalid adapter event"))?;
        if object.len() != 2
            || !object.contains_key("step")
            || !object.contains_key("payload")
            || !event["payload"].is_object()
            || self.phase == 3
        {
            return Err(failure("malformed or nonterminal adapter cleanup record"));
        }
        let payload = &event["payload"];
        match (self.phase, event["step"].as_str()) {
            (_, Some("cleanup")) => {
                self.cleanup = Some(payload.clone());
                self.phase = 3;
            }
            (0, Some("handshake")) => {
                self.handshake(payload)?;
                self.phase = 1;
            }
            (1, Some("discover")) => {
                self.discovery(payload)?;
                self.phase = 2;
                self.discovered = true;
            }
            (2, Some("candidate")) => self.candidate(payload)?,
            _ => return Err(failure("adapter transcript steps out of order")),
        }
        Ok(())
    }

    fn handshake(&mut self, value: &Value) -> Result<(), ProfileError> {
        for operator in value["operators"]
            .as_array()
            .ok_or_else(|| failure("adapter handshake has no operators"))?
        {
            let id = operator["id"]
                .as_str()
                .ok_or_else(|| failure("malformed adapter operator"))?;
            let family = operator["family"]
                .as_str()
                .ok_or_else(|| failure("malformed adapter family"))?;
            if self.metadata.insert(id.into(), family.into()).is_some() {
                return Err(failure("duplicate adapter operator"));
            }
        }
        for operator in self.requested {
            if self.metadata.get(operator).map(String::as_str) != Some(self.family) {
                return Err(failure("operator does not belong to selected family"));
            }
        }
        Ok(())
    }

    fn discovery(&mut self, value: &Value) -> Result<(), ProfileError> {
        let mutants = value["mutants"]
            .as_array()
            .ok_or_else(|| failure("adapter discovery has no mutants"))?;
        if value["truncated"] != false
            || value["returnedCandidates"].as_u64() != Some(mutants.len() as u64)
            || value["totalCandidates"].as_u64() != Some(mutants.len() as u64)
            || mutants.len() > MAX_DISCOVERY_CANDIDATES
        {
            return Err(failure(
                "adapter discovery is truncated, inconsistent or exceeds candidate limit",
            ));
        }
        for value in mutants {
            let mutant: Mutant = serde_json::from_value(value.clone())
                .map_err(|_| failure("malformed discovered mutant"))?;
            mutant
                .validate_model()
                .map_err(|_| failure("invalid discovered mutant identity"))?;
            if mutant.file != self.source
                || mutant.family.as_str() != self.family
                || !self.requested.contains(&mutant.operator_id)
                || !mutant.components.is_empty()
            {
                return Err(failure(
                    "adapter returned a mutant outside requested file or operator selection",
                ));
            }
            if mutant.validation != ValidationState::Unvalidated
                || self.mutants.insert(mutant.id, value.clone()).is_some()
            {
                return Err(failure("duplicate or already validated discovered mutant"));
            }
        }
        Ok(())
    }

    fn candidate(&mut self, value: &Value) -> Result<(), ProfileError> {
        let mut mutant: Mutant = serde_json::from_value(value["mutant"].clone())
            .map_err(|_| failure("malformed validated mutant"))?;
        if self.mutants.get(&mutant.id) != Some(&value["mutant"])
            || self.seen.contains(&mutant.id)
            || value["index"].as_u64() != Some(self.seen.len() as u64)
        {
            return Err(failure("missing, changed or duplicate validated candidate"));
        }
        let validation = &value["validation"];
        let patch = &value["safePatch"];
        if validation["mutantId"].as_str() != Some(&mutant.id)
            || patch["mutantId"].as_str() != Some(&mutant.id)
            || patch["file"].as_str() != Some(&mutant.file)
            || patch["writesPerformed"] != false
        {
            return Err(failure(
                "static validation or patch proof identifies a different mutant",
            ));
        }
        let valid = match (
            validation["valid"].as_bool(),
            validation["validation"].as_str(),
        ) {
            (Some(true), Some("valid")) => true,
            (Some(false), Some("invalid")) => false,
            _ => return Err(failure("contradictory static validation evidence")),
        };
        if valid {
            mutant.validation = ValidationState::Valid;
            let expected = read_and_apply_mutant(self.package, &mutant, 4 * 1024 * 1024)
                .map_err(|_| failure("static candidate byte patch is not applicable"))?;
            if patch["applied"] != true
                || patch["patchedBytes"].as_u64() != Some(expected.len() as u64)
                || patch["patchedUtf8"].as_str().map(str::as_bytes) != Some(expected.as_slice())
                || patch["patchedHash"].as_str() != Some(&sha256_hex(&expected))
            {
                return Err(failure("static candidate patch proof mismatch"));
            }
            *self.valid.entry(mutant.operator_id).or_default() += 1;
        } else {
            let bytes = inputs::read_regular(&self.package.join(&mutant.file), 4 * 1024 * 1024)?;
            if sha256_hex(&bytes) != mutant.source_hash || patch["applied"] != false {
                return Err(failure(
                    "invalid candidate changed source or contains a patch",
                ));
            }
        }
        self.seen.insert(mutant.id);
        Ok(())
    }

    pub(super) fn finish(
        self,
        status: ExitStatus,
        signal: Option<i32>,
    ) -> Result<FamilyApplicability, ProfileError> {
        let cleanup = self
            .cleanup
            .ok_or_else(|| failure("adapter transcript has no cleanup receipt"))?;
        let valid: usize = self.valid.values().sum();
        if cleanup["complete"] != true
            || cleanup["runnerShutdown"] != true
            || cleanup["adapterCloseSucceeded"] != true
            || cleanup["writesPerformed"] != false
            || cleanup["activeRuns"].as_u64() != Some(0)
            || cleanup["validatedCandidates"].as_u64() != Some(self.seen.len() as u64)
            || cleanup["staticallyValidCandidates"].as_u64() != Some(valid as u64)
        {
            return Err(failure("adapter cleanup is incomplete or inconsistent"));
        }
        if cleanup["campaignSucceeded"] == false {
            if let Some(signal) = signal {
                let reason = if signal == libc::SIGTERM {
                    "SIGTERM"
                } else {
                    "SIGINT"
                };
                if status.code() == Some(1)
                    && cleanup["cancelled"] == true
                    && cleanup["cancellationReason"] == reason
                    && cleanup
                        .get("exitCode")
                        .is_some_and(|code| code.is_null() || code.as_u64().is_some())
                {
                    return Err(ProfileError::Cancelled(signal));
                }
            }
            return Err(failure(
                "adapter discovery failed without successful cleanup evidence",
            ));
        }
        if cleanup["campaignSucceeded"] != true
            || !status.success()
            || cleanup["exitCode"].as_u64() != Some(0)
            || cleanup["cancelled"] != false
            || !self.discovered
            || self.seen.len() != self.mutants.len()
        {
            return Err(failure(
                "incomplete adapter transcript or inconsistent owner exit",
            ));
        }
        if let Some(signal) = signal {
            return Err(ProfileError::Cancelled(signal));
        }
        let active_operators = failforge_config::catalogue::operators(self.family)
            .unwrap_or_default()
            .iter()
            .filter(|operator| self.valid.get(**operator).copied().unwrap_or(0) > 0)
            .map(|operator| (*operator).to_owned())
            .collect();
        Ok(FamilyApplicability {
            family: self.family.into(),
            active_operators,
            discovered_candidates: self.mutants.len(),
            statically_valid_candidates: valid,
            valid_counts: self
                .requested
                .iter()
                .map(|operator| {
                    (
                        operator.clone(),
                        self.valid.get(operator).copied().unwrap_or(0),
                    )
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    fn exit_one() -> ExitStatus {
        #[cfg(unix)]
        {
            ExitStatus::from_raw(256)
        }
        #[cfg(windows)]
        {
            ExitStatus::from_raw(1)
        }
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "failforge-profile-transcript-validation-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn handshake() -> Value {
        json!({"step":"handshake","payload":{"operators":[{"id":"RELATIONAL_BOUNDARY","family":"classic"}]}})
    }
    fn discovery(mutants: Vec<Value>) -> Value {
        json!({"step":"discover","payload":{"truncated":false,"returnedCandidates":mutants.len(),"totalCandidates":mutants.len(),"mutants":mutants}})
    }
    fn cleanup() -> Value {
        json!({"step":"cleanup","payload":{"complete":true,"runnerShutdown":true,"adapterCloseSucceeded":true,"writesPerformed":false,"activeRuns":0,"validatedCandidates":0,"staticallyValidCandidates":0,"campaignSucceeded":true,"exitCode":0,"cancelled":false}})
    }
    fn accept(transcript: &mut Transcript<'_>, event: &Value) -> Result<(), ProfileError> {
        transcript.accept(&serde_json::to_vec(event).unwrap())
    }

    #[test]
    fn complete_empty_discovery_is_valid_and_truncation_is_not() {
        let requested = vec!["RELATIONAL_BOUNDARY".to_owned()];
        let mut transcript = Transcript::new(Path::new("/"), "classic", &requested, "lib/a.dart");
        accept(&mut transcript, &handshake()).unwrap();
        accept(&mut transcript, &discovery(vec![])).unwrap();
        accept(&mut transcript, &cleanup()).unwrap();
        let result = transcript.finish(ExitStatus::from_raw(0), None).unwrap();
        assert_eq!(result.valid_counts, vec![("RELATIONAL_BOUNDARY".into(), 0)]);
        for changed in [json!(true), Value::Null] {
            let mut transcript =
                Transcript::new(Path::new("/"), "classic", &requested, "lib/a.dart");
            accept(&mut transcript, &handshake()).unwrap();
            let mut event = discovery(vec![]);
            event["payload"]["truncated"] = changed;
            assert!(accept(&mut transcript, &event).is_err());
        }
    }

    #[test]
    fn malformed_duplicate_and_nonterminal_transcript_are_rejected() {
        let mut transcript = Transcript::new(Path::new("/"), "classic", &[], "lib/a.dart");
        assert!(
            transcript
                .accept(br#"{"step":"handshake","step":"cleanup","payload":{}}"#)
                .is_err()
        );
        accept(&mut transcript, &handshake()).unwrap();
        assert!(accept(&mut transcript, &handshake()).is_err());
        accept(&mut transcript, &discovery(vec![])).unwrap();
        accept(&mut transcript, &cleanup()).unwrap();
        assert!(accept(&mut transcript, &cleanup()).is_err());
    }

    #[test]
    fn cancellation_requires_matching_terminal_cleanup() {
        let mut transcript = Transcript::new(Path::new("/"), "classic", &[], "lib/a.dart");
        let mut event = cleanup();
        event["payload"]["campaignSucceeded"] = false.into();
        event["payload"]["cancelled"] = true.into();
        event["payload"]["cancellationReason"] = "SIGTERM".into();
        event["payload"]["exitCode"] = Value::Null;
        accept(&mut transcript, &event).unwrap();
        assert!(matches!(
            transcript.finish(exit_one(), Some(libc::SIGTERM)),
            Err(ProfileError::Cancelled(libc::SIGTERM))
        ));
        event["payload"]["complete"] = false.into();
        let mut transcript = Transcript::new(Path::new("/"), "classic", &[], "lib/a.dart");
        accept(&mut transcript, &event).unwrap();
        assert!(matches!(
            transcript.finish(exit_one(), Some(libc::SIGTERM)),
            Err(ProfileError::Analysis(_))
        ));
    }

    #[test]
    fn selection_and_candidate_limits_are_checked_before_validation() {
        use failforge_dart::{ByteEdit, MutationFamily, Symbol};
        let requested = vec!["RELATIONAL_BOUNDARY".to_owned()];
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: requested[0].clone(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/other.dart".into(),
            source_hash: sha256_hex(b"a"),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 0,
                length_bytes: 1,
                expected: "a".into(),
                replacement: "b".into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        let mut transcript = Transcript::new(Path::new("/"), "classic", &requested, "lib/a.dart");
        accept(&mut transcript, &handshake()).unwrap();
        assert!(accept(&mut transcript, &discovery(vec![json!(mutant)])).is_err());
        mutant.file = "lib/a.dart".into();
        mutant.operator_id = "BOOLEAN_LITERAL_FLIP".into();
        mutant.id = mutant.stable_id();
        assert!(accept(&mut transcript, &discovery(vec![json!(mutant)])).is_err());
        assert!(
            accept(
                &mut transcript,
                &discovery(vec![Value::Null; MAX_DISCOVERY_CANDIDATES + 1])
            )
            .is_err()
        );
    }

    #[test]
    fn real_byte_proofs_are_checked_and_validated_results_do_not_write_sources() {
        use failforge_dart::{ByteEdit, MutationFamily, Symbol};
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("lib")).unwrap();
        let source = b"bool f(int x) => x < 1;\n";
        fs::write(fixture.0.join("lib/a.dart"), source).unwrap();
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: "RELATIONAL_BOUNDARY".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/a.dart".into(),
            source_hash: sha256_hex(source),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 19,
                length_bytes: 1,
                expected: "<".into(),
                replacement: "<=".into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        let patched = b"bool f(int x) => x <= 1;\n";
        let candidate = json!({"step":"candidate","payload":{"index":0,"mutant":mutant,
            "validation":{"mutantId":mutant.id,"valid":true,"validation":"valid"},
            "safePatch":{"applied":true,"mutantId":mutant.id,"file":mutant.file,"patchedHash":sha256_hex(patched),"patchedBytes":patched.len(),"patchedUtf8":std::str::from_utf8(patched).unwrap(),"writesPerformed":false}}});
        let requested = vec!["RELATIONAL_BOUNDARY".into()];
        let build = || {
            let mut transcript = Transcript::new(&fixture.0, "classic", &requested, "lib/a.dart");
            accept(&mut transcript, &handshake()).unwrap();
            accept(&mut transcript, &discovery(vec![json!(mutant)])).unwrap();
            transcript
        };
        let mut transcript = build();
        accept(&mut transcript, &candidate).unwrap();
        let mut terminal = cleanup();
        terminal["payload"]["validatedCandidates"] = 1.into();
        terminal["payload"]["staticallyValidCandidates"] = 1.into();
        accept(&mut transcript, &terminal).unwrap();
        let result = transcript.finish(ExitStatus::from_raw(0), None).unwrap();
        assert_eq!(result.active_operators, requested);
        assert_eq!(fs::read(fixture.0.join("lib/a.dart")).unwrap(), source);
        for field in [
            "patchedHash",
            "patchedBytes",
            "patchedUtf8",
            "mutantId",
            "file",
            "writesPerformed",
        ] {
            let mut event = candidate.clone();
            event["payload"]["safePatch"][field] = Value::Null;
            assert!(accept(&mut build(), &event).is_err(), "{field}");
        }
        let mut event = candidate.clone();
        event["payload"]["validation"]["valid"] = false.into();
        assert!(accept(&mut build(), &event).is_err());
        let mut transcript = build();
        accept(&mut transcript, &candidate).unwrap();
        assert!(accept(&mut transcript, &candidate).is_err());
        fs::write(fixture.0.join("lib/a.dart"), b"changed").unwrap();
        assert!(accept(&mut build(), &candidate).is_err());
        event["payload"]["validation"]["validation"] = "invalid".into();
        event["payload"]["safePatch"]["applied"] = false.into();
        assert!(accept(&mut build(), &event).is_err());
    }
}
