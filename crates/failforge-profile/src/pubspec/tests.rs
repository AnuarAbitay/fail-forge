use super::*;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "failforge-pubspec-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        fs::write(root.join("pubspec.yaml"), "name: example\n").unwrap();
        Self(root)
    }
    fn metadata(&self, text: &str) {
        fs::create_dir_all(self.0.join(".dart_tool")).unwrap();
        fs::write(self.0.join(".dart_tool/package_config.json"), text).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn author_aliases_merges_nulls_and_ordinary_scalar_spellings_are_supported() {
    for text in [
        "name: example\nflutter_sdk: &sdk {sdk: flutter}\ndev_dependencies: {flutter_test: *sdk}",
        "name: example\nsdk: &sdk {sdk: flutter}\ndev_dependencies: {flutter_test: {<<: *sdk}}",
        "defaults: &defaults {dev_dependencies: {flutter_test: {sdk: flutter}}}\n<<: *defaults\n",
        "deps: &deps {flutter_test: {sdk: flutter}}\ndev_dependencies: {<<: *deps}\n",
        "metadata: [yes, no, ON, OFF, 012, 1:20, 1.5, 2001-12-15]\ndependencies: null\ndev_dependencies: {flutter_test: {sdk: !!str flutter}}",
        "dependencies: {flutter_test: {sdk: flutter}}\ndev_dependencies: ~\n",
    ] {
        assert!(pubspec_flutter(text).unwrap(), "{text}");
    }
    for text in [
        "dependencies:\ndev_dependencies:\n",
        "dependencies: Null\ndev_dependencies: NULL",
        "dependencies: {flutter: {sdk: flutter}}\n",
    ] {
        assert!(!pubspec_flutter(text).unwrap(), "{text}");
    }
}

#[test]
fn explicit_values_override_merges_and_first_mapping_in_merge_sequence_wins() {
    let preamble = "dart: &dart {sdk: dart}\nflutter: &flutter {sdk: flutter}\n";
    assert!(
        !pubspec_flutter(&format!(
            "{preamble}dev_dependencies: {{flutter_test: {{<<: *flutter, sdk: dart}}}}"
        ))
        .unwrap()
    );
    assert!(
        !pubspec_flutter(&format!(
            "{preamble}dev_dependencies: {{flutter_test: {{<<: [*dart, *flutter]}}}}"
        ))
        .unwrap()
    );
    assert!(
        pubspec_flutter(&format!(
            "{preamble}dev_dependencies: {{flutter_test: {{<<: [*flutter, *dart]}}}}"
        ))
        .unwrap()
    );
    assert!(
        !pubspec_flutter("dev_dependencies: {flutter_test: {sdk: flutter, sdk: dart}}").unwrap()
    );
    assert!(
        pubspec_flutter("dev_dependencies: {flutter_test: {sdk: dart, sdk: flutter}}").unwrap()
    );
}

#[test]
fn malformed_shape_unknown_tags_aliases_and_unhashable_keys_are_rejected() {
    for text in [
        "[unterminated",
        "[]",
        "",
        "dependencies: []",
        "dev_dependencies: yes",
        "!!python/object:bad {}",
        "dependencies: *missing",
        "a: &same {}\nb: &same {}",
        "? [a, b]\n: value",
        "unrelated: {<<: 1}",
        "unrelated: {<<: [{}, 1]}",
        "---\n{}\n---\n{}",
        "name: safe\0\nunknown: bad",
    ] {
        assert!(pubspec_flutter(text).is_err(), "accepted {text:?}");
    }
}

#[test]
fn yaml_standard_set_and_ordered_mapping_tags_retain_their_python_shapes() {
    assert!(!pubspec_flutter("dependencies: {flutter_test: !!set {sdk: flutter}}").unwrap());
    assert!(pubspec_flutter("dependencies: !!set {flutter_test: null}").is_err());
    assert!(pubspec_flutter("!!set {dependencies: null}").is_err());
    assert!(pubspec_flutter("other: !!omap [{a: 1, b: 2}]").is_err());
    assert!(pubspec_flutter("other: !!pairs [42]").is_err());
    assert!(!pubspec_flutter("other: !!omap [{a: 1}, {b: 2}]").unwrap());
    assert!(!pubspec_flutter("dependencies: {flutter_test: !!pairs [{sdk: flutter}]}").unwrap());
}

#[test]
fn author_yaml_preserves_supported_minor_versions_and_ignored_directives() {
    for preamble in [
        "%YAML 1.3\n---\n",
        "%YAML 1.0\n---\n",
        "%custom ignored\n---\n",
    ] {
        assert!(
            pubspec_flutter(&format!(
                "{preamble}dev_dependencies: {{flutter_test: {{sdk: flutter}}}}\n"
            ))
            .unwrap()
        );
    }
    assert!(pubspec_flutter("%TAG ! tag:yaml.org,2002:\n---\nname: fixture\ndev_dependencies: !map {flutter_test: {sdk: flutter}}\n").unwrap());
    assert!(pubspec_flutter("%custom ignored\n%TAG ! tag:yaml.org,2002:\n%YAML 1.3\n---\ndev_dependencies: !map {flutter_test: {sdk: flutter}}\n").unwrap());
    assert!(
        pubspec_flutter("%TAG ! tag:python.org,2002:\n---\ndev_dependencies: !map {}\n").is_err()
    );
}

#[test]
fn quoted_merge_keys_are_ordinary_keys_and_recursive_aliases_do_not_expand() {
    assert!(!pubspec_flutter("dev_dependencies: {flutter_test: {'<<': {sdk: flutter}}}").unwrap());
    assert!(!pubspec_flutter("name: example\nrecursive: &self {child: *self}\n").unwrap());
    let mut text = String::from("a0: &a0 [yes]\n");
    for i in 1..64 {
        text.push_str(&format!("a{i}: &a{i} [*a{}, *a{}]\n", i - 1, i - 1));
    }
    assert!(!pubspec_flutter(&text).unwrap());
}

#[test]
fn merged_projection_stays_bounded_by_four_relevant_keys() {
    let mut text = String::from("a0: &a0 {sdk: flutter, unrelated0: value}\n");
    for i in 1..500 {
        text.push_str(&format!(
            "a{i}: &a{i} {{<<: [*a{}, *a{}], unrelated{i}: value}}\n",
            i - 1,
            i - 1
        ));
    }
    text.push_str("dev_dependencies: {flutter_test: *a499}\n");
    let (arena, _) = author_yaml(&text).unwrap();
    let mut cache = HashMap::new();
    for (id, node) in arena.iter().enumerate() {
        if matches!(node, Node::Mapping(_)) {
            mapping(&arena, id, &mut cache, &mut HashSet::new()).unwrap();
        }
    }
    assert!(cache.values().all(|projection| projection.len() <= 4));
    assert!(pubspec_flutter(&text).unwrap());
}

#[test]
fn nested_input_depth_and_oversized_metadata_are_bounded() {
    assert!(
        pubspec_flutter(&format!(
            "value: {}null{}",
            "[".repeat(130),
            "]".repeat(130)
        ))
        .is_err()
    );
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("pubspec.yaml"),
        vec![b'#'; MAX_YAML_BYTES + 1],
    )
    .unwrap();
    assert!(
        flutter_package(&fixture.0)
            .unwrap_err()
            .to_string()
            .contains("exceeds 1 MiB")
    );
    fs::write(fixture.0.join("pubspec.yaml"), "name: example\n").unwrap();
    fixture.metadata(&" ".repeat(8 * 1024 * 1024 + 1));
    assert!(
        flutter_package(&fixture.0)
            .unwrap_err()
            .to_string()
            .contains("exceeds 8 MiB")
    );
}

#[test]
fn prepared_framework_overrides_pubspec_and_never_probes_dependency_roots() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.join("pubspec.yaml"),
        "dev_dependencies: {flutter_test: {sdk: flutter}}",
    )
    .unwrap();
    assert!(flutter_package(&fixture.0).unwrap());
    fixture
        .metadata(r#"{"configVersion":2,"packages":[{"name":"test","rootUri":"../absent/test"}]}"#);
    assert!(!flutter_package(&fixture.0).unwrap());
    fs::write(fixture.0.join("pubspec.yaml"), "name: example\n").unwrap();
    fixture.metadata(r#"{"configVersion":2,"packages":[{"name":"flutter_test","rootUri":"../absent/flutter_test"}]}"#);
    assert!(flutter_package(&fixture.0).unwrap());
    assert!(!fixture.0.join("absent").exists());
}

#[test]
fn dependency_metadata_requires_exact_version_unique_names_and_valid_roots() {
    let fixture = Fixture::new();
    for text in [
        "{broken",
        "[]",
        r#"{"configVersion":true,"packages":[]}"#,
        r#"{"configVersion":2.0,"packages":[]}"#,
        r#"{"configVersion":3,"packages":[]}"#,
        r#"{"configVersion":2,"packages":{}}"#,
        r#"{"configVersion":2,"packages":["flutter_test"]}"#,
        r#"{"configVersion":2,"packages":[{"name":"","rootUri":"any"}]}"#,
        r#"{"configVersion":2,"packages":[{"name":"test","rootUri":""}]}"#,
        r#"{"configVersion":2,"packages":[{"name":"test"}]}"#,
        r#"{"configVersion":2,"packages":[{"name":"test\u0000","rootUri":"any"}]}"#,
        r#"{"configVersion":2,"packages":[{"name":"test","rootUri":"any"},{"name":"test","rootUri":"other"}]}"#,
    ] {
        fixture.metadata(text);
        assert!(flutter_package(&fixture.0).is_err(), "accepted {text}");
    }
}

#[test]
fn duplicate_json_keys_are_rejected_at_every_depth() {
    let fixture = Fixture::new();
    for text in [
        r#"{"configVersion":2,"packages":[],"packages":[]}"#,
        r#"{"configVersion":2,"packages":[{"name":"test","name":"flutter_test","rootUri":"any"}]}"#,
        r#"{"configVersion":2,"packages":[],"extra":[{"x":1,"x":2}]}"#,
    ] {
        fixture.metadata(text);
        assert!(
            flutter_package(&fixture.0)
                .unwrap_err()
                .to_string()
                .contains("duplicate package_config.json key")
        );
    }
}

#[test]
#[cfg(unix)]
fn malformed_utf8_and_package_metadata_symlinks_are_rejected() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("pubspec.yaml"), b"name: \xff\n").unwrap();
    assert!(
        flutter_package(&fixture.0)
            .unwrap_err()
            .to_string()
            .contains("UTF-8 YAML")
    );
    fs::write(fixture.0.join("pubspec.yaml"), "name: example\n").unwrap();
    fixture.metadata("{}");
    fs::write(fixture.0.join(".dart_tool/package_config.json"), b"\xff").unwrap();
    assert!(
        flutter_package(&fixture.0)
            .unwrap_err()
            .to_string()
            .contains("UTF-8 JSON")
    );
    fs::remove_file(fixture.0.join(".dart_tool/package_config.json")).unwrap();
    fs::write(
        fixture.0.join("metadata.json"),
        r#"{"configVersion":2,"packages":[]}"#,
    )
    .unwrap();
    symlink(
        fixture.0.join("metadata.json"),
        fixture.0.join(".dart_tool/package_config.json"),
    )
    .unwrap();
    assert!(flutter_package(&fixture.0).is_err());
    fs::remove_dir_all(fixture.0.join(".dart_tool")).unwrap();
    symlink(&fixture.0, fixture.0.join(".dart_tool")).unwrap();
    assert!(
        flutter_package(&fixture.0)
            .unwrap_err()
            .to_string()
            .contains(".dart_tool must not be a symlink")
    );
}

#[test]
fn valid_json_extra_metadata_scalar_types_do_not_change_framework_selection() {
    let fixture = Fixture::new();
    fixture.metadata(
        r#"{"configVersion":2,"packages":[],"extra":[null,true,false,-1,2,1.5,"текст",{},[]]}"#,
    );
    assert!(!flutter_package(&fixture.0).unwrap());
}

#[test]
fn invalid_explicit_safe_scalar_constructors_are_rejected() {
    for scalar in [
        "!!int nope",
        "!!int 0b012",
        "!!float nope",
        "!!timestamp nope",
        "!!timestamp 2024-99-99",
        "!!timestamp 2023-02-29",
        "!!timestamp 2024-02-29T24:00:00",
        "!!timestamp 2024-02-29T23:59:59+24:00",
        "!!binary xyz",
        "!!binary привет",
        "!!bool maybe",
        "2024-99-99",
        "<<",
        "=",
    ] {
        assert!(
            pubspec_flutter(&format!("metadata: {scalar}\n")).is_err(),
            "accepted {scalar}"
        );
    }
    for scalar in [
        "!!int +0xF_f",
        "!!int '12345678901234567890123456789012345678901234567890'",
        "!!int 1:20",
        "!!float .nan",
        "!!float 1",
        "!!float 1:20.5",
        "!!timestamp 2024-02-29",
        "!!timestamp 2024-02-29T23:59:59.123456789+23:59",
        "!!binary nope",
        "!!binary '!!!!'",
        "!!bool YES",
    ] {
        assert!(
            !pubspec_flutter(&format!("metadata: {scalar}\n")).unwrap(),
            "{scalar}"
        );
    }
}
