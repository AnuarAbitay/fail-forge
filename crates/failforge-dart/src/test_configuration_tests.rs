use super::*;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-test-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        for directory in ["copy/.dart_tool", "framework", "test-package"] {
            fs::create_dir_all(path.join(directory)).unwrap();
        }
        Self(path)
    }

    fn copy(&self) -> PathBuf {
        self.0.join("copy")
    }

    fn config(&self) -> Value {
        json!({"configVersion":2,"packages":[
            {"name":"failforge_subject","rootUri":"../","packageUri":"lib/"},
            {"name":"path","rootUri":"../../framework","packageUri":"lib/"},
            {"name":"test","rootUri":"../../test-package","packageUri":"lib/"}
        ]})
    }

    fn write(&self, value: &Value) -> Vec<u8> {
        let raw = serde_json::to_vec(value).unwrap();
        fs::write(self.copy().join(".dart_tool/package_config.json"), &raw).unwrap();
        raw
    }

    fn check(&self) -> Result<(), InputError> {
        validate_test_configuration(&self.copy(), "failforge_subject")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn isolated_and_ordinary_subject_bindings_are_valid_without_rewriting_metadata() {
    let fixture = Fixture::new();
    let original = fixture.write(&fixture.config());
    fixture.check().unwrap();
    assert_eq!(
        fs::read(fixture.copy().join(".dart_tool/package_config.json")).unwrap(),
        original
    );
    let mut ordinary = fixture.config();
    ordinary["packages"][0]["name"] = "subject".into();
    fixture.write(&ordinary);
    validate_test_configuration(&fixture.copy(), "subject").unwrap();
}

#[test]
fn current_subject_cannot_resolve_to_a_different_fresh_copy() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("previous-copy")).unwrap();
    let mut config = fixture.config();
    config["packages"][0]["rootUri"] = "../../previous-copy".into();
    let original = fixture.write(&config);
    assert!(
        fixture
            .check()
            .unwrap_err()
            .to_string()
            .contains("working copy"),
        "a valid package graph targeting stale source must still be rejected"
    );
    assert_eq!(
        fs::read(fixture.copy().join(".dart_tool/package_config.json")).unwrap(),
        original
    );
}

#[test]
fn duplicate_framework_and_subject_roots_are_rejected_before_a_runner_is_needed() {
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config["packages"][0]["rootUri"] = "../../framework".into();
    fixture.write(&config);
    assert!(
        fixture
            .check()
            .unwrap_err()
            .to_string()
            .contains("distinct roots")
    );
}

#[test]
fn missing_malformed_and_ambiguous_metadata_is_rejected_without_writes() {
    let fixture = Fixture::new();
    assert!(fixture.check().is_err());
    let path = fixture.copy().join(".dart_tool/package_config.json");
    for raw in [
        &b"{"[..],
        &b"{\"configVersion\":2,\"configVersion\":1,\"packages\":[]}"[..],
        &b"{\"configVersion\":1,\"packages\":[]}"[..],
        &b"{\"configVersion\":2,\"packages\":[]}"[..],
    ] {
        fs::write(&path, raw).unwrap();
        assert!(fixture.check().is_err());
        assert_eq!(fs::read(&path).unwrap(), raw);
    }
}

#[test]
fn alias_is_required_even_when_the_config_is_otherwise_valid() {
    let fixture = Fixture::new();
    fixture.write(&fixture.config());
    for alias in ["", "absent", "path", "test"] {
        assert!(validate_test_configuration(&fixture.copy(), alias).is_err());
    }
}
