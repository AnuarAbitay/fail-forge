//! Exercise native analysis dispatch and round-trip catalogue templates through
//! the production loader. Dispatch tests need no PyYAML; template integration
//! cases explicitly skip when it has not yet been installed (as in early CI).
#![cfg(unix)]

use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
static HAS_YAML: OnceLock<bool> = OnceLock::new();

fn installation() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn yaml_available() -> bool {
    let available = *HAS_YAML.get_or_init(|| {
        Command::new("python3")
            .args(["-E", "-c", "import yaml"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    });
    if !available {
        eprintln!("skipping YAML integration case: python3 PyYAML is not installed");
    }
    available
}

struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff profile integration {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("package/lib")).unwrap();
        fs::write(root.join("package/pubspec.yaml"), "name: fixture\n").unwrap();
        fs::write(
            root.join("package/lib/calculator.dart"),
            "int add(int a, int b) => a + b;\n",
        )
        .unwrap();
        Self { root, child: None }
    }

    fn probe_installation(&self) -> PathBuf {
        let installed = self.root.join("installed tree");
        let entry = installed.join("python/failforge_campaign/profile_generate.py");
        fs::create_dir_all(entry.parent().unwrap()).unwrap();
        fs::write(&entry, "import os, sys\nprint(os.getpid(), flush=True)\nprint(sys.flags.ignore_environment, flush=True)\nfor arg in sys.argv[1:]:\n print(os.fsencode(arg).hex(), flush=True)\nsys.exit(29)\n").unwrap();
        installed
    }

    fn source(&self) -> PathBuf {
        self.root.join("package/lib/calculator.dart")
    }

    fn profile(&self) -> PathBuf {
        self.root.join("package/profiles/calculator.yml")
    }

    fn args(&self) -> Vec<OsString> {
        vec![
            "--file".into(),
            self.source().into_os_string(),
            "--family".into(),
            "classic".into(),
            "--output".into(),
            self.profile().into_os_string(),
            "--template".into(),
        ]
    }

    fn spawn(&mut self, arguments: &[OsString], installed: &Path) -> u32 {
        self.spawn_in(arguments, installed, &self.root.clone())
    }

    fn spawn_in(&mut self, arguments: &[OsString], installed: &Path, cwd: &Path) -> u32 {
        assert!(self.child.is_none());
        let stdout = File::create(self.root.join("stdout")).unwrap();
        let stderr = File::create(self.root.join("stderr")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_failforge"))
            .args(["profile", "generate"])
            .args(arguments)
            .arg("--installation")
            .arg(installed)
            .env("PYTHONPATH", self.root.join("untrusted python path"))
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.child = Some(child);
        pid
    }

    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "profile generation exceeded independent test deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        let read = |name| {
            let mut bytes = Vec::new();
            File::open(self.root.join(name))
                .unwrap()
                .take(128 * 1024)
                .read_to_end(&mut bytes)
                .unwrap();
            bytes
        };
        Output {
            status,
            stdout: read("stdout"),
            stderr: read("stderr"),
        }
    }

    fn run(&mut self, arguments: &[OsString]) -> Output {
        self.spawn(arguments, &installation());
        self.finish()
    }

    fn assert_profile(&self, path: &Path, assertions: &str) {
        let code = format!(
            "import sys\nfrom pathlib import Path\nimport yaml\nsys.path.insert(0, str(Path(sys.argv[1]) / 'python'))\nfrom failforge_campaign.config import load_yaml\npath = Path(sys.argv[2])\nraw = yaml.safe_load(path.read_text())\nprofile = load_yaml(path, cwd=path.parent, installation=Path(sys.argv[1]))\nassert profile.project == Path(sys.argv[3]).resolve(), profile.project\nassert 'version' not in raw and 'project' not in raw\n{assertions}\n"
        );
        let output = Command::new("python3")
            .args(["-E", "-c", &code])
            .arg(installation())
            .arg(path)
            .arg(self.root.join("package"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_success(&output);
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_rejected(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

fn hex(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn analysis_dispatch_forwards_toolchain_and_runner_replaces_pid_and_ignores_python_environment() {
    let mut fixture = Fixture::new();
    let installed = fixture.probe_installation();
    let literal = format!("$(touch {})", fixture.root.join("must-not-exist").display());
    let arguments: Vec<OsString> = vec![
        "--file".into(),
        literal.clone().into(),
        "--family".into(),
        "classic".into(),
        "--family".into(),
        "language_specific".into(),
        "--language".into(),
        "dart".into(),
        "--operator".into(),
        "BOOLEAN_LITERAL_FLIP".into(),
        "--operator".into(),
        "NSM-001".into(),
        "--output".into(),
        "profile with spaces.yml".into(),
        "--dart-sdk".into(),
        "Dart SDK with spaces".into(),
        "--adapter-root".into(),
        "adapter with spaces".into(),
        "--force".into(),
        "--dry-run".into(),
    ];
    let pid = fixture.spawn(&arguments, &installed);
    let output = fixture.finish();
    assert_eq!(output.status.code(), Some(29));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines[0], pid.to_string());
    assert_eq!(lines[1], "1");
    let mut expected: Vec<_> = arguments
        .iter()
        .map(|value| hex(value.as_bytes()))
        .collect();
    expected.push(hex("--installation"));
    expected.push(hex(installed
        .canonicalize()
        .unwrap()
        .as_os_str()
        .as_bytes()));
    expected.push(hex("--runner"));
    expected.push(hex(fs::canonicalize(env!("CARGO_BIN_EXE_failforge"))
        .unwrap()
        .as_os_str()
        .as_bytes()));
    assert_eq!(lines[2..], expected);
    assert!(!fixture.root.join("must-not-exist").exists());
}

#[test]
fn generate_missing_interpreter_has_redacted_launch_diagnostic() {
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    arguments.extend(["--python".into(), "missing-python-secret-marker".into()]);
    let installed = fixture.probe_installation();
    fixture.spawn(&arguments, &installed);
    let output = fixture.finish();
    assert_eq!(output.status.code(), Some(125));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot execute"), "{stderr}");
    assert!(!stderr.contains("secret-marker"));
}

#[test]
fn generate_requires_file_family_and_output_before_interpreter_launch() {
    let mut fixture = Fixture::new();
    for arguments in [
        vec!["--file", "secret.dart", "--family", "classic"],
        vec!["--file", "secret.dart", "--output", "secret.yml"],
        vec!["--family", "classic", "--output", "secret.yml"],
        vec![
            "--file",
            "secret.dart",
            "--family",
            "unknown",
            "--output",
            "secret.yml",
        ],
        vec![
            "--file",
            "secret.dart",
            "--family",
            "language_specific",
            "--output",
            "secret.yml",
        ],
        vec![
            "--file",
            "secret.dart",
            "--family",
            "classic",
            "--language",
            "dart",
            "--output",
            "secret.yml",
        ],
    ] {
        let mut arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
        arguments.extend(["--python".into(), "must-not-execute-interpreter".into()]);
        let output = fixture.run(&arguments);
        assert_rejected(&output);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret"));
    }
}

#[test]
fn classic_template_round_trips_with_every_catalogue_switch_and_runtime_defaults() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let source = fs::read(fixture.source()).unwrap();
    let output = fixture.run(&fixture.args());
    assert_success(&output);
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("mutate"));
    assert!(stdout.contains("--config"));
    assert!(stdout.contains("calculator.yml"));
    fixture.assert_profile(
        &fixture.profile(),
        r#"assert set(raw['mutations']) == {'classic'}
assert raw['mutations']['classic']['enabled'] is True
switches = raw['mutations']['classic']['operators']
assert len(switches) == 32 and sum(switches.values()) == 31
assert switches['CONDITIONAL_NEGATION'] is False
assert raw['workers'] == 1
assert raw['timeouts'] == {'test': '30s', 'command': '120s'}
assert raw['limits']['max_mutants'] == 1000
assert raw['rules']['include'] == ['lib/calculator.dart']
assert len(profile.campaigns) == 1
assert len(profile.campaigns[0].rules.operators) == 31"#,
    );
    assert_eq!(fs::read(fixture.source()).unwrap(), source);
    assert!(!fixture.root.join("package/.dart_tool").exists());
}

#[test]
fn selected_families_and_operators_produce_exact_independent_switches() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    arguments.extend([
        "--family".into(),
        "language_specific".into(),
        "--family".into(),
        "failure_mode".into(),
        "--language".into(),
        "dart".into(),
        "--operator".into(),
        "BOOLEAN_LITERAL_FLIP".into(),
        "--operator".into(),
        "NSM-001".into(),
    ]);
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(
        &fixture.profile(),
        r#"assert set(raw['mutations']) == {'classic', 'failure_mode', 'language_specific'}
assert raw['mutations']['failure_mode']['enabled'] is True
assert not any(raw['mutations']['failure_mode']['operators'].values())
assert raw['mutations']['language_specific']['language'] == 'dart'
assert len(raw['mutations']['language_specific']['operators']) == 19
for family, expected in [('classic', 'BOOLEAN_LITERAL_FLIP'), ('language_specific', 'NSM-001')]:
 switches = raw['mutations'][family]['operators']
 assert [key for key, enabled in switches.items() if enabled] == [expected]
assert [campaign.family for campaign in profile.campaigns] == ['classic', 'language_specific']"#,
    );
}

#[test]
fn dart_semantic_template_defaults_enable_all_nineteen_operators() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    arguments[3] = "language_specific".into();
    arguments.extend(["--language".into(), "dart".into()]);
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(
        &fixture.profile(),
        r#"assert set(raw['mutations']) == {'language_specific'}
switches = raw['mutations']['language_specific']['operators']
assert len(switches) == 19 and all(switches.values())
assert len(profile.campaigns[0].rules.operators) == 19"#,
    );
}

#[test]
fn failure_mode_template_defaults_follow_dart_or_flutter_package_kind() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    arguments[3] = "failure_mode".into();
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(&fixture.profile(), r#"assert set(raw['mutations']) == {'failure_mode'}
assert raw['mutations']['failure_mode']['operators'] == {'EXCEPTION_AT_ENTRY': True, 'METHOD_CHANNEL_ERROR': False}"#);
    fs::write(
        fixture.root.join("package/pubspec.yaml"),
        "name: fixture\ndependencies:\n  flutter:\n    sdk: flutter\n",
    )
    .unwrap();
    arguments.push("--force".into());
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(&fixture.profile(), r#"assert raw['mutations']['failure_mode']['operators'] == {'EXCEPTION_AT_ENTRY': True, 'METHOD_CHANNEL_ERROR': False}"#);
    fs::write(
        fixture.root.join("package/pubspec.yaml"),
        "name: fixture\ndev_dependencies:\n  flutter_test:\n    sdk: flutter\n",
    )
    .unwrap();
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(&fixture.profile(), r#"assert raw['mutations']['failure_mode']['operators'] == {'EXCEPTION_AT_ENTRY': True, 'METHOD_CHANNEL_ERROR': True}"#);
}

#[test]
fn language_and_operator_selection_errors_leave_no_profile_directories() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let base = fixture.args();
    let mut missing_language = base.clone();
    missing_language[3] = "language_specific".into();
    let mut orphan_language = base.clone();
    orphan_language.extend(["--language".into(), "dart".into()]);
    let mut wrong_family = base.clone();
    wrong_family.extend(["--operator".into(), "NSM-001".into()]);
    let mut unknown = base.clone();
    unknown.extend(["--operator".into(), "UNKNOWN_OPERATOR".into()]);
    for arguments in [missing_language, orphan_language, wrong_family, unknown] {
        assert_rejected(&fixture.run(&arguments));
        assert!(!fixture.root.join("package/profiles").exists());
    }
}

#[test]
fn dry_run_is_only_loadable_yaml_and_creates_no_output_directory() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    arguments.push("--dry-run".into());
    let output = fixture.run(&arguments);
    assert_success(&output);
    assert!(output.stderr.is_empty());
    assert!(!fixture.root.join("package/profiles").exists());
    let preview = fixture.root.join("package/preview.yml");
    fs::write(&preview, &output.stdout).unwrap();
    fixture.assert_profile(
        &preview,
        r#"assert set(raw['mutations']) == {'classic'}
assert raw['rules']['include'] == ['lib/calculator.dart']"#,
    );
}

#[test]
fn existing_profile_requires_force_and_force_replaces_a_regular_profile() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    fs::create_dir_all(fixture.profile().parent().unwrap()).unwrap();
    fs::write(fixture.profile(), "preserve existing profile\n").unwrap();
    let mut arguments = fixture.args();
    assert_rejected(&fixture.run(&arguments));
    assert_eq!(
        fs::read_to_string(fixture.profile()).unwrap(),
        "preserve existing profile\n"
    );
    arguments.push("--force".into());
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(
        &fixture.profile(),
        "assert raw['mutations']['classic']['enabled'] is True",
    );
}

#[test]
fn force_never_follows_output_symlinks_or_replaces_directories_or_fifos() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    fs::create_dir_all(fixture.profile().parent().unwrap()).unwrap();
    let original = fixture.root.join("protected data");
    fs::write(&original, "preserve me").unwrap();
    let mut arguments = fixture.args();
    arguments.push("--force".into());
    symlink(&original, fixture.profile()).unwrap();
    assert_rejected(&fixture.run(&arguments));
    assert!(
        fs::symlink_metadata(fixture.profile())
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&original).unwrap(), "preserve me");
    fs::remove_file(fixture.profile()).unwrap();
    symlink(fixture.root.join("absent target"), fixture.profile()).unwrap();
    assert_rejected(&fixture.run(&arguments));
    assert!(
        fs::symlink_metadata(fixture.profile())
            .unwrap()
            .is_symlink()
    );
    fs::remove_file(fixture.profile()).unwrap();
    fs::create_dir(fixture.profile()).unwrap();
    assert_rejected(&fixture.run(&arguments));
    assert!(fixture.profile().is_dir());
    fs::remove_dir(fixture.profile()).unwrap();
    let fifo = CString::new(fixture.profile().as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert_rejected(&fixture.run(&arguments));
    assert!(fs::symlink_metadata(fixture.profile()).is_ok());
}

#[test]
fn output_must_share_the_sources_nearest_pubspec_package() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("package/nested")).unwrap();
    fs::write(
        fixture.root.join("package/nested/pubspec.yaml"),
        "name: nested\n",
    )
    .unwrap();
    let mut arguments = fixture.args();
    arguments[5] = fixture
        .root
        .join("package/nested/profiles/profile.yml")
        .into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!fixture.root.join("package/nested/profiles").exists());
    arguments[5] = fixture.root.join("outside/profile.yml").into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!fixture.root.join("outside").exists());
}

#[test]
fn source_and_output_paths_with_spaces_unicode_and_quotes_round_trip() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let source = fixture.root.join("package/lib/есеп 'quoted'.dart");
    fs::rename(fixture.source(), &source).unwrap();
    let profile = fixture.root.join("package/профили 'quoted'/есеп.yml");
    let mut arguments = fixture.args();
    arguments[1] = source.into_os_string();
    arguments[5] = profile.clone().into_os_string();
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(
        &profile,
        r#"assert raw['rules']['include'] == ["lib/есеп 'quoted'.dart"]
assert profile.campaigns[0].rules.include == ("lib/есеп 'quoted'.dart",)"#,
    );
}

#[test]
fn unsupported_file_inputs_and_output_suffixes_are_rejected_without_side_effects() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    for name in [
        "missing.dart",
        "source.txt",
        "file*.dart",
        "file?.dart",
        "file[1].dart",
        "file{1}.dart",
        "file\\name.dart",
    ] {
        let path = fixture.root.join("package/lib").join(name);
        if name != "missing.dart" {
            fs::write(&path, "void main() {}\n").unwrap();
        }
        let mut arguments = fixture.args();
        arguments[1] = path.into_os_string();
        assert_rejected(&fixture.run(&arguments));
        assert!(!fixture.root.join("package/profiles").exists());
    }
    for suffix in ["json", "YAML", "txt"] {
        let mut arguments = fixture.args();
        arguments[5] = fixture
            .root
            .join(format!("package/profiles/profile.{suffix}"))
            .into_os_string();
        assert_rejected(&fixture.run(&arguments));
        assert!(!fixture.root.join("package/profiles").exists());
    }
}

#[test]
fn force_cannot_overwrite_pubspec_or_write_through_an_output_parent_symlink() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let pubspec = fixture.root.join("package/pubspec.yaml");
    let original = fs::read(&pubspec).unwrap();
    let mut arguments = fixture.args();
    arguments[5] = pubspec.clone().into_os_string();
    arguments.push("--force".into());
    assert_rejected(&fixture.run(&arguments));
    assert_eq!(fs::read(pubspec).unwrap(), original);

    let destination = fixture.root.join("redirected");
    fs::create_dir(&destination).unwrap();
    symlink(&destination, fixture.root.join("package/profiles")).unwrap();
    arguments[5] = fixture.profile().into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!destination.join("calculator.yml").exists());
    assert!(
        fs::symlink_metadata(fixture.root.join("package/profiles"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn relative_paths_resolve_from_invocation_directory_and_yaml_suffix_is_supported() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    arguments[1] = "package/lib/calculator.dart".into();
    arguments[5] = "package/profiles/relative.yaml".into();
    assert_success(&fixture.run(&arguments));
    fixture.assert_profile(
        &fixture.root.join("package/profiles/relative.yaml"),
        "assert raw['rules']['include'] == ['lib/calculator.dart']",
    );
}

#[test]
fn nonregular_source_inputs_are_rejected_without_blocking_or_output_creation() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let directory = fixture.root.join("package/lib/directory.dart");
    fs::create_dir(&directory).unwrap();
    let mut arguments = fixture.args();
    arguments[1] = directory.into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!fixture.root.join("package/profiles").exists());

    let fifo = fixture.root.join("package/lib/pipe.dart");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    arguments[1] = fifo.into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!fixture.root.join("package/profiles").exists());
}

#[test]
fn source_symlinks_and_internal_parent_aliases_are_rejected() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let mut arguments = fixture.args();
    let source_link = fixture.root.join("package/lib/alias.dart");
    symlink(fixture.source(), &source_link).unwrap();
    arguments[1] = source_link.into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!fixture.root.join("package/profiles").exists());

    let directory_link = fixture.root.join("package/linked-lib");
    symlink(fixture.root.join("package/lib"), &directory_link).unwrap();
    arguments[1] = directory_link.join("calculator.dart").into_os_string();
    assert_rejected(&fixture.run(&arguments));
    assert!(!fixture.root.join("package/profiles").exists());
}

#[test]
fn concurrent_generators_without_force_publish_exactly_one_complete_profile() {
    if !yaml_available() {
        return;
    }
    let mut first = Fixture::new();
    let mut second = Fixture::new();
    let arguments = first.args();
    first.spawn(&arguments, &installation());
    second.spawn(&arguments, &installation());
    let first_output = first.finish();
    let second_output = second.finish();
    let mut codes = [first_output.status.code(), second_output.status.code()];
    codes.sort();
    assert_eq!(
        codes,
        [Some(0), Some(2)],
        "first stderr: {}\nsecond stderr: {}",
        String::from_utf8_lossy(&first_output.stderr),
        String::from_utf8_lossy(&second_output.stderr)
    );
    first.assert_profile(
        &first.profile(),
        "assert raw['mutations']['classic']['enabled'] is True",
    );
    let entries: Vec<_> = fs::read_dir(first.profile().parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [OsString::from("calculator.yml")]);
}

#[test]
fn leading_dash_source_and_output_values_survive_native_to_python_dispatch() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    let library = fixture.root.join("package/lib");
    fs::write(library.join("-calc.dart"), "bool enabled() => true;\n").unwrap();
    let arguments: Vec<OsString> = vec![
        "--file=-calc.dart".into(),
        "--family".into(),
        "classic".into(),
        "--output=-profile.yml".into(),
        "--template".into(),
    ];
    fixture.spawn_in(&arguments, &installation(), &library);
    assert_success(&fixture.finish());
    fixture.assert_profile(
        &library.join("-profile.yml"),
        "assert raw['rules']['include'] == ['lib/-calc.dart']\nassert profile.campaigns[0].rules.include == ('lib/-calc.dart',)",
    );
}

#[test]
fn source_outside_library_is_rejected_without_publishing_an_inert_profile() {
    if !yaml_available() {
        return;
    }
    let mut fixture = Fixture::new();
    for relative in [
        "source.dart",
        "bin/source.dart",
        "test/source.dart",
        "example/source.dart",
    ] {
        let source = fixture.root.join("package").join(relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "bool selected() => true;\n").unwrap();
        let mut arguments = fixture.args();
        arguments[1] = source.clone().into_os_string();
        let result = fixture.run(&arguments);
        assert_rejected(&result);
        assert!(String::from_utf8_lossy(&result.stderr).contains("lib/"));
        assert_eq!(
            fs::read_to_string(source).unwrap(),
            "bool selected() => true;\n"
        );
        assert!(!fixture.profile().parent().unwrap().exists());
    }
}

struct RestoreDirectoryReadPermission(PathBuf);

impl Drop for RestoreDirectoryReadPermission {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
    }
}

#[test]
fn searchable_external_parent_needs_no_directory_listing_to_generate_profile() {
    if !yaml_available() || unsafe { libc::geteuid() } == 0 {
        return;
    }
    let mut fixture = Fixture::new();
    let parent = fixture.root.join("search-only");
    fs::create_dir(&parent).unwrap();
    let project = parent.join("package");
    fs::rename(fixture.root.join("package"), &project).unwrap();
    let source = project.join("lib/calculator.dart");
    let profile = project.join("profiles/generated.yml");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o111)).unwrap();
    let _restore = RestoreDirectoryReadPermission(parent.clone());
    assert_eq!(
        fs::read_dir(&parent).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(fs::read(&source).is_ok());
    let arguments: Vec<OsString> = vec![
        "--file".into(),
        source.into_os_string(),
        "--family".into(),
        "classic".into(),
        "--output".into(),
        profile.clone().into_os_string(),
        "--template".into(),
    ];
    fixture.spawn_in(&arguments, &installation(), &project);
    assert_success(&fixture.finish());
    let output = Command::new("python3")
        .args(["-E", "-c", "import sys; from pathlib import Path; import yaml; sys.path.insert(0, str(Path(sys.argv[1])/'python')); from failforge_campaign.config import load_yaml; p=Path(sys.argv[2]); v=yaml.safe_load(p.read_text()); c=load_yaml(p,cwd=p.parent,installation=Path(sys.argv[1])); assert v['rules']['include']==['lib/calculator.dart']; assert c.project==Path(sys.argv[3]).resolve()"])
        .arg(installation())
        .arg(&profile)
        .arg(&project)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_success(&output);
}

#[test]
fn template_dispatch_needs_no_runner_or_sdk_arguments() {
    let mut fixture = Fixture::new();
    let installed = fixture.probe_installation();
    let arguments = fixture.args();
    fixture.spawn(&arguments, &installed);
    let output = fixture.finish();
    assert_eq!(output.status.code(), Some(29));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    let mut expected: Vec<_> = arguments
        .iter()
        .map(|value| hex(value.as_bytes()))
        .collect();
    expected.push(hex("--installation"));
    expected.push(hex(installed
        .canonicalize()
        .unwrap()
        .as_os_str()
        .as_bytes()));
    assert_eq!(lines[2..], expected);
    assert!(
        lines[2..]
            .iter()
            .all(|argument| argument != &hex("--runner"))
    );
}

#[test]
fn template_rejects_analysis_flags_before_interpreter_launch() {
    let mut fixture = Fixture::new();
    for flag in ["--dart-sdk", "--flutter-sdk", "--adapter-root"] {
        let mut arguments = fixture.args();
        arguments.extend([
            flag.into(),
            "secret-tool-path".into(),
            "--python".into(),
            "must-not-execute".into(),
        ]);
        let output = fixture.run(&arguments);
        assert_rejected(&output);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-tool-path"));
    }
    assert!(!fixture.profile().exists());
}
