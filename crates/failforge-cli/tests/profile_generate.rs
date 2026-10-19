//! Native profile generation needs no Python interpreter or PyYAML.
#![cfg(unix)]
use failforge_config::{Context, RunConfig, load_yaml};
use serde_json::{Value, json};
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    child: Option<Child>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ff native profile {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("package/lib")).unwrap();
        fs::create_dir(root.join("installation")).unwrap();
        fs::write(root.join("package/pubspec.yaml"), "name: fixture\n").unwrap();
        fs::write(
            root.join("package/lib/calculator.dart"),
            "int add(int a, int b) => a + b;\n",
        )
        .unwrap();
        Self { root, child: None }
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
    fn spawn(&mut self, args: &[OsString]) {
        assert!(self.child.is_none());
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_failforge"))
                .args(["profile", "generate"])
                .args(args)
                .arg("--installation")
                .arg(self.root.join("installation"))
                .env("PATH", "/nonexistent-path")
                .env("PYTHONPATH", "/untrusted-python")
                .current_dir(&self.root)
                .stdin(Stdio::null())
                .stdout(File::create(self.root.join("stdout")).unwrap())
                .stderr(File::create(self.root.join("stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
    }
    fn finish(&mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "profile generation exceeded test deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.child.take();
        Output {
            status,
            stdout: fs::read(self.root.join("stdout")).unwrap(),
            stderr: fs::read(self.root.join("stderr")).unwrap(),
        }
    }
    fn run(&mut self, args: &[OsString]) -> Output {
        self.spawn(args);
        self.finish()
    }
    fn raw(&self, path: &Path) -> Value {
        let context = Context::new(&self.root, &self.root.join("installation")).unwrap();
        let RunConfig::Profile(profile) = load_yaml(path, &context).unwrap() else {
            panic!("expected profile");
        };
        assert_eq!(
            profile.project(),
            self.root.join("package").canonicalize().unwrap()
        );
        let raw = failforge_config::yaml::parse_yaml(&fs::read_to_string(path).unwrap()).unwrap();
        assert!(raw.get("version").is_none() && raw.get("project").is_none());
        raw
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
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status {:?}, stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn rejected(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn templates_work_without_interpreter_python_sources_or_path_programs() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args.extend(["--python".into(), "/missing/python".into()]);
    success(&f.run(&args));
    let raw = f.raw(&f.profile());
    let switches = raw["mutations"]["classic"]["operators"]
        .as_object()
        .unwrap();
    assert_eq!(switches.len(), 32);
    assert_eq!(
        switches
            .values()
            .filter(|v| v.as_bool() == Some(true))
            .count(),
        31
    );
    assert_eq!(switches["CONDITIONAL_NEGATION"], false);
    assert_eq!(raw["workers"], 1);
    assert_eq!(raw["timeouts"], json!({"test":"30s","command":"120s"}));
    assert_eq!(raw["limits"], json!({"max_mutants":1000}));
    assert_eq!(raw["rules"]["include"], json!(["lib/calculator.dart"]));
    assert!(!f.root.join("package/.dart_tool").exists());
}

#[test]
fn selected_families_preserve_independent_switches() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args.extend(
        [
            "--family",
            "language_specific",
            "--family",
            "failure_mode",
            "--language",
            "dart",
            "--operator",
            "BOOLEAN_LITERAL_FLIP",
            "--operator",
            "NSM-001",
        ]
        .map(Into::into),
    );
    success(&f.run(&args));
    let raw = f.raw(&f.profile());
    let selections = raw["mutations"].as_object().unwrap();
    assert_eq!(selections.len(), 3);
    for (family, expected) in [
        ("classic", vec!["BOOLEAN_LITERAL_FLIP"]),
        ("failure_mode", vec![]),
        ("language_specific", vec!["NSM-001"]),
    ] {
        let actual: Vec<_> = selections[family]["operators"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| v.as_bool() == Some(true))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(actual, expected);
    }
    assert_eq!(selections["language_specific"]["language"], "dart");
}

#[test]
fn dart_semantic_template_has_all_nineteen_operators() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args[3] = "language_specific".into();
    args.extend(["--language".into(), "dart".into()]);
    success(&f.run(&args));
    let raw = f.raw(&f.profile());
    let operators = raw["mutations"]["language_specific"]["operators"]
        .as_object()
        .unwrap();
    assert_eq!(operators.len(), 19);
    assert!(operators.values().all(|v| v.as_bool() == Some(true)));
}

#[test]
fn failure_mode_uses_flutter_test_identity_and_author_yaml_aliases() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args[3] = "failure_mode".into();
    for (pubspec, expected) in [
        ("name: fixture\n", false),
        (
            "name: fixture\ndependencies:\n  flutter:\n    sdk: flutter\n",
            false,
        ),
        (
            "name: fixture\nshared: &flutter {sdk: flutter}\ndev_dependencies:\n  flutter_test: *flutter\n",
            true,
        ),
    ] {
        fs::write(f.root.join("package/pubspec.yaml"), pubspec).unwrap();
        success(&f.run(&args));
        assert_eq!(
            f.raw(&f.profile())["mutations"]["failure_mode"]["operators"],
            json!({"EXCEPTION_AT_ENTRY":true,"METHOD_CHANNEL_ERROR":expected})
        );
        if !args.iter().any(|a| a == "--force") {
            args.push("--force".into());
        }
    }
}

#[test]
fn selection_errors_create_no_profile_directories() {
    let mut f = Fixture::new();
    for extra in [
        vec!["--language", "dart"],
        vec!["--operator", "NSM-001"],
        vec!["--operator", "UNKNOWN_OPERATOR"],
        vec!["--family", "language_specific"],
        vec!["--family", "unknown"],
    ] {
        let mut args = f.args();
        args.extend(extra.into_iter().map(Into::into));
        rejected(&f.run(&args));
        assert!(!f.root.join("package/profiles").exists());
    }
}

#[test]
fn dry_run_outputs_only_loadable_yaml_and_never_creates_output_parent() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args.push("--dry-run".into());
    let output = f.run(&args);
    success(&output);
    assert!(output.stderr.is_empty());
    assert!(!f.root.join("package/profiles").exists());
    let preview = f.root.join("package/preview.yml");
    fs::write(&preview, &output.stdout).unwrap();
    assert_eq!(
        f.raw(&preview)["rules"]["include"],
        json!(["lib/calculator.dart"])
    );
}

#[test]
fn force_replaces_only_regular_files_and_preserves_mode() {
    let mut f = Fixture::new();
    let mut args = f.args();
    fs::create_dir_all(f.profile().parent().unwrap()).unwrap();
    fs::write(f.profile(), "old profile\n").unwrap();
    fs::set_permissions(f.profile(), fs::Permissions::from_mode(0o600)).unwrap();
    rejected(&f.run(&args));
    assert_eq!(fs::read_to_string(f.profile()).unwrap(), "old profile\n");
    args.push("--force".into());
    success(&f.run(&args));
    assert_eq!(
        fs::metadata(f.profile()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    f.raw(&f.profile());
}

#[test]
fn force_rejects_symlinks_directories_and_fifos_without_touching_targets() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args.push("--force".into());
    fs::create_dir_all(f.profile().parent().unwrap()).unwrap();
    let target = f.root.join("protected");
    fs::write(&target, "preserve").unwrap();
    symlink(&target, f.profile()).unwrap();
    rejected(&f.run(&args));
    assert_eq!(fs::read_to_string(&target).unwrap(), "preserve");
    fs::remove_file(f.profile()).unwrap();
    symlink(f.root.join("missing"), f.profile()).unwrap();
    rejected(&f.run(&args));
    fs::remove_file(f.profile()).unwrap();
    fs::create_dir(f.profile()).unwrap();
    rejected(&f.run(&args));
    fs::remove_dir(f.profile()).unwrap();
    let path = CString::new(f.profile().as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    rejected(&f.run(&args));
}

#[test]
fn output_must_share_source_package_and_cannot_replace_reserved_files() {
    let mut f = Fixture::new();
    fs::create_dir(f.root.join("package/nested")).unwrap();
    fs::write(f.root.join("package/nested/pubspec.yaml"), "name: nested\n").unwrap();
    for relative in [
        "package/nested/profile.yml",
        "outside/profile.yml",
        "package/pubspec.yaml",
        "package/analysis_options.yaml",
        "package/.dart_tool/profile.yml",
        "package/profiles/one.json",
    ] {
        let mut args = f.args();
        args[5] = f.root.join(relative).into_os_string();
        args.push("--force".into());
        rejected(&f.run(&args));
    }
    assert_eq!(
        fs::read_to_string(f.root.join("package/pubspec.yaml")).unwrap(),
        "name: fixture\n"
    );
}

#[test]
fn source_and_output_unicode_spaces_quotes_and_relative_paths_are_literal() {
    let mut f = Fixture::new();
    let source = f.root.join("package/lib/есеп 'quoted'.dart");
    fs::rename(f.source(), &source).unwrap();
    let profile = f.root.join("package/профили 'quoted'/есеп.yaml");
    let mut args = f.args();
    args[1] = source.strip_prefix(&f.root).unwrap().as_os_str().to_owned();
    args[5] = profile.clone().into_os_string();
    let output = f.run(&args);
    success(&output);
    assert_eq!(
        f.raw(&profile)["rules"]["include"],
        json!(["lib/есеп 'quoted'.dart"])
    );
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Run: failforge mutate --config")
    );
}

#[test]
fn invalid_source_paths_and_special_files_are_rejected_without_blocking() {
    let mut f = Fixture::new();
    for name in [
        "missing.dart",
        "source.txt",
        "file*.dart",
        "file?.dart",
        "file[1].dart",
        "file{1}.dart",
        "file\\name.dart",
    ] {
        let source = f.root.join("package/lib").join(name);
        if name != "missing.dart" {
            fs::write(&source, "void main() {}").unwrap();
        }
        let mut args = f.args();
        args[1] = source.into_os_string();
        rejected(&f.run(&args));
    }
    let directory = f.root.join("package/lib/directory.dart");
    fs::create_dir(&directory).unwrap();
    let fifo = f.root.join("package/lib/pipe.dart");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    for source in [directory, fifo] {
        let mut args = f.args();
        args[1] = source.into_os_string();
        rejected(&f.run(&args));
    }
    assert!(!f.root.join("package/profiles").exists());
}

#[test]
fn internal_source_and_output_parent_symlinks_are_rejected() {
    let mut f = Fixture::new();
    let alias = f.root.join("package/lib/alias.dart");
    symlink(f.source(), &alias).unwrap();
    let mut args = f.args();
    args[1] = alias.into_os_string();
    rejected(&f.run(&args));
    symlink(f.root.join("package/lib"), f.root.join("package/linked")).unwrap();
    args[1] = f
        .root
        .join("package/linked/calculator.dart")
        .into_os_string();
    rejected(&f.run(&args));
    let outside = f.root.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, f.root.join("package/profiles")).unwrap();
    rejected(&f.run(&f.args()));
    assert!(fs::read_dir(outside).unwrap().next().is_none());
}

#[test]
fn concurrent_generators_publish_exactly_one_complete_file() {
    let mut first = Fixture::new();
    let mut second = Fixture::new();
    let args = first.args();
    first.spawn(&args);
    second.spawn(&args);
    let a = first.finish();
    let b = second.finish();
    let mut codes = [a.status.code(), b.status.code()];
    codes.sort();
    assert_eq!(
        codes,
        [Some(0), Some(2)],
        "{} / {}",
        String::from_utf8_lossy(&a.stderr),
        String::from_utf8_lossy(&b.stderr)
    );
    first.raw(&first.profile());
    let names: Vec<_> = fs::read_dir(first.profile().parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, [OsString::from("calculator.yml")]);
}

#[test]
fn non_unicode_paths_are_rejected_without_lossy_conversion() {
    let mut f = Fixture::new();
    let source = f
        .root
        .join("package/lib")
        .join(OsString::from_vec(b"invalid-\xff.dart".to_vec()));
    let mut args = f.args();
    args[1] = source.into_os_string();
    rejected(&f.run(&args));
    assert!(!f.profile().exists());
}

#[test]
fn default_analysis_requires_prepared_project_instead_of_falling_back_to_template() {
    let mut f = Fixture::new();
    let mut args = f.args();
    args.retain(|a| a != "--template");
    let output = f.run(&args);
    assert!(!output.status.success());
    assert!(!f.profile().exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("prepared"));
}
