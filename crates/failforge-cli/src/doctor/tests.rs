use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    config: CampaignConfig,
    helper: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "failforge-doctor-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let context = Context::new(&root, &root).unwrap();
        let config = failforge_config::from_mapping(&json!({"project":"project","family":"classic","dart_sdk":"sdk","adapter_root":"adapter"}), &context, None).unwrap();
        let fixture = Self {
            helper: root.join("bin/failforge-adapter-campaign"),
            root,
            config,
        };
        fixture.put(
            "sdk/bin/dart",
            b"this is deliberately not a runnable program",
        );
        fixture.make_executable("sdk/bin/dart");
        fixture.put("sdk/version", b"3.13.5\n");
        fixture.put("sdk/lib/core/core.dart", b"library dart.core;");
        fixture.put(
            "bin/failforge-adapter-campaign",
            b"this must not be executed",
        );
        fixture.make_executable("bin/failforge-adapter-campaign");
        fixture.put(
            "adapter/bin/failforge_dart_adapter.dart",
            b"void main() {}\n",
        );
        fixture.put("adapter/pubspec.yaml", b"name: failforge_dart_adapter\ndependencies:\n  analyzer: any\ndev_dependencies:\n  test: any\n");
        fixture.put("analyzer/lib/analyzer.dart", b"library analyzer;");
        fixture.put("test/bin/test.dart", b"void main() {}\n");
        fixture.package_config("adapter", &["failforge_dart_adapter", "analyzer", "test"]);
        fixture.put("project/pubspec.yaml", b"name: subject\n");
        fixture.put("project/lib/source.dart", b"bool f() => true;\n");
        fixture.put("project/test/source_test.dart", b"void main() {}\n");
        fixture.package_config("project", &["subject", "test"]);
        fixture.project_graph(false);
        fixture
    }

    fn put(&self, relative: &str, bytes: &[u8]) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn make_executable(&self, relative: &str) {
        fs::set_permissions(self.root.join(relative), fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn package_config(&self, owner: &str, names: &[&str]) {
        let rows = names.iter().map(|name| json!({"name":name,"rootUri":format!("../../{}", match *name { "subject" => "project", "failforge_dart_adapter" => "adapter", name => name })})).collect::<Vec<_>>();
        self.put(
            &format!("{owner}/.dart_tool/package_config.json"),
            &serde_json::to_vec(&json!({"configVersion":2,"packages":rows})).unwrap(),
        );
    }

    fn project_graph(&self, flutter: bool) {
        let framework = if flutter { "flutter_test" } else { "test" };
        self.put("project/.dart_tool/package_graph.json", &serde_json::to_vec(&json!({"roots":["subject"],"packages":[{"name":"subject","dependencies":[framework]},{"name":framework,"dependencies":[]}]})).unwrap());
    }

    fn inspect(&self) -> Report {
        inspect(&self.config, &self.root, &self.helper, &[&self.config])
    }

    fn status(&self, label: &str) -> Status {
        self.inspect()
            .0
            .into_iter()
            .find(|check| check.label == label)
            .unwrap()
            .status
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn complete_dart_prerequisites_do_not_execute_programs_or_write_files() {
    let fixture = Fixture::new();
    let before = snapshot(&fixture.root);
    let report = fixture.inspect();
    assert_eq!(report.status(), 0, "{report:?}");
    assert_eq!(fixture.status("Flutter SDK"), Status::Skip);
    assert_eq!(snapshot(&fixture.root), before);
    assert!(!fixture.root.join("project/.failforge").exists());
    let mut output = Vec::new();
    report.write(&mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("[OK] Configuration"));
    assert!(output.contains("SDK programs and tests were not run"));
    assert!(output.contains("No files were changed"));
}

fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
    let mut result = vec![];
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        result.push((
            path.strip_prefix(root).unwrap().to_owned(),
            if metadata.is_file() {
                fs::read(&path).unwrap()
            } else {
                vec![]
            },
            metadata.permissions().mode(),
        ));
        if metadata.is_dir() {
            pending.extend(
                fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
    }
    result.sort();
    result
}

#[test]
fn stable_sdk_version_range_accepts_later_dart_3_releases_and_is_bounded() {
    for version in ["3.13.0", "3.13.5\n", "3.13.99999", "3.14.0", "3.99.1"] {
        assert!(supported_version(version.as_bytes()).is_some(), "{version}");
    }
    for version in [
        "",
        "3.12.99",
        "4.0.0",
        "4.13.0",
        "3.13",
        "3.13.1.2",
        "3.13.-1",
        "3.13.0-beta.1",
        "3.14.0-dev.1",
        "3.13.0 (stable)",
        "v3.13.1",
        "٣.13.1",
        "03.13.1",
        "3.013.1",
        "3.13.01",
        "3.13.999999999999999999999999999999",
    ] {
        assert!(supported_version(version.as_bytes()).is_none(), "{version}");
    }
    assert!(supported_version(&[0xff]).is_none());
    let fixture = Fixture::new();
    fixture.put("sdk/version", &vec![b'1'; 4097]);
    assert_eq!(fixture.status("Dart SDK version"), Status::Fail);
}

#[test]
fn later_sdk_minors_warn_without_failing_or_executing_programs() {
    let fixture = Fixture::new();
    for version in ["3.14.0\n", "3.99.1\n"] {
        fixture.put("sdk/version", version.as_bytes());
        let before = snapshot(&fixture.root);
        let report = fixture.inspect();
        assert_eq!(report.status(), 0, "{report:?}");
        assert_eq!(fixture.status("Dart SDK version"), Status::Ok);
        assert_eq!(fixture.status("Dart SDK compatibility"), Status::Warn);
        assert_eq!(snapshot(&fixture.root), before);
        let mut output = Vec::new();
        report.write(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("[WARN] Dart SDK compatibility"), "{output}");
        assert!(output.contains("3.13.5"), "{output}");
    }
    fixture.put("sdk/version", b"3.13.5\n");
    assert!(
        fixture
            .inspect()
            .0
            .iter()
            .all(|check| check.status != Status::Warn)
    );
}

#[test]
fn independent_failures_are_aggregated_and_project_dependents_skip() {
    let fixture = Fixture::new();
    for path in [
        "sdk/bin/dart",
        "sdk/version",
        "adapter/bin/failforge_dart_adapter.dart",
        "adapter/.dart_tool/package_config.json",
        "project/.dart_tool/package_graph.json",
        "bin/failforge-adapter-campaign",
    ] {
        fs::remove_file(fixture.root.join(path)).unwrap();
    }
    let report = fixture.inspect();
    assert_eq!(report.status(), 1);
    for label in [
        "Dart SDK",
        "Dart SDK version",
        "Adapter entry point",
        "Adapter dependencies",
        "Project",
        "Native helper",
    ] {
        assert_eq!(fixture.status(label), Status::Fail, "{label}");
    }
    for label in ["Test runner", "Flutter SDK", "Tests"] {
        assert_eq!(fixture.status(label), Status::Skip, "{label}");
    }
    assert_eq!(fixture.status("Report destination"), Status::Ok);
}

#[test]
fn sdk_and_helper_require_executable_access_not_just_file_existence() {
    let fixture = Fixture::new();
    fs::set_permissions(
        fixture.root.join("sdk/bin/dart"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::set_permissions(&fixture.helper, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(fixture.status("Dart SDK"), Status::Fail);
    assert_eq!(fixture.status("Native helper"), Status::Fail);
    assert_eq!(fixture.status("Dart SDK version"), Status::Ok);
}

#[test]
fn adapter_metadata_requires_self_identity_and_all_declared_dependency_roots() {
    let fixture = Fixture::new();
    for names in [
        &[][..],
        &["failforge_dart_adapter"][..],
        &["analyzer", "test"][..],
        &["failforge_dart_adapter", "analyzer"][..],
    ] {
        fixture.package_config("adapter", names);
        assert_eq!(
            fixture.status("Adapter dependencies"),
            Status::Fail,
            "{names:?}"
        );
    }
    fixture.package_config("adapter", &["failforge_dart_adapter", "analyzer", "test"]);
    assert_eq!(fixture.status("Adapter dependencies"), Status::Ok);
    fs::remove_dir_all(fixture.root.join("analyzer")).unwrap();
    assert_eq!(fixture.status("Adapter dependencies"), Status::Fail);
}

#[test]
fn malformed_duplicate_and_aliased_adapter_metadata_are_rejected() {
    let fixture = Fixture::new();
    for bytes in [
        b"{}".as_slice(),
        b"{bad",
        b"{\"configVersion\":2,\"packages\":[],\"packages\":[]}",
    ] {
        fixture.put("adapter/.dart_tool/package_config.json", bytes);
        assert_eq!(fixture.status("Adapter dependencies"), Status::Fail);
    }
    fixture.package_config("adapter", &["failforge_dart_adapter", "analyzer", "test"]);
    let metadata = fixture.root.join("adapter/.dart_tool/package_config.json");
    let target = fixture.root.join("adapter/config.json");
    fs::rename(&metadata, &target).unwrap();
    symlink(&target, &metadata).unwrap();
    assert_eq!(fixture.status("Adapter dependencies"), Status::Fail);
}

#[test]
fn project_inspection_rejects_source_aliases_and_stale_package_identity() {
    let fixture = Fixture::new();
    let source = fixture.root.join("project/lib/source.dart");
    let alias_target = fixture.root.join("outside.dart");
    fs::rename(&source, &alias_target).unwrap();
    symlink(&alias_target, &source).unwrap();
    assert_eq!(fixture.status("Project"), Status::Fail);
    fs::remove_file(&source).unwrap();
    fs::rename(&alias_target, &source).unwrap();
    fixture.package_config("project", &["test"]);
    assert_eq!(fixture.status("Project"), Status::Fail);
}

#[test]
fn test_runner_and_custom_test_discovery_have_distinct_results() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root.join("test/bin/test.dart")).unwrap();
    assert_eq!(fixture.status("Test runner"), Status::Fail);
    fixture.put("test/bin/test.dart", b"void main() {}");
    fs::remove_file(fixture.root.join("project/test/source_test.dart")).unwrap();
    assert_eq!(fixture.status("Tests"), Status::Fail);
    fixture.put(
        "project/dart_test.yaml",
        b"paths: [spec]\nfilename: '*_spec.dart'\n",
    );
    assert_eq!(fixture.status("Tests"), Status::Skip);
    assert_eq!(fixture.inspect().status(), 0);
    fixture.put("project/test/nested/another_test.dart", b"void main() {}");
    assert_eq!(fixture.status("Tests"), Status::Ok);
}

#[test]
fn report_destination_rejects_existing_and_protected_directories_without_creating_any() {
    let mut fixture = Fixture::new();
    for path in [
        fixture.root.join("sdk/output"),
        fixture.root.join("adapter/output"),
        fixture.root.join("test/output"),
        fixture.root.join("project/lib/output"),
        fixture.root.join("project"),
        fixture.root.clone(),
    ] {
        fixture.config.output = Some(path);
        assert_eq!(fixture.status("Report destination"), Status::Fail);
    }
    fixture.config.output = Some(fixture.root.join("project/.failforge/new-report"));
    assert_eq!(fixture.status("Report destination"), Status::Ok);
    assert!(!fixture.root.join("project/.failforge").exists());
    fs::create_dir_all(fixture.config.output.as_ref().unwrap()).unwrap();
    assert_eq!(fixture.status("Report destination"), Status::Fail);
}

#[test]
fn readonly_report_parent_is_detected_without_creating_probe_files() {
    // Root can write despite ordinary Unix mode restrictions.
    // SAFETY: geteuid has no arguments or side effects.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let mut fixture = Fixture::new();
    let parent = fixture.root.join("readonly");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o555)).unwrap();
    fixture.config.output = Some(parent.join("report"));
    assert_eq!(fixture.status("Report destination"), Status::Fail);
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn flutter_sdk_inference_and_prepared_host_artifacts_are_checked_without_running_it() {
    let mut fixture = Fixture::new();
    let sdk = fixture.root.join("flutter/bin/cache/dart-sdk");
    fs::create_dir_all(sdk.parent().unwrap()).unwrap();
    fs::rename(fixture.root.join("sdk"), &sdk).unwrap();
    fixture.config.dart_sdk = sdk;
    fixture.put("flutter/bin/flutter", b"do not run this");
    fixture.make_executable("flutter/bin/flutter");
    let host = if cfg!(target_os = "macos") {
        "darwin-x64"
    } else if cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    };
    let tester = format!("flutter/bin/cache/artifacts/engine/{host}/flutter_tester");
    let files = [
        "flutter/bin/cache/flutter_tools.snapshot".to_owned(),
        format!("flutter/bin/cache/artifacts/engine/{host}/icudtl.dat"),
        "flutter/bin/cache/artifacts/engine/common/flutter_patched_sdk/platform_strong.dill".into(),
        "flutter/bin/cache/dart-sdk/bin/snapshots/frontend_server_aot.dart.snapshot".into(),
        "flutter/packages/flutter/pubspec.yaml".into(),
        "flutter/packages/flutter/lib/src/services/platform_channel.dart".into(),
        tester.clone(),
    ];
    for path in &files {
        fixture.put(path, b"prepared artifact");
    }
    fixture.make_executable(&tester);
    fixture.put("flutter_test/pubspec.yaml", b"name: flutter_test\n");
    fixture.package_config("project", &["subject", "flutter_test"]);
    fixture.project_graph(true);
    assert_eq!(
        flutter_sdk(&fixture.config),
        Some(fixture.root.join("flutter"))
    );
    assert_eq!(fixture.inspect().status(), 0);
    assert_eq!(fixture.status("Test runner"), Status::Skip);
    for path in &files {
        let bytes = fs::read(fixture.root.join(path)).unwrap();
        fs::remove_file(fixture.root.join(path)).unwrap();
        assert_eq!(fixture.status("Flutter SDK"), Status::Fail, "{path}");
        fixture.put(path, &bytes);
        if path == &tester {
            fixture.make_executable(path);
        }
    }
    fixture.config.output = Some(fixture.root.join("flutter/new-report"));
    assert_eq!(fixture.status("Report destination"), Status::Fail);
    fixture.config.output = None;
    fixture.config.dart_sdk = fixture.root.join("other-sdk");
    fs::create_dir(&fixture.config.dart_sdk).unwrap();
    fixture.config.flutter_sdk = Some(fixture.root.join("flutter"));
    assert_eq!(fixture.status("Flutter SDK"), Status::Fail);
}

#[test]
fn default_report_destination_rejects_intermediate_alias_even_when_target_exists() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("outside/runs")).unwrap();
    symlink(
        fixture.root.join("outside"),
        fixture.root.join("project/.failforge"),
    )
    .unwrap();
    let before = snapshot(&fixture.root);
    assert_eq!(fixture.status("Report destination"), Status::Fail);
    assert_eq!(snapshot(&fixture.root), before);
}

#[test]
fn report_ancestor_needs_directory_read_access_as_well_as_write_search() {
    // SAFETY: geteuid has no arguments or side effects.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let mut fixture = Fixture::new();
    let parent = fixture.root.join("write-search-only");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).unwrap();
    fixture.config.output = Some(parent.join("nested/report"));
    assert_eq!(fixture.status("Report destination"), Status::Fail);
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn framework_incompatibility_is_checked_in_every_selected_family() {
    let fixture = Fixture::new();
    let mut incompatible = fixture.config.clone();
    incompatible.family = "failure_mode".into();
    incompatible.rules.operators = Some(vec!["METHOD_CHANNEL_ERROR".into()]);
    let report = inspect(
        &fixture.config,
        &fixture.root,
        &fixture.helper,
        &[&fixture.config, &incompatible],
    );
    assert_eq!(report.status(), 2);
    assert!(
        report
            .0
            .iter()
            .any(|check| check.label == "Configuration" && check.status == Status::Fail)
    );
    assert!(
        report
            .0
            .iter()
            .any(|check| check.label == "Project" && check.status == Status::Ok)
    );
}

#[test]
fn diagnostics_quote_metacharacters_and_escape_terminal_controls() {
    for (value, expected) in [
        ("a b", "'a b'"),
        ("a'b", "'a'\"'\"'b'"),
        ("$(touch x);echo", "'$(touch x);echo'"),
        ("", "''"),
        ("a\nb", "$'a\\012b'"),
        ("a\u{1b}[31m", "$'a\\033[31m'"),
    ] {
        assert_eq!(quote(Path::new(value)), expected);
    }
}

#[test]
fn suggested_command_paths_round_trip_in_bash_without_expanding_shell_syntax() {
    let fixture = Fixture::new();
    let marker = fixture.root.join("shell-expanded");
    for input in [
        "a'b",
        "a\nb",
        "a\t\\'b",
        "safe\u{202e}name",
        "path with spaces Ж",
    ] {
        let quoted = quote(Path::new(input));
        let output = std::process::Command::new("/bin/bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                &format!("printf '%s' {quoted}"),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, input.as_bytes());
    }
    let attack = format!("$(touch {}); echo injected", marker.display());
    let output = std::process::Command::new("/bin/bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            &format!("printf '%s' {}", quote(Path::new(&attack))),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, attack.as_bytes());
    assert!(!marker.exists());
}

#[test]
fn rendering_propagates_output_failures() {
    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let fixture = Fixture::new();
    assert!(fixture.inspect().write(&mut Failing).is_err());
}
