use super::*;
use crate::{ByteEdit, Mutant, Symbol, ValidationState, sha256_hex};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("lib")).unwrap();
        std::fs::write(path.join("lib/source.dart"), b"int value = 1;\n").unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn payload(&self) -> Value {
        json!({"workspaceRoot":self.0,"packageRoot":".","limits":{"maxMutants":2}})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mutant() -> Mutant {
    let mut mutant = Mutant {
        id: String::new(),
        operator_id: "CONSTANT_REPLACEMENT".into(),
        operator_version: 1,
        family: MutationFamily::Classic,
        file: "lib/source.dart".into(),
        source_hash: sha256_hex(b"int value = 1;\n"),
        components: vec![],
        edit: ByteEdit {
            offset_bytes: 12,
            length_bytes: 1,
            expected: "1".into(),
            replacement: "2".into(),
        },
        symbol: Symbol::default(),
        validation: ValidationState::Unvalidated,
    };
    mutant.id = mutant.stable_id();
    mutant
}
fn handshake() -> Value {
    json!({"protocolVersion":1,"operators":[{"id":"CONSTANT_REPLACEMENT","family":"classic","version":1}]})
}
fn discovery(mutants: Vec<Value>) -> Value {
    json!({"returnedCandidates":mutants.len(),"totalCandidates":mutants.len(),"truncated":false,"mutants":mutants})
}
fn validation(mutant: &Mutant, valid: bool) -> Value {
    json!({"mutantId":mutant.id,"valid":valid,"validation":if valid {"valid"} else {"invalid"}})
}

#[test]
fn configuration_rejects_invalid_roots_limits_depth_and_response_limits_before_spawn() {
    let fixture = Fixture::new();
    let config = || {
        DiscoveryConfig::new(
            "/not-started".into(),
            fixture.0.clone(),
            fixture.payload(),
            MutationFamily::Classic,
        )
        .unwrap()
    };
    assert_eq!(config().package_root(), fixture.0);
    for limits in [
        json!(null),
        json!({"unknown":1}),
        json!({"maxMutants":true}),
        json!({"maxMutants":-1}),
        json!({"maxMutants":10001}),
        json!({"maxMutants":1.0}),
    ] {
        let mut payload = fixture.payload();
        payload["limits"] = limits;
        assert!(
            DiscoveryConfig::new(
                "/not-started".into(),
                fixture.0.clone(),
                payload,
                MutationFamily::Classic
            )
            .is_err()
        );
    }
    for root in [json!("../outside"), json!("/outside"), json!(null)] {
        let mut payload = fixture.payload();
        payload["packageRoot"] = root;
        assert!(
            DiscoveryConfig::new(
                "/not-started".into(),
                fixture.0.clone(),
                payload,
                MutationFamily::Classic
            )
            .is_err()
        );
    }
    let mut payload = fixture.payload();
    let mut nested = Value::Null;
    for _ in 0..66 {
        nested = json!([nested]);
    }
    payload["nested"] = nested;
    assert!(
        DiscoveryConfig::new(
            "/not-started".into(),
            fixture.0.clone(),
            payload,
            MutationFamily::Classic
        )
        .is_err()
    );
    assert!(config().with_capture_dir("relative".into()).is_err());
    assert!(config().with_capture_dir(fixture.0.join("capture")).is_ok());
    assert!(
        config()
            .with_client_options(ClientOptions {
                max_line_bytes: 0,
                ..Default::default()
            })
            .is_err()
    );
    assert!(
        config()
            .with_client_options(ClientOptions {
                request_timeout: Duration::ZERO,
                ..Default::default()
            })
            .is_err()
    );
    assert_eq!(candidate_limit(&json!({})).unwrap(), 1000);
    for count in [0, 10_000] {
        assert_eq!(
            candidate_limit(&json!({"limits":{"maxMutants":count}})).unwrap(),
            count
        );
    }
}

#[test]
fn private_controls_are_removed_from_adapter_environment() {
    let fixture = Fixture::new();
    let config = DiscoveryConfig::new(
        "/dart".into(),
        fixture.0.clone(),
        fixture.payload(),
        MutationFamily::Classic,
    )
    .unwrap();
    let spec = config.spec();
    for name in PRIVATE_ENV {
        assert!(spec.env.remove.contains(std::ffi::OsStr::new(name)));
    }
}

#[test]
fn catalogue_and_discovery_require_unique_family_version_identity_and_exact_counters() {
    let catalog = operator_catalogue(&handshake()).unwrap();
    let raw = serde_json::to_value(mutant()).unwrap();
    let good = discovery(vec![raw.clone()]);
    assert_eq!(
        discovered(&good, &catalog, MutationFamily::Classic, 1).unwrap(),
        std::slice::from_ref(&raw)
    );
    for field in ["totalCandidates", "returnedCandidates"] {
        for count in [json!(true), json!(-1), json!(0), json!(2), json!(1.0)] {
            let mut broken = good.clone();
            broken[field] = count;
            assert!(discovered(&broken, &catalog, MutationFamily::Classic, 1).is_err());
        }
    }
    assert!(discovered(&good, &catalog, MutationFamily::FailureMode, 1).is_err());
    assert!(discovered(&good, &catalog, MutationFamily::Classic, 0).is_err());
    assert!(
        discovered(
            &discovery(vec![raw.clone(), raw]),
            &catalog,
            MutationFamily::Classic,
            2
        )
        .is_err()
    );
    let mut broken = good.clone();
    broken["truncated"] = json!(true);
    assert!(discovered(&broken, &catalog, MutationFamily::Classic, 1).is_err());
    for field in ["id", "family", "version"] {
        let mut metadata = handshake();
        metadata["operators"][0][field] = Value::Null;
        assert!(operator_catalogue(&metadata).is_err());
    }
    let mut duplicate = handshake();
    let operator = duplicate["operators"][0].clone();
    duplicate["operators"]
        .as_array_mut()
        .unwrap()
        .push(operator);
    assert!(operator_catalogue(&duplicate).is_err());
    let mut other = mutant();
    other.operator_id = "OTHER".into();
    other.id = other.stable_id();
    assert!(
        discovered(
            &discovery(vec![json!(other)]),
            &catalog,
            MutationFamily::Classic,
            1
        )
        .is_err()
    );
}

#[test]
fn candidate_retains_raw_metadata_and_authenticates_exact_patch_and_utf8_size() {
    let fixture = Fixture::new();
    let mutant = mutant();
    let mut original = json!(mutant);
    original["symbol"]["className"] = Value::Null;
    let evidence = candidate(3, &original, validation(&mutant, true), &fixture.0).unwrap();
    assert_eq!(evidence.index, 3);
    assert_eq!(evidence.mutant, original);
    assert_eq!(evidence.mutant["validation"], "unvalidated");
    assert_eq!(evidence.safe_patch["patchedUtf8"], "int value = 2;\n");
    assert_eq!(evidence.safe_patch["patchedBytes"], 15);
    assert_eq!(
        evidence.safe_patch["patchedHash"],
        sha256_hex(b"int value = 2;\n")
    );
    assert_eq!(evidence.safe_patch["writesPerformed"], false);
    assert_eq!(
        std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
        b"int value = 1;\n"
    );
    for value in [
        json!({"mutantId":"wrong","valid":true,"validation":"valid"}),
        json!({"mutantId":mutant.id,"valid":true,"validation":"invalid"}),
    ] {
        assert!(candidate(0, &original, value, &fixture.0).is_err());
    }
}

#[test]
fn invalid_candidate_still_checks_source_without_requiring_patch_applicability() {
    let fixture = Fixture::new();
    let mut mutant = mutant();
    mutant.edit.offset_bytes = 999;
    mutant.id = mutant.stable_id();
    let original = json!(mutant);
    let evidence = candidate(0, &original, validation(&mutant, false), &fixture.0).unwrap();
    assert_eq!(evidence.safe_patch["applied"], false);
    assert!(evidence.safe_patch.get("patchedUtf8").is_none());
    assert!(candidate(0, &original, validation(&mutant, true), &fixture.0).is_err());
    std::fs::write(fixture.0.join("lib/source.dart"), "changed").unwrap();
    assert!(candidate(0, &original, validation(&mutant, false), &fixture.0).is_err());
}

#[test]
fn invalid_candidates_reject_symlinks_and_oversized_sources() {
    let fixture = Fixture::new();
    let mutant = mutant();
    let original = json!(mutant);
    std::fs::remove_file(fixture.0.join("lib/source.dart")).unwrap();
    std::fs::write(fixture.0.join("outside.dart"), b"int value = 1;\n").unwrap();
    std::os::unix::fs::symlink("../outside.dart", fixture.0.join("lib/source.dart")).unwrap();
    assert!(candidate(0, &original, validation(&mutant, false), &fixture.0).is_err());
    std::fs::remove_file(fixture.0.join("lib/source.dart")).unwrap();
    let source = vec![b' '; MAX_SOURCE_BYTES + 1];
    std::fs::write(fixture.0.join("lib/source.dart"), &source).unwrap();
    let mut oversized = mutant;
    oversized.source_hash = sha256_hex(&source);
    oversized.id = oversized.stable_id();
    assert!(
        candidate(
            0,
            &json!(oversized),
            validation(&oversized, false),
            &fixture.0
        )
        .is_err()
    );
}

#[test]
fn sink_bounds_each_record_and_aggregate_before_writes_and_reserves_cleanup() {
    let mut bytes = Vec::new();
    let mut sink = EventSink {
        output: &mut bytes,
        emitted: 0,
    };
    assert!(
        sink.emit("candidate", &json!("x".repeat(MAX_LINE_BYTES)), false)
            .is_err()
    );
    assert_eq!(sink.emitted, 0);
    sink.emitted = MAX_TRANSCRIPT_BYTES - CLEANUP_RESERVE;
    assert!(sink.emit("candidate", &json!({}), false).is_err());
    sink.emit("cleanup", &json!({"complete":true}), true)
        .unwrap();
    let event: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(event["step"], "cleanup");
    let mut bytes = Vec::new();
    let mut sink = EventSink {
        output: &mut bytes,
        emitted: 0,
    };
    sink.emit("candidate", &json!({"text":"a\u{2028}b\u{2029}c"}), false)
        .unwrap();
    assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
}

#[tokio::test]
async fn pre_cancelled_session_does_not_spawn_and_retains_cleanup_receipt() {
    let fixture = Fixture::new();
    let config = DiscoveryConfig::new(
        "/does-not-exist".into(),
        fixture.0.clone(),
        fixture.payload(),
        MutationFamily::Classic,
    )
    .unwrap();
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    let mut output = Vec::new();
    let run = discover(&config, &mut output, &cancel).await;
    assert!(matches!(
        run.outcome,
        Err(DiscoveryError::Cancelled(libc::SIGTERM))
    ));
    assert!(run.command_result.is_none());
    assert_eq!(run.cleanup["complete"], true);
    assert_eq!(run.cleanup["cancelled"], true);
    assert_eq!(run.cleanup["validatedCandidates"], 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&output).unwrap()["step"],
        "cleanup"
    );
}

#[test]
fn complete_candidate_is_counted_when_flush_fails_after_the_frame_write() {
    struct FailFlushOnce {
        bytes: Vec<u8>,
        failed: bool,
    }
    impl Write for FailFlushOnce {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if !self.failed {
                self.failed = true;
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }
    let mut output = FailFlushOnce {
        bytes: Vec::new(),
        failed: false,
    };
    let mut candidates = Vec::new();
    let fixture = Fixture::new();
    let mutant = mutant();
    let item = candidate(0, &json!(mutant), validation(&mutant, true), &fixture.0).unwrap();
    {
        let mut sink = EventSink {
            output: &mut output,
            emitted: 0,
        };
        assert!(matches!(
            emit_candidate(&mut sink, &mut candidates, item),
            Err(DiscoveryError::Io(io::ErrorKind::BrokenPipe))
        ));
        assert_eq!(candidates.len(), 1);
        sink.emit(
            "cleanup",
            &json!({"campaignSucceeded":false,"validatedCandidates":candidates.len()}),
            true,
        )
        .unwrap();
    }
    let records: Vec<Value> = output
        .bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["payload"]["validatedCandidates"], 1);
    assert_eq!(records[1]["payload"]["campaignSucceeded"], false);
}
