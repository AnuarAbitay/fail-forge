use failforge_dart::{Mutant, RequestEnvelope, apply_mutant};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchFixture {
    source_utf8: String,
    mutant: Mutant,
    patched_utf8: String,
}

#[test]
fn dart_and_rust_share_canonical_id_and_exact_utf8_byte_patch() {
    for encoded in [
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocol/fixtures/patch.json"
        )),
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../protocol/fixtures/patch-unicode-expression.json"
        )),
    ] {
        let fixture: PatchFixture = serde_json::from_str(encoded).unwrap();
        fixture.mutant.validate_model().unwrap();
        assert_eq!(fixture.mutant.stable_id(), fixture.mutant.id);
        assert_eq!(
            apply_mutant(fixture.source_utf8.as_bytes(), &fixture.mutant).unwrap(),
            fixture.patched_utf8.as_bytes()
        );
    }
}

#[test]
fn dart_and_rust_share_handshake_envelope() {
    let request: RequestEnvelope = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../protocol/fixtures/handshake-request.json"
    )))
    .unwrap();
    request.validate().unwrap();
    assert_eq!(request.request_id, "shared-1");
    assert_eq!(request.command.as_str(), "handshake");
}
