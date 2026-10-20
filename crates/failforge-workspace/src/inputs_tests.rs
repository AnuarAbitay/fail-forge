use super::*;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "failforge-profile-inputs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn duplicate_metadata_is_rejected_at_every_nesting_level() {
    for text in [
        r#"{"packages":[],"packages":[]}"#,
        r#"{"packages":[{"name":"a","name":"b"}]}"#,
    ] {
        assert!(serde_json::from_str::<UniqueValue>(text).is_err());
    }
    assert!(serde_json::from_str::<UniqueValue>(r#"{"n":null,"b":true,"a":[1,2.5,"s"]}"#).is_ok());
}

#[test]
fn local_package_uris_preserve_spaces_unicode_and_relative_roots() {
    let fixture = Fixture::new();
    let package = fixture.0.join("some space Ж");
    fs::create_dir(&package).unwrap();
    let config = fixture.0.join("config.json");
    assert_eq!(
        local_uri(&config, "some%20space%20%D0%96").unwrap(),
        package
    );
    assert_eq!(
        local_uri(&config, "%2Fsome%20space%20%D0%96").unwrap(),
        package
    );
    assert_eq!(
        local_uri(
            &config,
            &file_uri(&package).unwrap().replacen("file:", "FILE:", 1)
        )
        .unwrap(),
        package
    );
    assert_eq!(
        local_uri(&config, &file_uri(&package).unwrap()[5..]).unwrap(),
        package
    );
    let outside = Fixture::new();
    symlink(&outside.0, fixture.0.join("alias")).unwrap();
    assert_eq!(
        local_uri(&config, "alias/../some%20space%20%D0%96").unwrap(),
        package
    );
    assert_eq!(
        local_uri(&config, &file_uri(&package).unwrap()).unwrap(),
        package
    );
    assert_eq!(
        local_uri(
            &config,
            &format!(
                "file://localhost{}",
                file_uri(&package).unwrap().strip_prefix("file://").unwrap()
            )
        )
        .unwrap(),
        package
    );
    for bad in [
        "https://example.com/a",
        "file://remote/a",
        "//remote/a",
        "a%00b",
        "a\\b",
        "",
        "#fragment",
        "file:",
    ] {
        assert!(local_uri(&config, bad).is_err(), "{bad}");
    }
}

#[test]
fn snapshot_rebases_only_prepared_roots_and_retains_frozen_source_bytes() {
    let fixture = Fixture::new();
    let original = fixture.0.join("original");
    let test_package = fixture.0.join("test dependency");
    fs::create_dir_all(original.join(".dart_tool/test")).unwrap();
    fs::create_dir_all(original.join("lib")).unwrap();
    fs::create_dir(&test_package).unwrap();
    fs::write(original.join("pubspec.yaml"), "name: subject\n").unwrap();
    fs::write(original.join("lib/source.dart"), "bool f() => true;\n").unwrap();
    fs::write(original.join(".dart_tool/test/old-cache"), "disposable").unwrap();
    let config = json!({"configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/"},{"name":"test","rootUri":file_uri(&test_package).unwrap(),"packageUri":"lib/"}]});
    let graph = json!({"roots":["subject"],"packages":[{"name":"subject","dependencies":["test"]},{"name":"test","dependencies":[]}]});
    fs::write(
        original.join(".dart_tool/package_config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    fs::write(
        original.join(".dart_tool/package_graph.json"),
        serde_json::to_vec(&graph).unwrap(),
    )
    .unwrap();
    let cancel = Cancellation::default();
    let project = inspect_project(&original, &cancel).unwrap();
    let hashes = source_hashes(&original, &cancel).unwrap();
    let snapshot = copy_snapshot(&project, &fixture.0.join("snapshot"), &hashes, &cancel).unwrap();
    assert_eq!(snapshot.packages["subject"], snapshot.root);
    assert_eq!(snapshot.packages["test"], test_package);
    assert_eq!(
        fs::read(snapshot.root.join("lib/source.dart")).unwrap(),
        fs::read(original.join("lib/source.dart")).unwrap()
    );
    assert!(!snapshot.root.join(".dart_tool/test").exists());
    verify_source(&original, &hashes, &cancel).unwrap();
    fs::write(original.join("lib/source.dart"), "changed").unwrap();
    assert!(
        copy_snapshot(
            &project,
            &fixture.0.join("changed-snapshot"),
            &hashes,
            &cancel
        )
        .is_err()
    );
}

#[test]
fn hashes_detect_additions_changes_and_reject_source_symlinks() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("source.dart"), "one").unwrap();
    let cancel = Cancellation::default();
    let before = source_hashes(&fixture.0, &cancel).unwrap();
    fs::create_dir(fixture.0.join(".failforge")).unwrap();
    fs::write(fixture.0.join(".failforge/report"), "ignored").unwrap();
    verify_source(&fixture.0, &before, &cancel).unwrap();
    fs::write(fixture.0.join("source.dart"), "two").unwrap();
    assert!(verify_source(&fixture.0, &before, &cancel).is_err());
    symlink("source.dart", fixture.0.join("alias.dart")).unwrap();
    assert!(source_hashes(&fixture.0, &cancel).is_err());
    assert!(bound_tree(&fixture.0, &cancel).is_ok());
    fs::remove_file(fixture.0.join("alias.dart")).unwrap();
    symlink("/", fixture.0.join("external")).unwrap();
    assert!(bound_tree(&fixture.0, &cancel).is_err());
}

#[test]
fn file_reader_never_blocks_on_fifo_and_enforces_size() {
    let fixture = Fixture::new();
    let path = fixture.0.join("pipe");
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the C string is live and names a new test-only path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(read_regular(&path, 5).is_err());
    fs::write(fixture.0.join("file"), b"123456").unwrap();
    assert!(read_regular(&fixture.0.join("file"), 5).is_err());
}

#[test]
fn streaming_hashes_match_sha256_vectors_across_multiple_read_blocks() {
    let fixture = Fixture::new();
    let path = fixture.0.join("input");
    let cancel = Cancellation::default();
    for (bytes, expected) in [
        (
            vec![],
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc".to_vec(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            vec![b'a'; 1_000_000],
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
        ),
    ] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(hash_file(&path, &cancel).unwrap(), expected);
    }
    cancel.request(libc::SIGTERM);
    assert!(matches!(
        hash_file(&path, &cancel),
        Err(InputError::Cancelled(libc::SIGTERM))
    ));
}

#[test]
fn bound_tree_records_file_alias_bytes_and_directory_alias_resolution_once() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("real")).unwrap();
    fs::write(fixture.0.join("real/one"), b"abc").unwrap();
    symlink("real", fixture.0.join("directory-alias")).unwrap();
    symlink("real/one", fixture.0.join("file-alias")).unwrap();
    symlink(".", fixture.0.join("root-alias")).unwrap();
    let binding = bound_tree(&fixture.0, &Cancellation::default()).unwrap();
    let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(
        serde_json::to_value(&binding).unwrap(),
        json!({
            "root": fixture.0,
            "files": {"real/one": hash, "file-alias": hash},
            "aliases": {
                "directory-alias": {"link":"real", "target":"real"},
                "file-alias": {"link":"real/one", "target":"real/one"},
                "root-alias": {"link":".", "target":"."}
            }
        })
    );
    let roundtrip: BoundTree =
        serde_json::from_value(serde_json::to_value(&binding).unwrap()).unwrap();
    assert_eq!(roundtrip, binding);
    fs::remove_file(fixture.0.join("file-alias")).unwrap();
    symlink("./real/one", fixture.0.join("file-alias")).unwrap();
    assert_ne!(
        bound_tree(&fixture.0, &Cancellation::default()).unwrap(),
        binding
    );
}

#[test]
fn bound_tree_rejects_broken_cyclic_external_and_excluded_alias_targets() {
    let cancel = Cancellation::default();
    for target in [
        "missing",
        "alias",
        ".git/file",
        ".dart_tool/file",
        "build/file",
    ] {
        let fixture = Fixture::new();
        for directory in [".git", ".dart_tool", "build"] {
            fs::create_dir(fixture.0.join(directory)).unwrap();
            fs::write(fixture.0.join(directory).join("file"), "ignored").unwrap();
        }
        symlink(target, fixture.0.join("alias")).unwrap();
        assert!(bound_tree(&fixture.0, &cancel).is_err(), "{target}");
    }
    let fixture = Fixture::new();
    let outside = Fixture::new();
    fs::write(outside.0.join("file"), "external").unwrap();
    symlink(outside.0.join("file"), fixture.0.join("alias")).unwrap();
    assert!(bound_tree(&fixture.0, &cancel).is_err());
}

#[test]
fn source_and_bound_trees_keep_their_distinct_exclusion_rules() {
    let fixture = Fixture::new();
    for relative in [
        ".git/ignored",
        ".dart_tool/ignored",
        "build/ignored",
        ".failforge/report",
        "__pycache__/cached",
        "nested/build/input",
        "nested/incremental_kernel_keep",
        "incremental_kernel_kept.dart",
    ] {
        let path = fixture.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "bytes").unwrap();
    }
    fs::write(fixture.0.join("incremental_kernel_disposable"), "cache").unwrap();
    let cancel = Cancellation::default();
    let source = source_hashes(&fixture.0, &cancel).unwrap();
    assert_eq!(
        source.keys().cloned().collect::<BTreeSet<_>>(),
        BTreeSet::from([
            PathBuf::from("nested/build/input"),
            PathBuf::from("nested/incremental_kernel_keep"),
            PathBuf::from("incremental_kernel_kept.dart"),
        ])
    );
    let bound = bound_tree(&fixture.0, &cancel).unwrap();
    assert_eq!(bound.files.len(), 6);
    assert!(bound.files.contains_key(Path::new(".failforge/report")));
    assert!(bound.files.contains_key(Path::new("__pycache__/cached")));
    assert!(
        bound
            .files
            .contains_key(Path::new("incremental_kernel_disposable"))
    );
}

#[test]
fn broken_prepared_metadata_aliases_are_rejected_instead_of_skipped() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join(".dart_tool")).unwrap();
    let cancel = Cancellation::default();
    for relative in [
        ".dart_tool/package_config.json",
        ".dart_tool/package_graph.json",
    ] {
        let path = fixture.0.join(relative);
        symlink("missing", &path).unwrap();
        assert!(source_hashes(&fixture.0, &cancel).is_err());
        fs::remove_file(path).unwrap();
    }
    fs::remove_dir(fixture.0.join(".dart_tool")).unwrap();
    symlink("missing", fixture.0.join(".dart_tool")).unwrap();
    assert!(source_hashes(&fixture.0, &cancel).is_err());
}

fn prepared_project(fixture: &Fixture, framework: &str, collision: bool) -> PathBuf {
    let root = fixture.0.join("subject");
    let dependency = fixture.0.join("dependency");
    fs::create_dir_all(root.join(".dart_tool")).unwrap();
    fs::create_dir(&dependency).unwrap();
    fs::write(root.join("pubspec.yaml"), "name: subject\n").unwrap();
    let config = json!({"configVersion":2,"packages":[
        {"name":"subject","rootUri":"../"},
        {"name":framework,"rootUri":file_uri(&dependency).unwrap()}
    ]});
    let graph = json!({"roots":["subject"], "packages":[
        {"name":"subject","dependencies":[framework]},
        {"name":framework,"dependencies":if collision {vec!["subject"]} else {vec![]}}
    ]});
    fs::write(
        root.join(".dart_tool/package_config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join(".dart_tool/package_graph.json"),
        serde_json::to_vec(&graph).unwrap(),
    )
    .unwrap();
    root
}

#[test]
fn prepared_project_preserves_graph_and_detects_framework_collisions() {
    for framework in ["test", "flutter_test"] {
        for collision in [false, true] {
            let fixture = Fixture::new();
            let root = prepared_project(&fixture, framework, collision);
            let project = inspect_project(&root, &Cancellation::default()).unwrap();
            assert_eq!(project.root, root);
            assert_eq!(project.name, "subject");
            assert_eq!(project.flutter, framework == "flutter_test");
            assert_eq!(project.framework_collision, collision);
            assert_eq!(
                project.graph,
                metadata_json(&root.join(".dart_tool/package_graph.json")).unwrap()
            );
            let wire = serde_json::to_value(&project).unwrap();
            assert_eq!(wire["frameworkCollision"], collision);
            assert_eq!(
                serde_json::from_value::<PreparedProject>(wire).unwrap(),
                project
            );
        }
    }
}

#[test]
fn malformed_prepared_graphs_are_rejected_before_snapshotting() {
    for graph in [
        json!({"roots":[],"packages":[]}),
        json!({"roots":["subject","other"],"packages":[]}),
        json!({"roots":["other"],"packages":[]}),
        json!({"roots":["subject"],"packages":[{"name":"subject"}]}),
        json!({"roots":["subject"],"packages":[{"name":"test"}]}),
        json!({"roots":["subject"],"packages":[{"name":"subject"},{"name":"test","dependencies":[null]}]}),
        json!({"roots":["subject"],"packages":[{"name":"subject"},{"name":"test"},{"name":"test"}]}),
    ] {
        let fixture = Fixture::new();
        let root = prepared_project(&fixture, "test", false);
        fs::write(
            root.join(".dart_tool/package_graph.json"),
            serde_json::to_vec(&graph).unwrap(),
        )
        .unwrap();
        assert!(
            inspect_project(&root, &Cancellation::default()).is_err(),
            "{graph}"
        );
    }
}

#[test]
fn bounded_readers_reject_aliases_and_expose_only_the_requested_prefix() {
    let fixture = Fixture::new();
    let path = fixture.0.join("file");
    fs::write(&path, "123456").unwrap();
    assert_eq!(read_regular_prefix(&path, 2).unwrap(), b"123");
    assert_eq!(read_regular_prefix(&path, 0).unwrap(), b"1");
    assert_eq!(read_regular(&path, 6).unwrap(), b"123456");
    assert_eq!(read_regular(&path, usize::MAX).unwrap(), b"123456");
    symlink("file", fixture.0.join("alias")).unwrap();
    assert!(read_regular(&fixture.0.join("alias"), 6).is_err());
    assert!(read_regular(&fixture.0, 6).is_err());
}

#[test]
fn cancellation_prevents_reading_project_and_tree_contents() {
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    let fixture = Fixture::new();
    let root = prepared_project(&fixture, "test", false);
    for result in [
        inspect_project(&root, &cancel).map(|_| ()),
        source_hashes(&root, &cancel).map(|_| ()),
        bound_tree(&root, &cancel).map(|_| ()),
    ] {
        assert!(matches!(result, Err(InputError::Cancelled(libc::SIGTERM))));
    }
}

#[test]
fn source_binding_detects_added_removed_and_modified_prepared_inputs() {
    let fixture = Fixture::new();
    let root = prepared_project(&fixture, "test", false);
    let cancel = Cancellation::default();
    let original = source_hashes(&root, &cancel).unwrap();
    assert!(original.contains_key(Path::new(".dart_tool/package_config.json")));
    assert!(original.contains_key(Path::new(".dart_tool/package_graph.json")));
    let added = root.join("new.dart");
    fs::write(&added, "new input").unwrap();
    assert!(verify_source(&root, &original, &cancel).is_err());
    fs::remove_file(added).unwrap();
    verify_source(&root, &original, &cancel).unwrap();
    let metadata = root.join(".dart_tool/package_graph.json");
    let original_metadata = fs::read(&metadata).unwrap();
    fs::write(&metadata, "{}").unwrap();
    assert!(verify_source(&root, &original, &cancel).is_err());
    fs::write(&metadata, original_metadata).unwrap();
    verify_source(&root, &original, &cancel).unwrap();
    fs::remove_file(metadata).unwrap();
    assert!(verify_source(&root, &original, &cancel).is_err());
}

#[test]
fn package_configuration_requires_unique_valid_versioned_local_entries() {
    let fixture = Fixture::new();
    let root = prepared_project(&fixture, "test", false);
    let config = root.join(".dart_tool/package_config.json");
    for value in [
        json!({"configVersion":true,"packages":[]}),
        json!({"configVersion":2.0,"packages":[]}),
        json!({"configVersion":2,"packages":{}}),
        json!({"configVersion":2,"packages":[null]}),
        json!({"configVersion":2,"packages":[{"name":"","rootUri":"../"}]}),
        json!({"configVersion":2,"packages":[{"name":"a","rootUri":"../"},{"name":"a","rootUri":"../"}]}),
        json!({"configVersion":2,"packages":[{"name":"a","rootUri":"https://example.invalid/"}]}),
        json!({"configVersion":2,"packages":[{"name":"a","rootUri":"missing"}]}),
        json!({"configVersion":2,"packages":[{"name":"a","rootUri":"../pubspec.yaml"}]}),
    ] {
        fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(dependency_roots(&config).is_err(), "{value}");
    }
}

#[test]
fn package_configuration_rejects_different_names_for_one_canonical_root() {
    let fixture = Fixture::new();
    let package = fixture.0.join("dependency");
    fs::create_dir(&package).unwrap();
    symlink(&package, fixture.0.join("dependency-alias")).unwrap();
    let config = fixture.0.join("package_config.json");
    for alias in [
        "dependency".to_owned(),
        "./dependency/".to_owned(),
        "dependency/../dependency".to_owned(),
        "%64ependency/".to_owned(),
        "dependency-alias/".to_owned(),
        file_uri(&package).unwrap(),
    ] {
        let value = json!({"configVersion":2,"packages":[
            {"name":"first","rootUri":"dependency/","packageUri":"lib/"},
            {"name":"second","rootUri":alias,"packageUri":"other/"}
        ]});
        fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
        for result in [
            dependency_roots(&config),
            dependency_roots_from_value(&config, &value),
        ] {
            assert!(
                matches!(result, Err(InputError::Invalid(ref message)) if message == "prepared dependencies must have distinct roots"),
                "{alias}: {result:?}"
            );
        }
    }
}

#[test]
fn duplicate_dependency_names_keep_their_existing_diagnostic() {
    let fixture = Fixture::new();
    for directory in ["first", "second"] {
        fs::create_dir(fixture.0.join(directory)).unwrap();
    }
    let config = fixture.0.join("package_config.json");
    for root in ["first/", "second/"] {
        let value = json!({"configVersion":2,"packages":[
            {"name":"same","rootUri":"first/"},
            {"name":"same","rootUri":root}
        ]});
        let result = dependency_roots_from_value(&config, &value);
        assert!(
            matches!(result, Err(InputError::Invalid(ref message)) if message == "duplicate prepared dependency"),
            "{root}: {result:?}"
        );
    }
}

#[test]
fn package_configuration_allows_distinct_roots_including_nested_packages() {
    let fixture = Fixture::new();
    for directory in ["outer/lib", "outer/tools/nested/lib", "outer-sibling/lib"] {
        fs::create_dir_all(fixture.0.join(directory)).unwrap();
    }
    let config = fixture.0.join("package_config.json");
    let value = json!({"configVersion":2,"packages":[
        {"name":"outer","rootUri":"outer/","packageUri":"lib/"},
        {"name":"nested","rootUri":"outer/tools/nested/","packageUri":"lib/"},
        {"name":"sibling","rootUri":"outer-sibling/","packageUri":"lib/"}
    ]});
    fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
    let expected = BTreeMap::from([
        ("outer".into(), fixture.0.join("outer")),
        ("nested".into(), fixture.0.join("outer/tools/nested")),
        ("sibling".into(), fixture.0.join("outer-sibling")),
    ]);
    assert_eq!(dependency_roots(&config).unwrap(), expected);
    assert_eq!(
        dependency_roots_from_value(&config, &value).unwrap(),
        expected
    );
}

#[test]
fn project_inspection_rejects_another_name_bound_to_the_subject_root() {
    let fixture = Fixture::new();
    let root = prepared_project(&fixture, "test", false);
    let config = root.join(".dart_tool/package_config.json");
    let mut value = metadata_json(&config).unwrap();
    value["packages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"shadow","rootUri":file_uri(&root).unwrap()}));
    fs::write(&config, serde_json::to_vec(&value).unwrap()).unwrap();
    let result = inspect_project(&root, &Cancellation::default());
    assert!(
        matches!(result, Err(InputError::Invalid(ref message)) if message == "prepared dependencies must have distinct roots"),
        "{result:?}"
    );
}

#[test]
fn integer_metadata_is_never_silently_converted_into_a_rounded_float() {
    for integer in [
        "18446744073709551616",
        "18446744073709551617",
        "-9223372036854775809",
    ] {
        let raw = format!("{{\"extra\":[{{\"large\":{integer}}}]}}");
        assert!(
            parse_json(raw.as_bytes()).is_err(),
            "{integer} must not be rounded"
        );
    }
}

#[test]
fn integer_guards_preserve_exact_boundaries_strings_and_finite_floats() {
    let value = parse_json(br#"{"signed":-9223372036854775808,"unsigned":18446744073709551615,"zero":-0,"float":18446744073709551616.0,"exponent":1e30,"quoted":"escaped \"18446744073709551617\"\\tail"}"#).unwrap();
    assert_eq!(value["signed"].as_i64(), Some(i64::MIN));
    assert_eq!(value["unsigned"].as_u64(), Some(u64::MAX));
    assert_eq!(value["zero"].as_u64(), Some(0));
    assert_eq!(value["float"].as_f64(), Some(18446744073709551616.0));
    assert_eq!(value["exponent"].as_f64(), Some(1e30));
    assert_eq!(value["quoted"], "escaped \"18446744073709551617\"\\tail");
    for invalid in [
        "[1e400]",
        "[NaN]",
        "[Infinity]",
        "[01]",
        "[--1]",
        "[+1]",
        "[1e]",
        "[.5]",
        "[1.]",
        "\"unterminated\\",
    ] {
        assert!(parse_json(invalid.as_bytes()).is_err(), "{invalid}");
    }
}

#[test]
fn finite_float_metadata_retains_correctly_rounded_bits_when_serialized() {
    for token in [
        "51.248178375505404",
        "51.24817837550541",
        "-93.31137037688033",
        "1.7976931348623157e308",
        "4.9406564584124654e-324",
    ] {
        let value = parse_json(token.as_bytes()).unwrap();
        let expected = token.parse::<f64>().unwrap();
        assert_eq!(
            value.as_f64().unwrap().to_bits(),
            expected.to_bits(),
            "{token}"
        );
        let written = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            parse_json(&written).unwrap().as_f64().unwrap().to_bits(),
            expected.to_bits()
        );
    }
}
