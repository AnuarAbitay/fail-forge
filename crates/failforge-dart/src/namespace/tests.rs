//! Namespace isolation regressions and transaction fault injection.
use crate::test_fs::symlink;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use failforge_workspace::Cancellation;
use serde_json::{Value, json};

use super::isolation::{HOOK, Stage};
use super::{NamespaceError, invalid, isolate_subject};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    original: PathBuf,
    copy: PathBuf,
    framework: PathBuf,
    dependency: PathBuf,
}

fn put(root: &Path, name: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    #[cfg(unix)]
    fs::write(path, bytes).unwrap();
    #[cfg(windows)]
    {
        use failforge_workspace::windows_fs;
        use std::io::Write;
        let parent = windows_fs::directory(path.parent().unwrap()).unwrap();
        let exists = path.exists();
        let mut file =
            windows_fs::regular_at(&parent, path.file_name().unwrap(), true, !exists).unwrap();
        file.set_len(0).unwrap();
        file.write_all(bytes.as_ref()).unwrap();
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &destination.join(entry.file_name()));
        } else {
            #[cfg(unix)]
            fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
            #[cfg(windows)]
            {
                use failforge_workspace::windows_fs;
                use std::io::Write;
                let parent = windows_fs::directory(destination).unwrap();
                let mut file =
                    windows_fs::regular_at(&parent, &entry.file_name(), true, true).unwrap();
                file.write_all(&fs::read(entry.path()).unwrap()).unwrap();
            }
        }
    }
}

fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                walk(root, &path, out);
            } else if entry.file_type().unwrap().is_file() {
                out.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace(std::path::MAIN_SEPARATOR, "/"),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}

impl Fixture {
    fn new() -> Self {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff namespace {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let original = base.join("author Ж 😀");
        let copy = base.join("copy Ж 😀");
        let framework = base.join("framework Ж 😀");
        let dependency = base.join("test dependency Ж");
        put(&dependency, "bin/test.dart", b"void main() {}\n");
        put(&original, "lib/api.dart", b"export 'src/value.dart';\n");
        put(&original, "lib/src/value.dart", b"const value = 1;\n");
        put(&original, "lib/assets/data.bin", [0, 255, 42]);
        put(&original, "test/main.dart", b"// Original assertions.\r\nimport 'package:test/test.dart';\r\nimport 'package:subject/api.dart';\r\nexport r'package:subject/src/value.dart';\r\nvoid main(){expect(value,1);}\r\n");
        put(
            &original,
            "test/support.dart",
            b"import '../lib/api.dart';\n",
        );
        put(
            &original,
            ".dart_tool/package_graph.json",
            br#"{"roots":["subject"]}"#,
        );
        let config = json!({"configVersion":2,"packages":[{"name":"subject","rootUri":"../","packageUri":"lib/","languageVersion":"3.13"},{"name":"test","rootUri":format!("{}/",crate::test_catalog::path_uri(&dependency).unwrap()),"packageUri":"lib/"}],"metadata":{"unicode":"Ж😀","large":u64::MAX,"negative":i64::MIN,"fraction":0.12345678901234567_f64}});
        put(
            &original,
            ".dart_tool/package_config.json",
            serde_json::to_vec_pretty(&config).unwrap(),
        );
        copy_tree(&original, &copy);
        copy_tree(&original, &framework);
        Self {
            base,
            original,
            copy,
            framework,
            dependency,
        }
    }

    fn run(&self, plan: Option<&Value>, cancel: &Cancellation) -> Result<Value, NamespaceError> {
        isolate_subject(
            &self.original,
            &self.copy,
            &self.framework,
            "failforge_subject",
            plan,
            cancel,
        )
    }

    fn set_test(&self, name: &str, bytes: impl AsRef<[u8]>) {
        put(&self.original, name, bytes.as_ref());
        put(&self.copy, name, bytes.as_ref());
    }

    fn plan(&self, edits: BTreeMap<String, Vec<Value>>) -> Value {
        let rows: Vec<_> = snapshot(&self.original).into_iter().filter(|(name,_)| name.starts_with("test/") && name.ends_with(".dart")).map(|(name,raw)| {
            json!({"file":name,"sourceHash":crate::sha256_hex(&raw),"edits":edits.get(&name).cloned().unwrap_or_default()})
        }).collect();
        json!({"version":1,"kind":"dart_namespace_plan","packageName":"subject","alias":"failforge_subject","packageConfigHash":crate::sha256_hex(&fs::read(self.original.join(".dart_tool/package_config.json")).unwrap()),"files":rows,"unsupported":[]})
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            crate::test_fs::remove_tree(&self.base);
        }
        #[cfg(unix)]
        fn writable(path: &Path) {
            let Ok(info) = fs::symlink_metadata(path) else {
                return;
            };
            if info.is_dir() {
                let _ = fs::set_permissions(path, crate::test_fs::permissions(0o700));
                if let Ok(entries) = fs::read_dir(path) {
                    for entry in entries.flatten() {
                        writable(&entry.path());
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            writable(&self.base);
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

struct Hook;
impl Hook {
    fn new(hook: impl FnMut(Stage) -> Result<(), NamespaceError> + 'static) -> Self {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Self
    }
}
impl Drop for Hook {
    fn drop(&mut self) {
        HOOK.with(|slot| slot.borrow_mut().take());
    }
}

#[cfg(unix)]
#[test]
fn posix_sync_preserves_regular_file_bytes_and_directory_entries() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.copy);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.copy.join("test/support.dart"))
        .unwrap();
    super::fs::sync(&file).unwrap();
    let directory = fs::File::open(fixture.copy.join("test")).unwrap();
    super::fs::sync(&directory).unwrap();
    assert_eq!(snapshot(&fixture.copy), before);
}

#[cfg(unix)]
#[test]
fn posix_sync_propagates_a_non_filesystem_descriptor_error() {
    use std::os::fd::FromRawFd;
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors provides the two writable integer output slots.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: successful pipe created two fresh descriptors owned only here.
    let (reader, _writer) = unsafe {
        (
            fs::File::from_raw_fd(descriptors[0]),
            fs::File::from_raw_fd(descriptors[1]),
        )
    };
    let error = super::fs::sync(&reader).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
}

#[test]
fn minimal_prefix_edits_preserve_upstream_framework_modes_and_config_values() {
    let fixture = Fixture::new();
    let original = snapshot(&fixture.original);
    let framework = snapshot(&fixture.framework);
    let config_before: Value =
        serde_json::from_slice(&original[".dart_tool/package_config.json"]).unwrap();
    fs::set_permissions(
        fixture.copy.join("test/main.dart"),
        crate::test_fs::permissions(0o444),
    )
    .unwrap();
    fs::set_permissions(
        fixture.copy.join(".dart_tool/package_config.json"),
        crate::test_fs::permissions(0o444),
    )
    .unwrap();
    let proof = fixture.run(None, &Cancellation::default()).unwrap();
    assert_eq!(
        proof
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "alias",
            "frameworkRoot",
            "frameworkLibraryHashes",
            "importProof",
            "testBin"
        ])
    );
    assert_eq!(proof["frameworkRoot"], fixture.framework.to_str().unwrap());
    assert_eq!(
        proof["testBin"],
        fixture.dependency.join("bin/test.dart").to_str().unwrap()
    );
    assert_eq!(snapshot(&fixture.original), original);
    assert_eq!(snapshot(&fixture.framework), framework);
    assert_eq!(
        proof["frameworkLibraryHashes"].as_object().unwrap().len(),
        3
    );
    let expected = String::from_utf8(original["test/main.dart"].clone())
        .unwrap()
        .replace("package:subject/", "package:failforge_subject/");
    assert_eq!(
        fs::read(fixture.copy.join("test/main.dart")).unwrap(),
        expected.as_bytes()
    );
    assert_eq!(
        fs::read(fixture.copy.join("test/support.dart")).unwrap(),
        original["test/support.dart"]
    );
    assert_eq!(proof["importProof"][0]["importDirectives"], 2);
    assert_eq!(proof["importProof"][0]["runtimePackageUris"], 0);
    assert_eq!(proof["importProof"][0]["assertionsUnchanged"], true);
    assert_eq!(
        proof["importProof"][0]["restoredSha256"],
        crate::sha256_hex(&original["test/main.dart"])
    );
    let after: Value = serde_json::from_slice(
        &fs::read(fixture.copy.join(".dart_tool/package_config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(after["metadata"], config_before["metadata"]);
    assert_eq!(after["packages"][1], config_before["packages"][1]);
    assert_eq!(after["packages"][2]["rootUri"], "../");
    assert_eq!(after["packages"][2]["name"], "failforge_subject");
    assert!(
        after["packages"][0]["rootUri"]
            .as_str()
            .unwrap()
            .contains("%20")
    );
    for name in ["test/main.dart", ".dart_tool/package_config.json"] {
        assert!(crate::test_fs::readonly(fixture.copy.join(name)));
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(fixture.copy.join(name)).unwrap().mode() & 0o777,
            0o444
        );
    }
}

#[test]
fn copied_metadata_cannot_redirect_verified_imports_or_change_dependency_semantics() {
    for tamper in 0..5 {
        let fixture = Fixture::new();
        let config_path = fixture.copy.join(".dart_tool/package_config.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        match tamper {
            0 => {
                config["packages"][0]["packageUri"] = json!("shadow/");
                put(&fixture.copy, "shadow/api.dart", b"const value = 1000;");
            }
            1 => config["packages"][0]["languageVersion"] = json!("2.12"),
            2 => config["metadata"]["large"] = json!(1),
            3 => {
                let replacement = fixture.base.join("foreign test");
                put(&replacement, "bin/test.dart", b"void main() {}");
                config["packages"][1]["rootUri"] = json!(format!(
                    "{}/",
                    crate::test_catalog::path_uri(&replacement).unwrap()
                ));
            }
            _ => put(
                &fixture.copy,
                ".dart_tool/package_graph.json",
                br#"{"roots":["subject"],"extra":"changed"}"#,
            ),
        }
        fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let before = snapshot(&fixture.copy);
        let original = snapshot(&fixture.original);
        let error = fixture
            .run(None, &Cancellation::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("namespace copied package")
                || error.contains("namespace subject packageUri"),
            "{error}"
        );
        assert_eq!(snapshot(&fixture.copy), before);
        assert_eq!(snapshot(&fixture.original), original);
    }
}

#[test]
fn legitimate_rebased_dependencies_shared_framework_and_short_alias_are_supported() {
    let fixture = Fixture::new();
    let original_path = fixture.original.join(".dart_tool/package_config.json");
    let mut original: Value = serde_json::from_slice(&fs::read(&original_path).unwrap()).unwrap();
    original["packages"][0]["rootUri"] = json!(format!(
        "{}/",
        crate::test_catalog::path_uri(&fixture.original).unwrap()
    ));
    original["packages"][1]["rootUri"] = json!("../../test dependency Ж/");
    fs::write(&original_path, serde_json::to_vec(&original).unwrap()).unwrap();
    // Snapshot config has already rebased the same dependency to an absolute
    // file URI. Graph whitespace is irrelevant, while its values are bound.
    put(
        &fixture.copy,
        ".dart_tool/package_graph.json",
        b"{\n  \"roots\": [\"subject\"]\n}\n",
    );
    let before = snapshot(&fixture.original);
    let proof = isolate_subject(
        &fixture.original,
        &fixture.copy,
        &fixture.original,
        "a",
        None,
        &Cancellation::default(),
    )
    .unwrap();
    assert_eq!(proof["alias"], "a");
    assert_eq!(proof["frameworkRoot"], fixture.original.to_str().unwrap());
    assert_eq!(snapshot(&fixture.original), before);
    let expected = String::from_utf8(before["test/main.dart"].clone())
        .unwrap()
        .replace("package:subject/", "package:a/");
    assert_eq!(
        fs::read(fixture.copy.join("test/main.dart")).unwrap(),
        expected.as_bytes()
    );
    assert_eq!(
        proof["importProof"][0]["restoredSha256"],
        crate::sha256_hex(&before["test/main.dart"])
    );
}

#[test]
fn subject_package_uri_must_identify_the_verified_library_tree() {
    for package_uri in ["shadow/", "./lib/"] {
        let fixture = Fixture::new();
        for root in [&fixture.original, &fixture.copy] {
            put(root, "shadow/api.dart", b"const value = 1000;");
            let path = root.join(".dart_tool/package_config.json");
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config["packages"][0]["packageUri"] = json!(package_uri);
            fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
        }
        let before = snapshot(&fixture.copy);
        let original = snapshot(&fixture.original);
        let result = fixture.run(None, &Cancellation::default());
        if package_uri == "shadow/" {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("namespace subject packageUri")
            );
            assert_eq!(snapshot(&fixture.copy), before);
        } else {
            result.unwrap();
        }
        assert_eq!(snapshot(&fixture.original), original);
    }
}

#[test]
fn lexical_budget_is_shared_across_files_before_instrumentation() {
    let fixture = Fixture::new();
    // Each source is below every individual scanner limit. Their combined
    // lexical work exceeds the campaign limit without allocating giant tokens.
    let many_comments = b"/*x*/".repeat(super::scanner::MAX_ITEMS / 2 + 1);
    fixture.set_test("test/a.dart", &many_comments);
    fixture.set_test("test/b.dart", &many_comments);
    let before = snapshot(&fixture.copy);
    let error = fixture
        .run(None, &Cancellation::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("aggregate lexical item budget exceeded"),
        "{error}"
    );
    assert_eq!(snapshot(&fixture.copy), before);
}

#[test]
fn complete_preflight_rejects_unplanned_assertion_and_library_self_references() {
    for library in [false, true] {
        let fixture = Fixture::new();
        if library {
            put(
                &fixture.original,
                "lib/api.dart",
                b"export 'package:subject/src/value.dart';",
            );
            put(
                &fixture.framework,
                "lib/api.dart",
                b"export 'package:subject/src/value.dart';",
            );
            put(
                &fixture.copy,
                "lib/api.dart",
                b"export 'package:subject/src/value.dart';",
            );
        } else {
            fixture.set_test(
                "test/z_bad.dart",
                b"void main(){expect('package:subject/a.dart','original');}",
            );
        }
        let before = snapshot(&fixture.copy);
        assert!(fixture.run(None, &Cancellation::default()).is_err());
        assert_eq!(snapshot(&fixture.copy), before);
    }
}

#[test]
fn runtime_plan_is_required_before_writes_and_exact_edits_are_bound_and_invertible() {
    let fixture = Fixture::new();
    let raw = "\u{feff}// 😀\r\nimport 'dart:isolate'; Uri? f()=>Isolate.resolvePackageUriSync(Uri.parse(r'package:subject/a.dart'));\r\n";
    fixture.set_test("test/runtime.dart", raw);
    let before = snapshot(&fixture.copy);
    assert!(matches!(
        fixture.run(None, &Cancellation::default()),
        Err(NamespaceError::PlanRequired)
    ));
    assert_eq!(snapshot(&fixture.copy), before);
    let offset = raw.find("package:subject/").unwrap();
    let edit = json!({"kind":"runtime_package_uri","offsetBytes":offset,"lengthBytes":"package:subject/".len(),"expected":"package:subject/","replacement":"package:failforge_subject/"});
    let plan = fixture.plan(BTreeMap::from([("test/runtime.dart".into(), vec![edit])]));
    let proof = fixture.run(Some(&plan), &Cancellation::default()).unwrap();
    let runtime = proof["importProof"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["file"] == "test/runtime.dart")
        .unwrap();
    assert_eq!(runtime["runtimePackageUris"], 1);
    assert_eq!(runtime["assertionsUnchanged"], false);
    assert_eq!(runtime["nonNamespaceBytesUnchanged"], true);
    assert_eq!(
        fs::read(fixture.copy.join("test/runtime.dart")).unwrap(),
        raw.replace("package:subject/", "package:failforge_subject/")
            .as_bytes()
    );
}

#[test]
fn forged_plan_schema_bindings_coverage_types_and_ranges_fail_without_writes() {
    let fixture = Fixture::new();
    let raw = b"void f(){resolvePackageUriSync(Uri.parse('package:subject/a.dart'));}";
    fixture.set_test("test/runtime.dart", raw);
    let offset = raw
        .windows(b"package:subject/".len())
        .position(|part| part == b"package:subject/")
        .unwrap();
    let edit = json!({"kind":"runtime_package_uri","offsetBytes":offset,"lengthBytes":16,"expected":"package:subject/","replacement":"package:failforge_subject/"});
    let valid = fixture.plan(BTreeMap::from([("test/runtime.dart".into(), vec![edit])]));
    let row = valid["files"]
        .as_array()
        .unwrap()
        .iter()
        .position(|v| v["file"] == "test/runtime.dart")
        .unwrap();
    let mut cases = vec![];
    for (key, value) in [
        ("version", json!(true)),
        ("version", json!(2)),
        ("alias", json!("other")),
        ("packageName", json!("other")),
        ("packageConfigHash", json!("wrong")),
        ("unsupported", json!([{}])),
        ("extra", json!(1)),
        ("files", json!([])),
    ] {
        let mut plan = valid.clone();
        plan[key] = value;
        cases.push(plan);
    }
    for (key, value) in [
        ("file", json!("../outside.dart")),
        ("sourceHash", json!("wrong")),
        ("extra", json!(true)),
    ] {
        let mut plan = valid.clone();
        plan["files"][row][key] = value;
        cases.push(plan);
    }
    for (key, value) in [
        ("offsetBytes", json!(true)),
        ("offsetBytes", json!(-1)),
        ("offsetBytes", json!(u64::MAX)),
        ("offsetBytes", json!(offset + 1)),
        ("lengthBytes", json!(17)),
        ("expected", json!("wrong")),
        ("replacement", json!("wrong")),
        ("kind", json!("header_package_uri")),
        ("extra", json!(0)),
    ] {
        let mut plan = valid.clone();
        plan["files"][row]["edits"][0][key] = value;
        cases.push(plan);
    }
    let mut repeated = valid.clone();
    repeated["files"][row]["edits"]
        .as_array_mut()
        .unwrap()
        .push(valid["files"][row]["edits"][0].clone());
    cases.push(repeated);
    let before = snapshot(&fixture.copy);
    for plan in cases {
        assert!(
            fixture.run(Some(&plan), &Cancellation::default()).is_err(),
            "{plan}"
        );
        assert_eq!(snapshot(&fixture.copy), before);
    }
}

#[test]
fn duplicate_or_overflowing_metadata_is_rejected_instead_of_lossily_rewritten() {
    for bytes in [
        br#"{"packages":[],"packages":[]}"#.as_slice(),
        br#"{"configVersion":2,"value":18446744073709551616,"packages":[]}"#,
    ] {
        let fixture = Fixture::new();
        put(&fixture.copy, ".dart_tool/package_config.json", bytes);
        let before = snapshot(&fixture.copy);
        assert!(fixture.run(None, &Cancellation::default()).is_err());
        assert_eq!(snapshot(&fixture.copy), before);
    }
}

#[test]
fn aliases_configuration_ambiguity_and_framework_drift_fail_before_writes() {
    for case in [
        "alias",
        "duplicate-own",
        "missing-test",
        "foreign-test",
        "missing-bin",
        "extra-resource",
        "changed-resource",
    ] {
        let fixture = Fixture::new();
        let config = fixture.copy.join(".dart_tool/package_config.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
        match case {
            "alias" => value["packages"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"failforge_subject"})),
            "duplicate-own" => {
                let own = value["packages"][0].clone();
                value["packages"].as_array_mut().unwrap().push(own);
            }
            "missing-test" => {
                value["packages"].as_array_mut().unwrap().pop();
            }
            "foreign-test" => {
                value["packages"][1]["rootUri"] = "https://invalid.example/test".into()
            }
            "missing-bin" => {
                fs::remove_file(fixture.dependency.join("bin/test.dart")).unwrap();
            }
            "extra-resource" => put(&fixture.framework, "lib/extra.bin", b"extra"),
            _ => put(&fixture.framework, "lib/assets/data.bin", b"changed"),
        }
        fs::write(config, serde_json::to_vec(&value).unwrap()).unwrap();
        let before = snapshot(&fixture.copy);
        assert!(
            fixture.run(None, &Cancellation::default()).is_err(),
            "{case}"
        );
        assert_eq!(snapshot(&fixture.copy), before);
    }
    let fixture = Fixture::new();
    for alias in ["subject", "test", "UpperCase", "../escape", "", "with-dash"] {
        assert!(
            isolate_subject(
                &fixture.original,
                &fixture.copy,
                &fixture.framework,
                alias,
                None,
                &Cancellation::default()
            )
            .is_err()
        );
    }
}

#[test]
fn overlapping_roots_and_copy_hardlinks_or_symlink_ancestors_are_rejected() {
    for name in [
        "lib/api.dart",
        "test/main.dart",
        ".dart_tool/package_config.json",
    ] {
        let fixture = Fixture::new();
        let original = snapshot(&fixture.original);
        fs::remove_file(fixture.copy.join(name)).unwrap();
        fs::hard_link(fixture.original.join(name), fixture.copy.join(name)).unwrap();
        assert!(fixture.run(None, &Cancellation::default()).is_err());
        assert_eq!(snapshot(&fixture.original), original);
    }
    let fixture = Fixture::new();
    let original = snapshot(&fixture.original);
    assert!(
        isolate_subject(
            &fixture.original,
            &fixture.original,
            &fixture.framework,
            "failforge_subject",
            None,
            &Cancellation::default()
        )
        .is_err()
    );
    let alias = fixture.base.join("alias");
    symlink(&fixture.original, &alias).unwrap();
    assert!(
        isolate_subject(
            &fixture.original,
            &alias,
            &fixture.framework,
            "failforge_subject",
            None,
            &Cancellation::default()
        )
        .is_err()
    );
    fs::remove_dir_all(fixture.copy.join("test")).unwrap();
    symlink(fixture.original.join("test"), fixture.copy.join("test")).unwrap();
    assert!(fixture.run(None, &Cancellation::default()).is_err());
    assert_eq!(snapshot(&fixture.original), original);
}

#[test]
fn replaced_target_and_added_hardlink_fail_closed_without_touching_original() {
    for hardlink in [false, true] {
        let fixture = Fixture::new();
        let original = snapshot(&fixture.original);
        let target = fixture.copy.join("test/main.dart");
        let outside = fixture.base.join("outside");
        let path = target.clone();
        let _hook = Hook::new(move |stage| {
            if stage == Stage::BeforeWrite(0) {
                if hardlink {
                    fs::hard_link(&path, &outside)?;
                } else {
                    fs::remove_file(&path)?;
                    fs::write(&path, b"foreign replacement")?;
                }
            }
            Ok(())
        });
        assert!(fixture.run(None, &Cancellation::default()).is_err());
        assert_eq!(snapshot(&fixture.original), original);
        if !hardlink {
            assert_eq!(fs::read(target).unwrap(), b"foreign replacement");
        }
    }
}

#[test]
fn write_failure_and_post_rename_metadata_failure_roll_back_all_committed_bytes() {
    for fail_at in [Stage::BeforeWrite(1), Stage::Renamed, Stage::AfterWrite(1)] {
        let fixture = Fixture::new();
        let before = snapshot(&fixture.copy);
        let mut failed = false;
        let _hook = Hook::new(move |stage| {
            if stage == fail_at && !failed {
                failed = true;
                return Err(invalid("injected write failure"));
            }
            Ok(())
        });
        let error = fixture.run(None, &Cancellation::default()).unwrap_err();
        assert!(
            error.to_string().contains("injected write failure"),
            "{error}"
        );
        assert_eq!(snapshot(&fixture.copy), before);
        assert!(
            !snapshot(&fixture.copy)
                .keys()
                .any(|name| name.contains(".failforge-namespace-"))
        );
    }
}

#[test]
fn replaced_temporary_is_neither_committed_nor_removed_by_cleanup() {
    for injected_failure in [false, true] {
        let fixture = Fixture::new();
        let before = snapshot(&fixture.copy);
        let directory = fixture.copy.join("test");
        let foreign_path = std::rc::Rc::new(std::cell::RefCell::new(None));
        let captured = foreign_path.clone();
        let _hook = Hook::new(move |stage| {
            if stage == Stage::TemporaryReady {
                let temporary = fs::read_dir(&directory)?
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".failforge-namespace-")
                    })
                    .unwrap()
                    .path();
                fs::remove_file(&temporary)?;
                fs::write(&temporary, b"foreign temporary replacement")?;
                *captured.borrow_mut() = Some(temporary);
                if injected_failure {
                    return Err(invalid("injected temporary failure"));
                }
            }
            Ok(())
        });
        let error = fixture
            .run(None, &Cancellation::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(if injected_failure {
                "injected temporary failure"
            } else if cfg!(windows) {
                "workspace directory entry changed"
            } else {
                "namespace temporary inode changed"
            }),
            "{error}"
        );
        let foreign = foreign_path.borrow().clone().unwrap();
        assert_eq!(
            fs::read(&foreign).unwrap(),
            b"foreign temporary replacement"
        );
        fs::remove_file(foreign).unwrap();
        assert_eq!(snapshot(&fixture.copy), before);
    }
}

#[test]
fn reused_temporary_name_after_rename_is_not_removed() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.copy);
    let directory = fixture.copy.join("test");
    let foreign_path = std::rc::Rc::new(std::cell::RefCell::new(None::<PathBuf>));
    let captured = foreign_path.clone();
    let mut inserted = false;
    let _hook = Hook::new(move |stage| {
        if inserted {
            return Ok(());
        }
        if stage == Stage::TemporaryReady {
            *captured.borrow_mut() = Some(
                fs::read_dir(&directory)?
                    .map(Result::unwrap)
                    .find(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".failforge-namespace-")
                    })
                    .unwrap()
                    .path(),
            );
        } else if stage == Stage::Renamed {
            fs::write(
                captured.borrow().as_ref().unwrap(),
                b"new occupant after rename",
            )?;
            inserted = true;
            return Err(invalid("injected post-rename failure"));
        }
        Ok(())
    });
    let error = fixture
        .run(None, &Cancellation::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("injected post-rename failure"), "{error}");
    let foreign = foreign_path.borrow().clone().unwrap();
    assert_eq!(fs::read(&foreign).unwrap(), b"new occupant after rename");
    fs::remove_file(foreign).unwrap();
    assert_eq!(snapshot(&fixture.copy), before);
}

#[test]
fn cancellation_before_and_during_commit_restores_the_complete_copy() {
    for stage in [None, Some(Stage::AfterWrite(0)), Some(Stage::AfterWrite(1))] {
        let fixture = Fixture::new();
        let before = snapshot(&fixture.copy);
        let cancel = Cancellation::default();
        let notify = cancel.clone();
        let _hook = Hook::new(move |current| {
            if Some(current) == stage {
                notify.request(libc::SIGTERM);
            }
            Ok(())
        });
        if stage.is_none() {
            cancel.request(libc::SIGTERM);
        }
        assert!(matches!(
            fixture.run(None, &cancel),
            Err(NamespaceError::Cancelled(libc::SIGTERM))
        ));
        assert_eq!(snapshot(&fixture.copy), before);
    }
}

#[test]
fn source_drift_before_or_after_commit_is_detected_and_copy_is_restored() {
    for at in [Stage::Preflight, Stage::AfterWrite(0)] {
        let fixture = Fixture::new();
        let before = snapshot(&fixture.copy);
        let original = fixture.original.join("lib/api.dart");
        let _hook = Hook::new(move |stage| {
            if stage == at {
                fs::write(&original, b"changed original")?;
            }
            Ok(())
        });
        assert!(fixture.run(None, &Cancellation::default()).is_err());
        assert_eq!(snapshot(&fixture.copy), before);
    }
}

#[test]
fn rollback_failure_is_reported_and_does_not_overwrite_a_replaced_inode() {
    let fixture = Fixture::new();
    let original = snapshot(&fixture.original);
    let target = fixture.copy.join("test/main.dart");
    let path = target.clone();
    let _hook = Hook::new(move |stage| {
        if stage == Stage::AfterWrite(0) {
            fs::remove_file(&path)?;
            fs::write(&path, b"foreign replacement")?;
            return Err(invalid("injected commit failure"));
        }
        Ok(())
    });
    let error = fixture
        .run(None, &Cancellation::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("namespace rollback failed"), "{error}");
    assert!(error.contains("injected commit failure"));
    assert_eq!(fs::read(target).unwrap(), b"foreign replacement");
    assert_eq!(snapshot(&fixture.original), original);
}
