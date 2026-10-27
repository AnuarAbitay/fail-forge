use super::*;
use std::fs;
use std::os::windows::fs::symlink_dir;

struct Fixture {
    parent: PathBuf,
    project: PathBuf,
    private: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let parent = std::env::temp_dir().join(format!(
            "failforge-copy-windows-{}",
            win::random_token().unwrap()
        ));
        fs::create_dir(&parent).unwrap();
        let root = win::directory(&parent).unwrap();
        let private = parent.join("private");
        win::create_directory_at(&root, OsStr::new("private")).unwrap();
        let project = parent.join("project");
        fs::create_dir_all(project.join(".dart_tool")).unwrap();
        fs::create_dir_all(parent.join("test-package")).unwrap();
        fs::write(project.join("pubspec.yaml"), "name: sample\n").unwrap();
        fs::create_dir(project.join("lib")).unwrap();
        fs::write(project.join("lib/value.dart"), "bool value() => false;\n").unwrap();
        fs::write(project.join(".dart_tool/package_config.json"), serde_json::to_vec(&json!({"configVersion":2,"packages":[{"name":"sample","rootUri":"../"},{"name":"test","rootUri":file_uri(&parent.join("test-package")).unwrap()}]})).unwrap()).unwrap();
        fs::write(project.join(".dart_tool/package_graph.json"), serde_json::to_vec(&json!({"roots":["sample"],"packages":[{"name":"sample","dependencies":["test"]},{"name":"test","dependencies":[]}]})).unwrap()).unwrap();
        Self {
            parent,
            project,
            private,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}
#[test]
fn snapshot_rebases_metadata_and_preserves_original_bytes() {
    let f = Fixture::new();
    let cancel = Cancellation::default();
    let before = source_hashes(&f.project, &cancel).unwrap();
    let destination = f.private.join("snapshot");
    let copied = copy_project(&f.project, &destination, &cancel).unwrap();
    assert_eq!(copied.root, destination.canonicalize().unwrap());
    assert_eq!(
        fs::read(destination.join("lib/value.dart")).unwrap(),
        fs::read(f.project.join("lib/value.dart")).unwrap()
    );
    assert_eq!(source_hashes(&f.project, &cancel).unwrap(), before);
    assert_ne!(
        win::identity(&win::regular(&f.project.join("lib/value.dart"), false).unwrap()).unwrap(),
        win::identity(&win::regular(&destination.join("lib/value.dart"), false).unwrap()).unwrap()
    );
}
#[test]
fn concurrent_source_edit_fails_and_cleans_only_owned_destination() {
    let f = Fixture::new();
    let destination = f.private.join("snapshot");
    let error = copy_with(
        &f.project,
        &destination,
        &Cancellation::default(),
        |stage, source, _| {
            if matches!(stage, Stage::Inventoried) {
                fs::write(source.join("lib/value.dart"), "changed").unwrap();
            }
        },
    )
    .unwrap_err();
    assert!(matches!(error, InputError::Invalid(_)));
    assert!(!destination.exists());
    assert_eq!(
        fs::read(f.project.join("lib/value.dart")).unwrap(),
        b"changed"
    );
}
#[test]
fn existing_destination_and_hardlinked_copied_metadata_are_rejected() {
    let f = Fixture::new();
    let destination = f.private.join("snapshot");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("keep"), "original").unwrap();
    assert!(copy_project(&f.project, &destination, &Cancellation::default()).is_err());
    assert_eq!(fs::read(destination.join("keep")).unwrap(), b"original");
    let target = f.private.join("snapshot-2");
    let external = f.parent.join("external");
    let result = copy_with(
        &f.project,
        &target,
        &Cancellation::default(),
        |stage, _, copy| {
            if matches!(stage, Stage::Copied) {
                fs::hard_link(copy.join(".dart_tool/package_config.json"), &external).unwrap();
            }
        },
    );
    assert!(result.is_err());
    let value: serde_json::Value = serde_json::from_slice(&fs::read(external).unwrap()).unwrap();
    assert_eq!(value["packages"][0]["rootUri"], "../");
}
#[test]
fn source_directory_reparse_point_is_never_traversed() {
    let f = Fixture::new();
    let outside = f.parent.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("secret"), "untouched").unwrap();
    match symlink_dir(&outside, f.project.join("alias")) {
        Ok(()) => (),
        Err(error) if error.raw_os_error() == Some(1314) => {
            // Junction creation needs no symlink privilege on hosted Windows.
            assert!(
                std::process::Command::new("cmd.exe")
                    .args(["/D", "/C", "mklink", "/J"])
                    .arg(f.project.join("alias"))
                    .arg(&outside)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        Err(error) => panic!("cannot create hostile alias: {error}"),
    }
    assert!(
        copy_project(
            &f.project,
            &f.private.join("snapshot"),
            &Cancellation::default()
        )
        .is_err()
    );
    assert_eq!(fs::read(outside.join("secret")).unwrap(), b"untouched");
}
#[test]
fn latched_cancellation_creates_no_snapshot() {
    let f = Fixture::new();
    let cancel = Cancellation::default();
    cancel.request(libc::SIGINT);
    assert!(matches!(
        copy_project(&f.project, &f.private.join("snapshot"), &cancel),
        Err(InputError::Cancelled(_))
    ));
    assert!(!f.private.join("snapshot").exists());
}

#[test]
fn readonly_prepared_metadata_is_rebased_without_changing_original_attributes() {
    let f = Fixture::new();
    let source = f.project.join(".dart_tool/package_config.json");
    let source_file = win::regular(&source, false).unwrap();
    win::set_readonly(&source_file, true).unwrap();
    let original = fs::read(&source).unwrap();
    let destination = f.private.join("readonly-snapshot");
    copy_project(&f.project, &destination, &Cancellation::default()).unwrap();
    assert!(fs::metadata(&source).unwrap().permissions().readonly());
    assert!(
        fs::metadata(destination.join(".dart_tool/package_config.json"))
            .unwrap()
            .permissions()
            .readonly()
    );
    assert_eq!(fs::read(&source).unwrap(), original);
    win::set_readonly(&source_file, false).unwrap();
    win::set_readonly(
        &win::regular(&destination.join(".dart_tool/package_config.json"), false).unwrap(),
        false,
    )
    .unwrap();
}
