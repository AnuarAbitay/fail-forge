use super::*;
use std::collections::BTreeSet;
use std::os::unix::fs::{PermissionsExt, symlink};

fn put(root: &Path, relative: &str, text: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn tracked() -> Vec<String> {
    [
        "adapters/dart/bin/failforge_dart_adapter.dart",
        "adapters/dart/bin/failforge_dart_namespace.dart",
        "adapters/dart/lib/engine.dart",
        "adapters/dart/test/engine_test.dart",
        "adapters/dart/.dart_tool/package_config.json",
        "adapters/dart/lib/protocol/schema.json",
        "adapters/dart/lib/.dart_tool/secret.dart",
        "adapters/dart/lib/.git/secret.dart",
        "adapters/dart/lib/target/secret.dart",
        "adapters/dart/lib/nested/.hidden.dart",
        "adapters/dart/lib/build/secret.dart",
        "adapters/dart/lib/reports/secret.dart",
        "adapters/dart/lib/tests/secret.dart",
        "target/debug/fixture",
        "docs/private.md",
        "docs/doctor.md",
        ".env",
        "AGENTS.md",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([format!("third-party/licenses/{}.txt", "a".repeat(64))])
    .collect()
}

#[test]
fn payload_allowlist_excludes_tests_tool_state_secrets_and_untracked_sources() {
    let root = Temporary::new("ff-package-allowlist").unwrap();
    for path in tracked() {
        put(&root.0, &path, "fixture");
    }
    for path in [
        "adapters/dart/lib/untracked.dart",
        "adapters/dart/lib/.env",
        "third-party/licenses/secret.txt",
        "docs/secret.md",
    ] {
        put(&root.0, path, "SECRET");
    }
    let selected = assets::select(&tracked()).unwrap();
    assert!(selected.contains("adapters/dart/lib/engine.dart"));
    assert!(selected.contains("third-party/inventory.json"));
    assert!(selected.contains("docs/doctor.md"));
    for path in [
        "adapters/dart/lib/untracked.dart",
        "adapters/dart/lib/.env",
        "third-party/licenses/secret.txt",
        "docs/secret.md",
        "adapters/dart/test/engine_test.dart",
        "adapters/dart/.dart_tool/package_config.json",
        "adapters/dart/lib/protocol/schema.json",
        "adapters/dart/lib/.dart_tool/secret.dart",
        "adapters/dart/lib/.git/secret.dart",
        "adapters/dart/lib/target/secret.dart",
        "adapters/dart/lib/nested/.hidden.dart",
        "adapters/dart/lib/build/secret.dart",
        "adapters/dart/lib/reports/secret.dart",
        "adapters/dart/lib/tests/secret.dart",
        "target/debug/fixture",
        ".env",
        "AGENTS.md",
        "docs/architecture.md",
        "docs/known-issues.md",
    ] {
        assert!(!selected.contains(path), "unexpected shipped file {path}");
    }
    assert!(assets::select(&[]).is_err());
}

#[test]
fn tracked_paths_reject_traversal_alias_syntax_controls_and_non_unicode() {
    assert_eq!(
        tracked_paths(b"lib/a.dart\0file with spaces.md\0")
            .unwrap()
            .len(),
        2
    );
    for path in [
        "../escape",
        "/absolute",
        "a/../b",
        "a/./b",
        "a//b",
        "a\\b",
        "a\nb",
        "a\tb",
        "a\u{7f}b",
        "a\u{1b}b",
    ] {
        assert!(!assets::safe_relative(path), "{path:?}");
        assert!(tracked_paths(format!("{path}\0").as_bytes()).is_err());
    }
    assert!(tracked_paths(b"\xff\0").is_err());
    assert!(tracked_paths(b"").is_err());
}

#[test]
fn copied_files_have_exact_bytes_hashes_and_only_intended_permissions() {
    let source = Temporary::new("ff-package-copy").unwrap();
    let destination = Temporary::new("ff-package-destination").unwrap();
    put(&source.0, "source", b"bytes\0\xff\n");
    fs::set_permissions(source.0.join("source"), fs::Permissions::from_mode(0o777)).unwrap();
    for executable in [false, true] {
        let path = if executable {
            "bin/program"
        } else {
            "lib/source"
        };
        let row = assets::copy(&source.0, "source", &destination.0, path, executable).unwrap();
        assert_eq!(
            fs::read(destination.0.join(path)).unwrap(),
            b"bytes\0\xff\n"
        );
        assert_eq!(
            row["sha256"],
            common::hash(&source.0.join("source")).unwrap()
        );
        assert_eq!(row["bytes"], 8);
        assert_eq!(
            fs::metadata(destination.0.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            if executable { 0o755 } else { 0o644 }
        );
        assert!(assets::copy(&source.0, "source", &destination.0, path, executable).is_err());
    }
}

#[test]
fn payload_rejects_file_links_directory_links_and_special_files() {
    let root = Temporary::new("ff-package-alias").unwrap();
    put(&root.0, "real/file", "unchanged");
    symlink(root.0.join("real/file"), root.0.join("alias")).unwrap();
    symlink(root.0.join("real"), root.0.join("directory-alias")).unwrap();
    assert!(assets::regular(&root.0, "alias").is_err());
    assert!(assets::regular(&root.0, "directory-alias/file").is_err());
    let fifo = CString::new(root.0.join("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(assets::regular(&root.0, "fifo").is_err());
    assert!(assets::regular(&root.0, "real").is_err());
    assert_eq!(
        fs::read_to_string(root.0.join("real/file")).unwrap(),
        "unchanged"
    );
}

#[test]
fn snapshots_detect_same_length_changes_and_use_stable_path_order() {
    let root = Temporary::new("ff-package-snapshot").unwrap();
    put(&root.0, "a", "one");
    put(&root.0, "b", "two");
    let before = assets::snapshot(&root.0, ["a", "b"]).unwrap();
    assert_eq!(
        assets::tree_digest(&before).unwrap(),
        assets::tree_digest(&assets::snapshot(&root.0, ["b", "a"]).unwrap()).unwrap()
    );
    put(&root.0, "a", "ONE");
    assert_ne!(before, assets::snapshot(&root.0, ["a", "b"]).unwrap());
}

fn license_fixture(root: &Path) -> BTreeSet<String> {
    for path in [
        "Cargo.lock",
        "adapters/dart/pubspec.yaml",
        "adapters/dart/pubspec.lock",
    ] {
        put(root, path, path);
    }
    let input: Vec<_> = [
        "Cargo.lock",
        "adapters/dart/pubspec.yaml",
        "adapters/dart/pubspec.lock",
    ]
    .into_iter()
    .map(|path| json!({"path":path,"sha256":assets::digest(root,path).unwrap()}))
    .collect();
    put(root, "THIRD_PARTY_NOTICES.md", "notices\n");
    put(root, "third-party/license-source", "license\n");
    let hash = assets::digest(root, "third-party/license-source").unwrap();
    let path = format!("third-party/licenses/{hash}.txt");
    put(root, &path, "license\n");
    common::write_json(&root.join("third-party/inventory.json"), &json!({"inputs":input,"packages":[{"notices":[{"file":format!("licenses/{hash}.txt"),"sha256":hash}]}]})).unwrap();
    let selected = BTreeSet::from([
        "THIRD_PARTY_NOTICES.md".to_owned(),
        "third-party/inventory.json".to_owned(),
        path,
    ]);
    let sums = selected
        .iter()
        .map(|p| format!("{}  {p}\n", assets::digest(root, p).unwrap()))
        .collect::<String>();
    put(root, "third-party/SHA256SUMS", sums);
    selected
}

#[test]
fn notice_inventory_binds_all_lockfiles_and_every_retained_notice() {
    let root = Temporary::new("ff-package-notices").unwrap();
    let selected = license_fixture(&root.0);
    assets::validate_notices(&root.0, &selected).unwrap();
    put(&root.0, "Cargo.lock", "drift");
    assert!(
        assets::validate_notices(&root.0, &selected)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    let selected = license_fixture(&root.0);
    let notice = selected
        .iter()
        .find(|p| p.starts_with("third-party/licenses/"))
        .unwrap();
    put(&root.0, notice, "changed");
    assert!(assets::validate_notices(&root.0, &selected).is_err());
}

#[test]
fn notice_checksums_cannot_be_missing_duplicated_or_redirected() {
    let root = Temporary::new("ff-package-notice-guards").unwrap();
    for bad in ["", "duplicate", "../../outside"] {
        let selected = license_fixture(&root.0);
        let file = root.0.join("third-party/SHA256SUMS");
        let text = fs::read_to_string(&file).unwrap();
        fs::write(
            &file,
            match bad {
                "" => String::new(),
                "duplicate" => format!("{text}{text}"),
                _ => format!("{}  {bad}\n", "a".repeat(64)),
            },
        )
        .unwrap();
        assert!(
            assets::validate_notices(&root.0, &selected).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn notice_filename_cannot_disagree_with_its_verified_content() {
    let root = Temporary::new("ff-package-notice-name").unwrap();
    let mut selected = license_fixture(&root.0);
    let original = selected
        .iter()
        .find(|path| path.starts_with("third-party/licenses/"))
        .unwrap()
        .clone();
    let wrong = format!("third-party/licenses/{}.txt", "a".repeat(64));
    fs::rename(root.0.join(&original), root.0.join(&wrong)).unwrap();
    selected.remove(&original);
    selected.insert(wrong.clone());
    let sums = selected
        .iter()
        .map(|path| format!("{}  {path}\n", assets::digest(&root.0, path).unwrap()))
        .collect::<String>();
    put(&root.0, "third-party/SHA256SUMS", sums);
    assert!(
        assets::validate_notices(&root.0, &selected)
            .unwrap_err()
            .to_string()
            .contains("filename")
    );
}

#[test]
fn runtime_notices_are_complete_exact_and_reference_only_relative_sysroot_paths() {
    let root = Temporary::new("ff-package-runtime").unwrap();
    for (relative, _) in assets::RUNTIME {
        put(&root.0, relative, format!("notice for {relative}\n"));
    }
    let destination = Temporary::new("ff-package-runtime-copy").unwrap();
    let (files, notices) = assets::runtime(&root.0, &destination.0).unwrap();
    assert_eq!(files.len(), 8);
    assert_eq!(notices.len(), 8);
    for notice in notices {
        let source = notice["sourcePath"].as_str().unwrap();
        assert!(!Path::new(source).is_absolute());
        assert_eq!(notice["sha256"], assets::digest(&root.0, source).unwrap());
    }
    fs::remove_file(root.0.join(assets::RUNTIME[7].0)).unwrap();
    let incomplete = Temporary::new("ff-package-runtime-missing").unwrap();
    assert!(
        assets::runtime(&root.0, &incomplete.0)
            .unwrap_err()
            .to_string()
            .contains("rust-src")
    );
}

#[test]
fn target_and_version_cannot_mislabel_an_archive() {
    for (target, os, arch) in [
        ("aarch64-apple-darwin", "macos", "aarch64"),
        ("x86_64-apple-darwin", "macos", "x86_64"),
        ("aarch64-unknown-linux-gnu", "linux", "aarch64"),
        ("x86_64-unknown-linux-musl", "linux", "x86_64"),
    ] {
        assert_eq!(
            host(&format!("rustc 1.99\nhost: {target}\n"), os, arch).unwrap(),
            target
        );
    }
    for (text, os, arch) in [
        ("host: aarch64-apple-darwin\n", "linux", "aarch64"),
        ("host: x86_64-unknown-linux-gnu\n", "linux", "aarch64"),
        (
            "host: aarch64-apple-darwin\nhost: aarch64-apple-darwin\n",
            "macos",
            "aarch64",
        ),
        ("host: ../evil\n", "macos", "aarch64"),
    ] {
        assert!(host(text, os, arch).is_err());
    }
    assert_eq!(
        version(b"failforge 0.1.0-beta.1\n", Some("0.1.0-beta.1")).unwrap(),
        "0.1.0-beta.1"
    );
    assert!(version(b"failforge 0.1.0\n", Some("0.1.0-beta.1")).is_err());
    for bad in [
        "failforge ../../escape",
        "other 0.1.0",
        "failforge 0.1.0\nextra",
        "failforge --flag",
    ] {
        assert!(version(bad.as_bytes(), None).is_err());
    }
}

#[test]
fn executable_headers_reject_scripts_truncation_and_wrong_architecture() {
    let root = Temporary::new("ff-package-binary").unwrap();
    let path = root.0.join("program");
    let mut elf = [0; 32];
    elf[..6].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1]);
    elf[18..20].copy_from_slice(&62_u16.to_le_bytes());
    fs::write(&path, elf).unwrap();
    binary_format(&path, "x86_64-unknown-linux-gnu").unwrap();
    assert!(binary_format(&path, "aarch64-unknown-linux-gnu").is_err());
    let mut macho = [0; 32];
    macho[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    macho[4..8].copy_from_slice(&0x0100000c_u32.to_le_bytes());
    fs::write(&path, macho).unwrap();
    binary_format(&path, "aarch64-apple-darwin").unwrap();
    assert!(binary_format(&path, "x86_64-apple-darwin").is_err());
    for bad in [b"#!/bin/sh\nexit 0\n".as_slice(), b"short", &[0; 32]] {
        fs::write(&path, bad).unwrap();
        assert!(binary_format(&path, "aarch64-apple-darwin").is_err());
    }
}

fn artifact(name: &str, path: &Path) -> Value {
    json!({"reason":"compiler-artifact","target":{"name":name},"profile":{"opt_level":"3","test":false,"debug_assertions":false,"overflow_checks":false},"executable":path})
}

#[test]
fn cargo_artifacts_require_both_optimized_owned_product_binaries() {
    let root = Temporary::new("ff-package-artifacts").unwrap();
    for name in BINARIES {
        put(&root.0, name, "binary");
    }
    let rows: Vec<_> = BINARIES
        .iter()
        .map(|name| artifact(name, &root.0.join(name)))
        .collect();
    let encode = |rows: &[Value]| rows.iter().map(|r| format!("{r}\n")).collect::<String>();
    assert_eq!(
        artifacts(encode(&rows).as_bytes(), &root.0).unwrap().len(),
        2
    );
    assert!(artifacts(encode(&rows[..1]).as_bytes(), &root.0).is_err());
    let mut wrong = rows.clone();
    wrong[0]["profile"]["opt_level"] = "0".into();
    assert!(artifacts(encode(&wrong).as_bytes(), &root.0).is_err());
    for flag in ["debug_assertions", "overflow_checks", "test"] {
        let mut override_profile = rows.clone();
        override_profile[0]["profile"][flag] = true.into();
        assert!(
            artifacts(encode(&override_profile).as_bytes(), &root.0).is_err(),
            "{flag}"
        );
    }
    let mut duplicate = rows.clone();
    duplicate.push(rows[0].clone());
    assert!(artifacts(encode(&duplicate).as_bytes(), &root.0).is_err());
    let outside = Temporary::new("ff-package-outside").unwrap();
    put(&outside.0, "failforge", "binary");
    wrong[0] = artifact("failforge", &outside.0.join("failforge"));
    assert!(artifacts(encode(&wrong).as_bytes(), &root.0).is_err());
    fs::remove_file(root.0.join("failforge")).unwrap();
    symlink(outside.0.join("failforge"), root.0.join("failforge")).unwrap();
    assert!(artifacts(encode(&rows).as_bytes(), &root.0).is_err());
}

#[test]
fn output_and_atomic_publication_preserve_existing_files_and_directories() {
    let root = Temporary::new("ff-package-publication").unwrap();
    let destination = root.0.join("output");
    assert_eq!(fresh_output(&destination).unwrap(), destination);
    fs::create_dir(&destination).unwrap();
    put(&destination, "keep", "original");
    assert!(fresh_output(&destination).is_err());
    let stage = root.0.join("stage");
    fs::create_dir(&stage).unwrap();
    put(&stage, "archive", "new");
    assert!(publish(&stage, &destination).is_err());
    assert_eq!(
        fs::read_to_string(destination.join("keep")).unwrap(),
        "original"
    );
    assert!(stage.join("archive").is_file());
    let link = root.0.join("link");
    symlink(&destination, &link).unwrap();
    assert!(fresh_output(&link).is_err());
    let output = root.0.join("new-output");
    publish(&stage, &output).unwrap();
    assert!(!stage.exists());
    assert!(output.join("archive").is_file());
}

#[test]
fn failed_packaging_preflight_leaves_existing_output_untouched() {
    let root = Temporary::new("ff-package-preflight").unwrap();
    put(&root.0, "output/keep", "original");
    let options = Options {
        installation: Some(root.0.clone()),
        output: root.0.join("output"),
        expected_version: None,
    };
    assert!(
        run(options)
            .unwrap_err()
            .to_string()
            .contains("must not already exist")
    );
    assert_eq!(
        fs::read_to_string(root.0.join("output/keep")).unwrap(),
        "original"
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
}
