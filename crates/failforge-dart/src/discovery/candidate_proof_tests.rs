use super::*;
use crate::{ByteEdit, MutationFamily, Symbol, ValidationState, sha256_hex};
use serde_json::json;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    source: String,
    mutant: Mutant,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-candidate-proof-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("lib")).unwrap();
        let source = "\u{feff}// Кириллица 😀\r\nfinal value = true;\r\n".to_owned();
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: "BOOLEAN_LITERAL_FLIP".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/данные 😀.dart".into(),
            source_hash: sha256_hex(source.as_bytes()),
            components: vec!["logic".into()],
            edit: ByteEdit {
                offset_bytes: source.find("true").unwrap() as u64,
                length_bytes: 4,
                expected: "true".into(),
                replacement: "false".into(),
            },
            symbol: Symbol {
                class_name: None,
                method_name: Some("value".into()),
            },
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        fs::write(root.join(&mutant.file), &source).unwrap();
        Self {
            root,
            source,
            mutant,
        }
    }
    fn raw(&self, valid: bool) -> Value {
        self.for_mutant(&self.mutant, valid)
    }
    fn for_mutant(&self, mutant: &Mutant, valid: bool) -> Value {
        let proof = if valid {
            let patched = self.source.replace("true", "false");
            json!({"applied":true,"mutantId":mutant.id,"file":mutant.file,
                "patchedHash":sha256_hex(patched.as_bytes()),"patchedBytes":patched.len(),
                "patchedUtf8":patched,"writesPerformed":false})
        } else {
            json!({"applied":false,"mutantId":mutant.id,"file":mutant.file,
                "reason":"static_invalid","writesPerformed":false})
        };
        json!({"index":0,"mutant":mutant,"validation":{"mutantId":mutant.id,"valid":valid,
            "validation":if valid {"valid"} else {"invalid"},"diagnostics":[]},"safePatch":proof})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn unicode_crlf_bom_proof_preserves_original_metadata_and_readonly_source() {
    let fixture = Fixture::new();
    let mut raw = fixture.raw(true);
    raw["mutant"]["symbol"]["className"] = Value::Null;
    raw["validation"]["diagnostics"] = json!([{"message":"точный текст\u{2028}😀"}]);
    let path = fixture.root.join(&fixture.mutant.file);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let verified = verify_candidate(&fixture.root, &raw).unwrap();
    assert_eq!(verified.mutant, fixture.mutant);
    assert_eq!(verified.raw_mutant, raw["mutant"]);
    assert_eq!(verified.validation, raw["validation"]);
    assert_eq!(
        verified.patch_sha256.as_deref(),
        raw["safePatch"]["patchedHash"].as_str()
    );
    assert_eq!(fs::read(&path).unwrap(), fixture.source.as_bytes());
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    for family in [
        MutationFamily::Classic,
        MutationFamily::FailureMode,
        MutationFamily::LanguageSpecific,
    ] {
        let mut mutant = fixture.mutant.clone();
        mutant.family = family;
        mutant.id = mutant.stable_id();
        assert_eq!(
            verify_candidate(&fixture.root, &fixture.for_mutant(&mutant, true))
                .unwrap()
                .mutant
                .family,
            family
        );
    }
}

#[test]
fn every_safe_patch_field_and_unknown_fields_are_bound_to_the_regenerated_proof() {
    let fixture = Fixture::new();
    let original = fixture.raw(true);
    for (field, replacement) in [
        ("patchedHash", json!("0".repeat(64))),
        ("patchedBytes", json!(true)),
        ("patchedBytes", json!(-1)),
        ("patchedBytes", json!(1.0)),
        ("patchedUtf8", json!(fixture.source)),
        ("patchedUtf8", json!(null)),
        ("mutantId", json!("0".repeat(64))),
        ("file", json!("lib/other.dart")),
        ("writesPerformed", json!(true)),
        ("writesPerformed", json!(0)),
        ("applied", json!(false)),
        ("applied", json!(1)),
        ("extra", Value::Null),
    ] {
        let mut raw = original.clone();
        raw["safePatch"][field] = replacement;
        assert!(verify_candidate(&fixture.root, &raw).is_err(), "{field}");
    }
    for field in original["safePatch"].as_object().unwrap().keys() {
        let mut raw = original.clone();
        raw["safePatch"].as_object_mut().unwrap().remove(field);
        assert!(
            verify_candidate(&fixture.root, &raw).is_err(),
            "missing {field}"
        );
    }
    let mut fractional_type = original.clone();
    fractional_type["safePatch"]["patchedBytes"] =
        json!(original["safePatch"]["patchedBytes"].as_u64().unwrap() as f64);
    assert!(verify_candidate(&fixture.root, &fractional_type).is_err());
    assert_eq!(
        fs::read(fixture.root.join(&fixture.mutant.file)).unwrap(),
        fixture.source.as_bytes()
    );
}

#[test]
fn candidate_shape_index_model_and_validation_types_fail_closed() {
    let fixture = Fixture::new();
    let valid = fixture.raw(true);
    for index in [
        json!(-1),
        json!(true),
        json!(1.0),
        Value::Null,
        json!("0"),
        json!(MAX_SAFE_INTEGER + 1),
    ] {
        let mut raw = valid.clone();
        raw["index"] = index;
        assert!(verify_candidate(&fixture.root, &raw).is_err());
    }
    for index in [0, 7, MAX_SAFE_INTEGER] {
        let mut raw = valid.clone();
        raw["index"] = json!(index);
        verify_candidate(&fixture.root, &raw).unwrap();
    }
    for field in ["index", "mutant", "validation", "safePatch"] {
        let mut raw = valid.clone();
        raw.as_object_mut().unwrap().remove(field);
        assert!(verify_candidate(&fixture.root, &raw).is_err());
    }
    let mut extra = valid.clone();
    extra["extra"] = true.into();
    assert!(verify_candidate(&fixture.root, &extra).is_err());
    for (pointer, replacement) in [
        ("/mutant", Value::Null),
        ("/mutant/family", json!("unknown")),
        ("/mutant/operatorVersion", json!(true)),
        ("/mutant/operatorVersion", json!(2)),
        ("/mutant/components", json!([1])),
        ("/mutant/symbol", json!([])),
        ("/mutant/id", json!("0".repeat(64))),
        ("/mutant/edit/offsetBytes", json!(true)),
        ("/mutant/edit/lengthBytes", json!(-1)),
        ("/mutant/edit/replacement", Value::Null),
        ("/validation", json!([])),
        ("/validation/mutantId", json!("0".repeat(64))),
        ("/validation/valid", json!(1)),
        ("/validation/validation", json!("invalid")),
    ] {
        let mut raw = valid.clone();
        *raw.pointer_mut(pointer).unwrap() = replacement;
        assert!(verify_candidate(&fixture.root, &raw).is_err(), "{pointer}");
    }
    let mut raw = valid.clone();
    raw["mutant"]["extra"] = true.into();
    assert!(verify_candidate(&fixture.root, &raw).is_err());
    let mut raw = valid.clone();
    raw["mutant"]["edit"] = json!([
        fixture.mutant.edit.offset_bytes,
        fixture.mutant.edit.length_bytes,
        fixture.mutant.edit.expected,
        fixture.mutant.edit.replacement
    ]);
    assert!(verify_candidate(&fixture.root, &raw).is_err());
    assert!(verify_candidate(Path::new("relative"), &valid).is_err());
}

#[test]
fn static_invalid_accepts_inapplicable_edit_but_still_binds_source_path_identity_and_proof() {
    let fixture = Fixture::new();
    let mut mutant = fixture.mutant.clone();
    mutant.edit.offset_bytes = 999;
    mutant.id = mutant.stable_id();
    let raw = fixture.for_mutant(&mutant, false);
    let verified = verify_candidate(&fixture.root, &raw).unwrap();
    assert!(verified.patch_sha256.is_none());
    assert_eq!(verified.mutant, mutant);
    assert!(verify_candidate(&fixture.root, &fixture.for_mutant(&mutant, true)).is_err());
    for field in ["applied", "writesPerformed", "mutantId", "file", "reason"] {
        let mut forged = raw.clone();
        forged["safePatch"][field] = Value::Null;
        assert!(verify_candidate(&fixture.root, &forged).is_err(), "{field}");
    }
    let mut forged = raw.clone();
    forged["safePatch"]["patchedHash"] = json!("0".repeat(64));
    assert!(verify_candidate(&fixture.root, &forged).is_err());
    let mut changed_hash = mutant.clone();
    changed_hash.source_hash = "0".repeat(64);
    changed_hash.id = changed_hash.stable_id();
    assert!(verify_candidate(&fixture.root, &fixture.for_mutant(&changed_hash, false)).is_err());
    for name in [
        "../outside.dart",
        "lib/../value.dart",
        "/source.dart",
        "lib\\value.dart",
        "C:/value.dart",
        "lib//value.dart",
        "lib/value.txt",
    ] {
        let mut unsafe_mutant = mutant.clone();
        unsafe_mutant.file = name.into();
        unsafe_mutant.id = unsafe_mutant.stable_id();
        assert!(
            verify_candidate(&fixture.root, &fixture.for_mutant(&unsafe_mutant, false)).is_err(),
            "{name}"
        );
    }
    fs::write(fixture.root.join(&mutant.file), b"changed source").unwrap();
    assert!(verify_candidate(&fixture.root, &raw).is_err());
}

#[test]
fn valid_edits_reject_utf8_split_ranges_mismatched_expected_and_no_change() {
    let fixture = Fixture::new();
    for (offset, length, expected, replacement) in [
        (1, 0, "", "x"),
        (u64::MAX, 4, "true", "false"),
        (fixture.mutant.edit.offset_bytes, 4, "xxxx", "false"),
        (fixture.mutant.edit.offset_bytes, 4, "true", "true"),
        (fixture.mutant.edit.offset_bytes, 999, "true", "false"),
    ] {
        let mut mutant = fixture.mutant.clone();
        mutant.edit = ByteEdit {
            offset_bytes: offset,
            length_bytes: length,
            expected: expected.into(),
            replacement: replacement.into(),
        };
        mutant.id = mutant.stable_id();
        assert!(verify_candidate(&fixture.root, &fixture.for_mutant(&mutant, true)).is_err());
    }
}

#[test]
fn static_invalid_source_reads_reject_symlinks_fifos_and_oversized_files() {
    let fixture = Fixture::new();
    let path = fixture.root.join(&fixture.mutant.file);
    let outside = fixture.root.join("outside.dart");
    fs::rename(&path, &outside).unwrap();
    symlink(&outside, &path).unwrap();
    assert!(verify_candidate(&fixture.root, &fixture.raw(false)).is_err());
    fs::remove_file(&path).unwrap();
    fs::rename(&outside, &path).unwrap();
    let mut mutant = fixture.mutant.clone();
    symlink(fixture.root.join("lib"), fixture.root.join("alias")).unwrap();
    mutant.file = "alias/данные 😀.dart".into();
    mutant.id = mutant.stable_id();
    assert!(verify_candidate(&fixture.root, &fixture.for_mutant(&mutant, false)).is_err());
    fs::remove_file(&path).unwrap();
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the terminated path refers to this fresh test-owned fixture.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(verify_candidate(&fixture.root, &fixture.raw(false)).is_err());
    fs::remove_file(&path).unwrap();
    let source = vec![b' '; super::super::MAX_SOURCE_BYTES + 1];
    fs::write(&path, &source).unwrap();
    let mut mutant = fixture.mutant.clone();
    mutant.source_hash = sha256_hex(&source);
    mutant.id = mutant.stable_id();
    assert!(verify_candidate(&fixture.root, &fixture.for_mutant(&mutant, false)).is_err());
}

#[test]
fn retained_evidence_depth_and_encoded_size_are_bounded_before_source_reads() {
    let fixture = Fixture::new();
    let mut raw = fixture.raw(true);
    let mut nested = Value::Null;
    for _ in 0..65 {
        nested = json!([nested]);
    }
    raw["validation"]["diagnostics"] = nested;
    assert!(verify_candidate(&fixture.root, &raw).is_err());
    let mut raw = fixture.raw(true);
    // Escaped control bytes exceed the encoded limit while their raw string
    // length remains below the structural budget.
    raw["validation"]["diagnostics"] = json!("\0".repeat(MAX_LINE_BYTES / 5));
    assert!(verify_candidate(&fixture.root, &raw).is_err());
}
