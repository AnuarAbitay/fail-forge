use failforge_dart::{
    Mutant, MutationFamily, PatchError, ProtocolError, TestEvidence, TestRunClassification,
    ValidationState, apply_mutant,
};
use serde::Deserialize;
use serde_json::{Value, json};

const PATCH_FIXTURES: [&str; 2] = [
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/fixtures/patch.json"
    )),
    include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/fixtures/patch-unicode-expression.json"
    )),
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchFixture {
    source_utf8: String,
    mutant: Mutant,
    patched_utf8: String,
}

fn fixtures() -> impl Iterator<Item = PatchFixture> {
    PATCH_FIXTURES
        .into_iter()
        .map(|encoded| serde_json::from_str(encoded).unwrap())
}

#[test]
fn family_wire_names_are_exact_and_match_canonical_id_strings() {
    for (family, wire) in [
        (MutationFamily::Classic, "classic"),
        (MutationFamily::FailureMode, "failure_mode"),
        (MutationFamily::LanguageSpecific, "language_specific"),
    ] {
        assert_eq!(family.as_str(), wire);
        assert_eq!(serde_json::to_value(family).unwrap(), json!(wire));
        assert_eq!(
            serde_json::from_value::<MutationFamily>(json!(wire)).unwrap(),
            family
        );
    }
}

#[test]
fn mutant_dto_requires_an_explicit_exact_known_family() {
    for fixture in fixtures() {
        let encoded = serde_json::to_value(fixture.mutant).unwrap();
        for invalid in [
            Value::Null,
            json!(true),
            json!(1),
            json!([]),
            json!({}),
            json!(""),
            json!("Classic"),
            json!("FailureMode"),
            json!("failureMode"),
            json!("failure-mode"),
            json!("failure_modes"),
            json!("unknown"),
            json!(" classic"),
            json!("failure_mode "),
        ] {
            let mut changed = encoded.clone();
            changed["family"] = invalid.clone();
            assert!(
                serde_json::from_value::<Mutant>(changed).is_err(),
                "invalid family accepted: {invalid}"
            );
        }
        let mut missing = encoded;
        missing.as_object_mut().unwrap().remove("family");
        assert!(serde_json::from_value::<Mutant>(missing).is_err());
    }
}

#[test]
fn family_changes_require_a_new_id_without_changing_the_byte_patch() {
    for fixture in fixtures() {
        let classic = fixture.mutant;
        assert_eq!(classic.family, MutationFamily::Classic);
        classic.validate_model().unwrap();
        assert_eq!(classic.stable_id(), classic.id);

        // This synthetic reassignment probes the language-neutral DTO and hash
        // boundary. The Dart operator catalog separately authenticates whether
        // an operator is allowed to emit a particular family.
        let mut failure = classic.clone();
        failure.family = MutationFamily::FailureMode;
        assert_ne!(failure.stable_id(), classic.id);
        assert_eq!(
            failure.validate_model(),
            Err(ProtocolError::MutantIdMismatch)
        );
        assert!(matches!(
            apply_mutant(fixture.source_utf8.as_bytes(), &failure),
            Err(PatchError::Protocol(ProtocolError::MutantIdMismatch))
        ));

        failure.id = failure.stable_id();
        failure.validate_model().unwrap();
        let roundtrip: Mutant =
            serde_json::from_value(serde_json::to_value(&failure).unwrap()).unwrap();
        assert_eq!(roundtrip, failure);
        for mutant in [&classic, &roundtrip] {
            assert_eq!(
                apply_mutant(fixture.source_utf8.as_bytes(), mutant).unwrap(),
                fixture.patched_utf8.as_bytes()
            );
        }
    }
}

#[test]
fn component_symbol_and_validation_metadata_never_reclassify_the_family() {
    for fixture in fixtures() {
        for family in [
            MutationFamily::Classic,
            MutationFamily::FailureMode,
            MutationFamily::LanguageSpecific,
        ] {
            let mut mutant = fixture.mutant.clone();
            mutant.family = family;
            mutant.id = mutant.stable_id();
            let id = mutant.id.clone();
            // Component labels and symbol names are user metadata; even names
            // matching either category carry no operator-family semantics.
            mutant.components = vec!["classic".into(), "failure_mode".into()];
            mutant.symbol.class_name = Some("failure_mode".into());
            mutant.symbol.method_name = Some("classic".into());
            for validation in [
                ValidationState::Unvalidated,
                ValidationState::Valid,
                ValidationState::Invalid,
            ] {
                mutant.validation = validation;
                let encoded = serde_json::to_value(&mutant).unwrap();
                assert_eq!(encoded["family"], json!(family.as_str()));
                let decoded: Mutant = serde_json::from_value(encoded).unwrap();
                decoded.validate_model().unwrap();
                assert_eq!(decoded.family, family);
                assert_eq!(decoded.id, id);
                assert_eq!(decoded.stable_id(), id);
                assert_eq!(decoded.validation, validation);
            }
        }
    }
}

#[test]
fn test_outcome_evidence_cannot_supply_or_replace_a_mutation_family() {
    let evidence = TestEvidence {
        baseline_succeeded: true,
        machine_session_complete: true,
        completed_tests: 1,
        assertion_failures: 1,
        compile_or_load_failed: false,
    };
    let encoded = serde_json::to_value(evidence).unwrap();
    assert!(encoded.get("family").is_none());
    for family in [
        MutationFamily::Classic,
        MutationFamily::FailureMode,
        MutationFamily::LanguageSpecific,
    ] {
        let mut mixed = encoded.clone();
        mixed["family"] = json!(family.as_str());
        assert!(serde_json::from_value::<TestEvidence>(mixed).is_err());
        assert!(serde_json::from_value::<TestRunClassification>(json!(family.as_str())).is_err());
    }
    assert_eq!(
        serde_json::to_value(TestRunClassification::TestFailure).unwrap(),
        json!("test_failure")
    );
    assert!(serde_json::from_value::<MutationFamily>(json!("test_failure")).is_err());
}
