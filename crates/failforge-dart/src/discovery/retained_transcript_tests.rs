use super::*;
use crate::{ByteEdit, Mutant, Symbol, ValidationState, sha256_hex};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-retained-transcript-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("lib")).unwrap();
        fs::write(root.join("lib/subject.dart"), "final value = true;\n").unwrap();
        Self { root }
    }
    fn rows(&self, valid: bool) -> Vec<Value> {
        let source = "final value = true;\n";
        let patched = "final value = false;\n";
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: "BOOLEAN_LITERAL_FLIP".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/subject.dart".into(),
            source_hash: sha256_hex(source.as_bytes()),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 14,
                length_bytes: 4,
                expected: "true".into(),
                replacement: "false".into(),
            },
            symbol: Symbol {
                class_name: None,
                method_name: None,
            },
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        let patch = if valid {
            json!({"applied":true,"writesPerformed":false,"mutantId":mutant.id,"file":mutant.file,
                "patchedHash":sha256_hex(patched.as_bytes()),"patchedBytes":patched.len(),"patchedUtf8":patched})
        } else {
            json!({"applied":false,"writesPerformed":false,"mutantId":mutant.id,"file":mutant.file,"reason":"static_invalid"})
        };
        vec![
            json!({"step":"handshake","payload":{"protocolVersion":1,"operators":[
                {"id":"BOOLEAN_LITERAL_FLIP","family":"classic","version":1}]}}),
            json!({"step":"discover","payload":{"truncated":false,"returnedCandidates":1,"totalCandidates":1,"mutants":[mutant]}}),
            json!({"step":"candidate","payload":{"index":0,"mutant":mutant,
                "validation":{"mutantId":mutant.id,"valid":valid,"validation":if valid {"valid"} else {"invalid"},"diagnostics":[]},
                "safePatch":patch}}),
            json!({"step":"cleanup","payload":{"complete":true,"campaignSucceeded":true,
                "adapterCloseSucceeded":true,"runnerShutdown":true,"activeRuns":0,"exitCode":0,
                "writesPerformed":false,"validatedCandidates":1,"staticallyValidCandidates":usize::from(valid)}}),
        ]
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn encode(rows: &[Value]) -> Vec<u8> {
    rows.iter()
        .flat_map(|row| {
            let mut raw = serde_json::to_vec(row).unwrap();
            raw.push(b'\n');
            raw
        })
        .collect()
}

#[test]
fn complete_valid_and_invalid_candidates_reuse_exact_source_proofs() {
    let fixture = Fixture::new();
    for valid in [true, false] {
        let rows = fixture.rows(valid);
        let retained = load_retained_transcript(&encode(&rows), &fixture.root, "classic").unwrap();
        assert_eq!(retained.candidates, vec![rows[2]["payload"].clone()]);
        assert_eq!(retained.steps["cleanup"], rows[3]["payload"]);
    }
    assert_eq!(
        fs::read_to_string(fixture.root.join("lib/subject.dart")).unwrap(),
        "final value = true;\n"
    );
}

#[test]
fn framing_terminal_order_duplicates_and_numeric_types_are_strict() {
    let fixture = Fixture::new();
    let rows = fixture.rows(true);
    let mut cases = Vec::new();
    let mut missing_lf = encode(&rows);
    missing_lf.pop();
    cases.push(missing_lf);
    cases.push(encode(&rows[..3]));
    let mut order = rows.clone();
    order.swap(0, 1);
    cases.push(encode(&order));
    let mut late = rows.clone();
    late.push(rows[2].clone());
    cases.push(encode(&late));
    let mut duplicate = rows.clone();
    duplicate.insert(2, rows[1].clone());
    cases.push(encode(&duplicate));
    cases.push(
        String::from_utf8(encode(&rows))
            .unwrap()
            .replace("\"activeRuns\":0", "\"activeRuns\":0,\"activeRuns\":0")
            .into_bytes(),
    );
    for field in [
        "activeRuns",
        "exitCode",
        "validatedCandidates",
        "staticallyValidCandidates",
    ] {
        for wrong in [json!(true), json!(-1), json!(0.0), json!(null)] {
            let mut rows = rows.clone();
            rows[3]["payload"][field] = wrong;
            cases.push(encode(&rows));
        }
    }
    for bytes in cases {
        assert!(load_retained_transcript(&bytes, &fixture.root, "classic").is_err());
    }
}

#[test]
fn missing_duplicate_mixed_or_forged_candidates_cannot_authorize_replay() {
    let fixture = Fixture::new();
    let rows = fixture.rows(true);
    for (pointer, replacement) in [
        ("/1/payload/truncated", json!(true)),
        ("/1/payload/totalCandidates", json!(2)),
        ("/2/payload/index", json!(1)),
        ("/2/payload/mutant/family", json!("failure_mode")),
        ("/2/payload/validation/valid", json!(false)),
        ("/2/payload/safePatch/patchedUtf8", json!("changed")),
        ("/0/payload/operators/0/version", json!(2)),
        ("/3/payload/cancelled", json!(true)),
    ] {
        let mut value = json!(rows);
        if pointer.ends_with("cancelled") {
            value[3]["payload"]["cancelled"] = replacement;
        } else {
            *value.pointer_mut(pointer).unwrap() = replacement;
        }
        assert!(
            load_retained_transcript(&encode(value.as_array().unwrap()), &fixture.root, "classic")
                .is_err(),
            "{pointer}"
        );
    }
    let mut doubled = rows.clone();
    doubled[0]["payload"]["operators"]
        .as_array_mut()
        .unwrap()
        .push(rows[0]["payload"]["operators"][0].clone());
    assert!(load_retained_transcript(&encode(&doubled), &fixture.root, "classic").is_err());
    fs::write(
        fixture.root.join("lib/subject.dart"),
        "final value = null;\n",
    )
    .unwrap();
    assert!(load_retained_transcript(&encode(&rows), &fixture.root, "classic").is_err());
}
