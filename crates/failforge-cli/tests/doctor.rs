//! Doctor inspects prepared inputs without starting SDKs, helpers or tests.
#![cfg(unix)]

use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const MARKER_PROGRAM: &str =
    "#!/bin/sh\nprintf 'unexpected execution\\n' >> \"$FAILFORGE_DOCTOR_MARKER\"\nexit 99\n";

fn put(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn executable(path: impl AsRef<Path>) {
    let path = path.as_ref();
    put(path, MARKER_PROGRAM);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn prepare_sdk(root: &Path) {
    executable(root.join("bin/dart"));
    put(root.join("lib/core/core.dart"), "// prepared SDK fixture\n");
    put(root.join("version"), "3.13.5\n");
}

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff doctor {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let fixture = Self {
            root: root.canonicalize().unwrap(),
            child: None,
        };
        fs::create_dir_all(fixture.installation().join("bin")).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_failforge"), fixture.binary()).unwrap();
        executable(fixture.helper());
        prepare_sdk(&fixture.sdk());
        put(
            fixture.adapter().join("pubspec.yaml"),
            "name: failforge_dart_adapter\nenvironment:\n  sdk: '>=3.13.0 <3.14.0'\ndependencies:\n  analyzer: ^14.5.0\n",
        );
        put(
            fixture.adapter().join("bin/failforge_dart_adapter.dart"),
            "void main() { throw StateError('doctor must not run adapters'); }\n",
        );
        put(
            fixture.root.join("analyzer/lib/analyzer.dart"),
            "// prepared analyzer dependency\n",
        );
        put(
            fixture.adapter().join(".dart_tool/package_config.json"),
            json!({"configVersion":2,"packages":[
                {"name":"failforge_dart_adapter","rootUri":"../","packageUri":"lib/"},
                {"name":"analyzer","rootUri":"../../../../analyzer/","packageUri":"lib/"}
            ]})
            .to_string(),
        );
        put(fixture.adapter().join("lib/adapter.dart"), "// adapter\n");
        put(
            fixture.project().join("pubspec.yaml"),
            "name: subject\nenvironment:\n  sdk: '>=3.13.0 <3.14.0'\ndev_dependencies:\n  test: ^1.26.3\n",
        );
        put(fixture.project().join("pubspec.lock"), "packages: {}\n");
        put(
            fixture.project().join("lib/subject.dart"),
            "int add(int a, int b) => a + b;\n",
        );
        put(
            fixture.project().join("test/subject_test.dart"),
            "void main() { throw StateError('doctor must not run tests'); }\n",
        );
        put(
            fixture.root.join("test-dependency/bin/test.dart"),
            "void main() { throw StateError('doctor must not run test runners'); }\n",
        );
        put(
            fixture.root.join("test-dependency/lib/test.dart"),
            "// prepared test dependency\n",
        );
        fixture.prepare_project("test");
        fs::create_dir(fixture.root.join("temporary")).unwrap();
        fixture
    }

    fn installation(&self) -> PathBuf {
        self.root.join("installation")
    }

    fn binary(&self) -> PathBuf {
        self.installation().join("bin/failforge")
    }

    fn helper(&self) -> PathBuf {
        self.installation().join("bin/failforge-adapter-campaign")
    }

    fn sdk(&self) -> PathBuf {
        self.installation()
            .join(".toolchain/flutter/bin/cache/dart-sdk")
    }

    fn adapter(&self) -> PathBuf {
        self.installation().join("adapters/dart")
    }

    fn project(&self) -> PathBuf {
        self.root.join("subject")
    }

    fn prepare_project(&self, framework: &str) {
        let dependency = if framework == "test" {
            "test-dependency"
        } else {
            "flutter-test-dependency"
        };
        put(
            self.project().join(".dart_tool/package_config.json"),
            json!({"configVersion":2,"packages":[
                {"name":"subject","rootUri":"../","packageUri":"lib/"},
                {"name":framework,"rootUri":format!("../../{dependency}/"),"packageUri":"lib/"}
            ]})
            .to_string(),
        );
        put(
            self.project().join(".dart_tool/package_graph.json"),
            json!({"roots":["subject"],"packages":[
                {"name":"subject","dependencies":[framework]},
                {"name":framework,"dependencies":[]}
            ]})
            .to_string(),
        );
    }

    fn profile(&self, contents: &str) -> PathBuf {
        let path = self.project().join("profiles/check.yml");
        put(
            &path,
            format!(
                "mutations:\n  classic:\n    enabled: true\n    operators:\n      NUMERIC_LITERAL_REPLACEMENT: true\n{contents}"
            ),
        );
        path
    }

    fn snapshot(&self) -> BTreeMap<PathBuf, String> {
        fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, String>) {
            let relative = path.strip_prefix(root).unwrap();
            if matches!(relative.to_str(), Some("stdout" | "stderr")) {
                return;
            }
            let metadata = fs::symlink_metadata(path).unwrap();
            let kind = if metadata.file_type().is_symlink() {
                format!("symlink:{:?}", fs::read_link(path).unwrap())
            } else if metadata.is_dir() {
                "directory".to_owned()
            } else if metadata.is_file() {
                format!("file:{:x}", Sha256::digest(fs::read(path).unwrap()))
            } else {
                "special".to_owned()
            };
            result.insert(
                relative.to_path_buf(),
                format!("{kind}:{}", metadata.permissions().mode()),
            );
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    visit(root, &entry.unwrap().path(), result);
                }
            }
        }
        let mut result = BTreeMap::new();
        visit(&self.root, &self.root, &mut result);
        result
    }

    fn run(&mut self, arguments: &[OsString]) -> Output {
        self.run_with_path(arguments, OsStr::new("/nonexistent-path"))
    }

    fn run_with_path(&mut self, arguments: &[OsString], search_path: &OsStr) -> Output {
        let stdout = Stdio::from(File::create(self.root.join("stdout")).unwrap());
        self.run_with_stdout(arguments, search_path, stdout)
    }

    fn run_with_stdout(
        &mut self,
        arguments: &[OsString],
        search_path: &OsStr,
        stdout: Stdio,
    ) -> Output {
        assert!(self.child.is_none());
        File::create(self.root.join("stdout")).unwrap();
        let before = self.snapshot();
        self.child = Some(
            Command::new(self.binary())
                .arg("doctor")
                .args(arguments)
                .current_dir(self.project())
                .env("PATH", search_path)
                .env("TMPDIR", self.root.join("temporary"))
                .env("FAILFORGE_DOCTOR_MARKER", self.root.join("executed"))
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(File::create(self.root.join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "doctor exceeded test deadline");
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        assert_eq!(self.snapshot(), before, "doctor changed filesystem inputs");
        assert!(
            !self.root.join("executed").exists(),
            "doctor executed a tool"
        );
        Output {
            status,
            stdout: fs::read(self.root.join("stdout")).unwrap(),
            stderr: fs::read(self.root.join("stderr")).unwrap(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn status(output: &Output, expected: i32) -> String {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn check(text: &str, outcome: &str, label: &str) {
    assert!(text.contains(&format!("[{outcome}] {label}:")), "{text}");
}

#[test]
fn healthy_relocated_installation_uses_cwd_and_fallback_sdk_without_execution_or_writes() {
    let mut fixture = Fixture::new();
    let output = fixture.run(&[]);
    let text = status(&output, 0);
    assert!(output.stderr.is_empty());
    for label in [
        "Configuration",
        "Installation",
        "Native helper",
        "Dart SDK",
        "Dart SDK version",
        "Adapter entry point",
        "Adapter dependencies",
        "Project",
        "Test runner",
        "Tests",
        "Report destination",
    ] {
        check(&text, "OK", label);
    }
    check(&text, "SKIP", "Flutter SDK");
    assert!(text.contains("3.13.5"));
    assert!(text.contains(fixture.project().to_str().unwrap()), "{text}");
    assert!(text.contains(fixture.sdk().to_str().unwrap()), "{text}");
}

#[test]
fn path_sdk_overrides_installation_fallback_and_explicit_sdk_overrides_path() {
    let mut fixture = Fixture::new();
    let path_sdk = fixture.root.join("path-sdk");
    prepare_sdk(&path_sdk);
    put(fixture.sdk().join("version"), "3.12.0\n");
    let text = status(
        &fixture.run_with_path(&[], path_sdk.join("bin").as_os_str()),
        0,
    );
    check(&text, "OK", "Dart SDK version");
    assert!(text.contains(path_sdk.to_str().unwrap()), "{text}");
    put(path_sdk.join("version"), "3.14.0\n");
    put(fixture.sdk().join("version"), "3.13.5\n");
    let arguments = ["--dart-sdk".into(), fixture.sdk().into_os_string()];
    let text = status(
        &fixture.run_with_path(&arguments, path_sdk.join("bin").as_os_str()),
        0,
    );
    check(&text, "OK", "Dart SDK version");
    assert!(text.contains(fixture.sdk().to_str().unwrap()), "{text}");
}

#[test]
fn explicit_missing_sdk_does_not_silently_fall_back_to_prepared_sdk() {
    let mut fixture = Fixture::new();
    let missing = fixture.root.join("missing-sdk");
    let text = status(
        &fixture.run(&["--dart-sdk".into(), missing.clone().into_os_string()]),
        1,
    );
    check(&text, "FAIL", "Dart SDK");
    check(&text, "OK", "Adapter dependencies");
    check(&text, "OK", "Project");
    assert!(text.contains("--dart-sdk"));
    assert!(text.contains(missing.to_str().unwrap()), "{text}");
}

#[test]
fn independent_missing_prerequisites_are_aggregated_with_repairs() {
    let mut fixture = Fixture::new();
    fs::remove_file(fixture.sdk().join("bin/dart")).unwrap();
    fs::remove_file(fixture.adapter().join("bin/failforge_dart_adapter.dart")).unwrap();
    fs::remove_file(fixture.helper()).unwrap();
    let text = status(&fixture.run(&[]), 1);
    for label in ["Dart SDK", "Adapter entry point", "Native helper"] {
        check(&text, "FAIL", label);
    }
    check(&text, "OK", "Project");
    check(&text, "OK", "Tests");
    assert!(text.matches("Fix:").count() >= 3, "{text}");
}

#[test]
fn unsupported_or_malformed_sdk_versions_fail_without_running_version_command() {
    let mut fixture = Fixture::new();
    for version in [
        "3.12.9\n",
        "3.14.0\n",
        "3.13.5-dev.1\n",
        "not-a-version\n",
        "",
    ] {
        put(fixture.sdk().join("version"), version);
        let text = status(&fixture.run(&[]), 1);
        check(&text, "FAIL", "Dart SDK version");
        check(&text, "OK", "Project");
    }
}

#[test]
fn missing_and_malformed_target_package_metadata_are_prerequisite_failures() {
    let mut fixture = Fixture::new();
    let config = fixture.project().join(".dart_tool/package_config.json");
    fs::remove_file(&config).unwrap();
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Project");
    check(&text, "OK", "Dart SDK version");
    assert!(text.contains("pub get"), "{text}");
    put(&config, "{broken");
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Project");
    assert!(text.contains("pub get"), "{text}");
}

#[test]
fn missing_test_entry_and_missing_author_tests_are_reported_separately() {
    let mut fixture = Fixture::new();
    fs::remove_file(fixture.root.join("test-dependency/bin/test.dart")).unwrap();
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Test runner");
    check(&text, "OK", "Tests");
    put(
        fixture.root.join("test-dependency/bin/test.dart"),
        "void main() {}\n",
    );
    fs::remove_file(fixture.project().join("test/subject_test.dart")).unwrap();
    let text = status(&fixture.run(&[]), 1);
    check(&text, "OK", "Test runner");
    check(&text, "FAIL", "Tests");
}

#[test]
fn yaml_paths_resolve_from_profile_and_report_destination_is_never_created() {
    let mut fixture = Fixture::new();
    let profile = fixture.profile(
        "dart_sdk: ../../installation/.toolchain/flutter/bin/cache/dart-sdk\nadapter_root: ../../installation/adapters/dart\noutput: ../../future-report\nrules:\n  include: [lib/subject.dart]\n",
    );
    // The profile is nested two directories below the fixture root.
    let text = status(
        &fixture.run(&["--config".into(), profile.into_os_string()]),
        0,
    );
    check(&text, "OK", "Configuration");
    check(&text, "OK", "Report destination");
    assert!(!fixture.root.join("future-report").exists());
    assert!(!fixture.project().join(".failforge").exists());
}

#[test]
fn malformed_yaml_and_nonexistent_literal_includes_have_configuration_exit_code() {
    let mut fixture = Fixture::new();
    let profile = fixture.profile("workers: invalid\n");
    let output = fixture.run(&["--config".into(), profile.clone().into_os_string()]);
    let text = status(&output, 2);
    check(&text, "FAIL", "Configuration");
    assert!(
        text.contains("workers") || String::from_utf8_lossy(&output.stderr).contains("workers"),
        "stdout: {text}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.profile("rules:\n  include: [lib/missing.dart]\n");
    let output = fixture.run(&["--config".into(), profile.into_os_string()]);
    status(&output, 2);
}

#[test]
fn every_active_profile_family_is_checked_for_target_compatibility() {
    let mut fixture = Fixture::new();
    let profile = fixture.profile(
        "  failure_mode:\n    enabled: true\n    operators:\n      METHOD_CHANNEL_ERROR: true\n",
    );
    let text = status(
        &fixture.run(&["--config".into(), profile.into_os_string()]),
        2,
    );
    check(&text, "FAIL", "Configuration");
    assert!(text.contains("METHOD_CHANNEL_ERROR"), "{text}");
}

#[test]
fn automatic_report_destination_rejects_symlink_escape_without_writing_outside_project() {
    let mut fixture = Fixture::new();
    let outside = fixture.root.join("outside-project");
    put(outside.join("sentinel"), "preserve external files\n");
    fs::create_dir(outside.join("runs")).unwrap();
    symlink(&outside, fixture.project().join(".failforge")).unwrap();
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Report destination");
    fs::remove_dir(outside.join("runs")).unwrap();
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Report destination");
    assert!(!outside.join("runs").exists());
}

#[test]
fn yaml_conflicts_with_direct_options_but_allows_installation_override() {
    let mut fixture = Fixture::new();
    let profile = fixture.profile("");
    for option in ["--project", "--dart-sdk", "--flutter-sdk", "--adapter-root"] {
        let output = fixture.run(&[
            "--config".into(),
            profile.clone().into_os_string(),
            option.into(),
            fixture.installation().into_os_string(),
        ]);
        status(&output, 2);
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
    }
    let output = fixture.run(&[
        "--config".into(),
        profile.into_os_string(),
        "--installation".into(),
        fixture.installation().into_os_string(),
    ]);
    status(&output, 0);
}

#[test]
fn adapter_missing_declared_dependency_does_not_pass_as_prepared() {
    let mut fixture = Fixture::new();
    put(
        fixture.adapter().join(".dart_tool/package_config.json"),
        r#"{"configVersion":2,"packages":[]}"#,
    );
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Adapter dependencies");
    check(&text, "OK", "Project");
    assert!(text.contains("pub get"), "{text}");
}

#[test]
fn source_symlinks_and_pub_workspaces_are_rejected_before_any_tests() {
    let mut fixture = Fixture::new();
    symlink("subject.dart", fixture.project().join("lib/alias.dart")).unwrap();
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Project");
    fs::remove_file(fixture.project().join("lib/alias.dart")).unwrap();
    put(
        fixture.project().join("pubspec.yaml"),
        "name: subject\nworkspace: [member]\n",
    );
    put(
        fixture.project().join(".dart_tool/package_graph.json"),
        json!({"roots":["subject","member"],"packages":[
            {"name":"subject","dependencies":["test"]},
            {"name":"member","dependencies":[]},
            {"name":"test","dependencies":[]}
        ]})
        .to_string(),
    );
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Project");
}

#[test]
fn flutter_project_requires_flutter_tools_even_when_dart_sdk_is_prepared() {
    let mut fixture = Fixture::new();
    put(
        fixture
            .root
            .join("flutter-test-dependency/lib/flutter_test.dart"),
        "// flutter test\n",
    );
    fixture.prepare_project("flutter_test");
    let text = status(&fixture.run(&[]), 1);
    check(&text, "FAIL", "Flutter SDK");
    check(&text, "OK", "Project");
    let flutter = fixture.root.join("flutter");
    prepare_sdk(&flutter.join("bin/cache/dart-sdk"));
    executable(flutter.join("bin/flutter"));
    let text = status(
        &fixture.run(&["--flutter-sdk".into(), flutter.into_os_string()]),
        1,
    );
    check(&text, "FAIL", "Flutter SDK");
}

#[test]
fn prepared_flutter_project_passes_static_checks_without_running_flutter() {
    let mut fixture = Fixture::new();
    put(
        fixture
            .root
            .join("flutter-test-dependency/lib/flutter_test.dart"),
        "// flutter test\n",
    );
    fixture.prepare_project("flutter_test");
    let flutter = fixture.root.join("flutter");
    let dart = flutter.join("bin/cache/dart-sdk");
    prepare_sdk(&dart);
    executable(flutter.join("bin/flutter"));
    put(
        flutter.join("packages/flutter/pubspec.yaml"),
        "name: flutter\n",
    );
    put(
        flutter.join("packages/flutter/lib/src/services/platform_channel.dart"),
        "// prepared Flutter platform channel API\n",
    );
    let host = if cfg!(target_os = "macos") {
        "darwin-x64"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "aarch64") {
        "linux-arm64"
    } else {
        "linux-x64"
    };
    let engine = flutter.join("bin/cache/artifacts/engine");
    executable(engine.join(host).join("flutter_tester"));
    for path in [
        flutter.join("bin/cache/flutter_tools.snapshot"),
        engine.join(host).join("icudtl.dat"),
        engine.join("common/flutter_patched_sdk/platform_strong.dill"),
        dart.join("bin/snapshots/frontend_server_aot.dart.snapshot"),
    ] {
        put(path, "prepared runtime artifact\n");
    }
    let text = status(
        &fixture.run(&["--flutter-sdk".into(), flutter.into_os_string()]),
        0,
    );
    check(&text, "OK", "Flutter SDK");
    check(&text, "SKIP", "Test runner");
    check(&text, "OK", "Tests");
}

#[test]
fn custom_test_discovery_is_explicitly_skipped_instead_of_claiming_missing_tests() {
    let mut fixture = Fixture::new();
    fs::remove_file(fixture.project().join("test/subject_test.dart")).unwrap();
    put(
        fixture.project().join("checks/subject.dart"),
        "void main() {}\n",
    );
    put(
        fixture.project().join("dart_test.yaml"),
        "paths: [checks]\nfilename: '*.dart'\n",
    );
    let text = status(&fixture.run(&[]), 0);
    check(&text, "SKIP", "Tests");
    check(&text, "OK", "Test runner");
}

fn fifo(path: &Path) {
    fs::remove_file(path).unwrap();
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: path is a live NUL-terminated fixture path; no descriptor is owned.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

#[test]
fn fifo_metadata_is_rejected_without_blocking_or_opening_tools() {
    for target in ["version", "adapter", "project", "profile"] {
        let mut fixture = Fixture::new();
        let arguments = match target {
            "version" => {
                fifo(&fixture.sdk().join("version"));
                vec![]
            }
            "adapter" => {
                fifo(&fixture.adapter().join(".dart_tool/package_config.json"));
                vec![]
            }
            "project" => {
                fifo(&fixture.project().join(".dart_tool/package_graph.json"));
                vec![]
            }
            "profile" => {
                let profile = fixture.profile("");
                fifo(&profile);
                vec!["--config".into(), profile.into_os_string()]
            }
            _ => unreachable!(),
        };
        let output = fixture.run(&arguments);
        status(&output, if target == "profile" { 2 } else { 1 });
    }
}

#[test]
fn closed_stdout_is_delivery_failure_instead_of_success_or_panic() {
    let mut fixture = Fixture::new();
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors provides storage for the two newly allocated FDs.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: pipe succeeded, and these Files each adopt one fresh FD once.
    let (reader, writer) = unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    };
    drop(reader);
    let output = fixture.run_with_stdout(&[], OsStr::new("/nonexistent-path"), Stdio::from(writer));
    status(&output, 125);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
}
