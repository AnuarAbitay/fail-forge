use super::*;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ff Windows config {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("project/profiles/nested")).unwrap();
        fs::create_dir(root.join("installation")).unwrap();
        fs::write(
            root.join("project/pubspec.yaml"),
            "name: configuration_fixture\n",
        )
        .unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn context(&self) -> Context {
        Context::new(&self.0, &self.0.join("installation")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn direct_and_yaml_paths_keep_their_windows_bases() {
    let f = Fixture::new();
    let direct = resolve_config(
        &json!({"family":"classic", "output":"reports/../result"}),
        &f.context(),
    )
    .unwrap();
    assert_eq!(
        direct.to_value()["output"],
        f.0.join("result").to_str().unwrap()
    );
    let profile = f.0.join("project/profiles/nested/run.yml");
    fs::write(&profile, "mutations:\n  classic:\n    enabled: true\n    operators: {BOOLEAN_LITERAL_FLIP: true}\noutput: ../result\n").unwrap();
    let value = load_yaml(&profile, &f.context()).unwrap().to_value();
    assert_eq!(value["project"], f.0.join("project").to_str().unwrap());
    assert_eq!(
        value["output"],
        f.0.join("project/profiles/result").to_str().unwrap()
    );
}

#[test]
fn windows_separators_and_missing_unicode_suffixes_are_normalized() {
    let f = Fixture::new();
    let forward =
        paths::resolve_native_path(Path::new("new space/данные/../result"), "output", &f.0)
            .unwrap();
    let backward =
        paths::resolve_native_path(Path::new("new space\\данные\\..\\result"), "output", &f.0)
            .unwrap();
    assert_eq!(forward, backward);
    assert_eq!(forward, f.0.join("new space/result"));
    assert!(!forward.exists());
}

#[test]
fn drive_relative_and_device_paths_are_rejected() {
    let f = Fixture::new();
    assert!(paths::resolve_native_path(Path::new("C:relative"), "output", &f.0).is_err());
    assert!(paths::resolve_native_path(Path::new(r"\\.\pipe\foreign"), "output", &f.0).is_err());
    assert!(paths::read_regular_bytes(Path::new(r"\\.\NUL"), 10, true).is_err());
}

#[test]
fn bounded_reader_accepts_regular_files_and_rejects_directories() {
    let f = Fixture::new();
    let file = f.0.join("config.yml");
    fs::write(&file, b"0123456789").unwrap();
    assert_eq!(
        paths::read_regular_bytes(&file, 4, false).unwrap(),
        b"01234"
    );
    assert!(paths::read_regular_bytes(&f.0, 4, false).is_err());
}

#[test]
fn windows_home_expansion_uses_the_user_profile() {
    let f = Fixture::new();
    let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    assert_eq!(
        paths::resolve_native_path(Path::new(r"~\new-profile"), "output", &f.0).unwrap(),
        paths::resolve_native_path(&home.join("new-profile"), "output", &f.0).unwrap()
    );
    assert!(
        paths::resolve_native_path(Path::new("~some-other-user/result"), "output", &f.0).is_err()
    );
}

#[test]
fn native_binary_names_are_windows_executables() {
    assert_eq!(platform::binary_name("failforge"), "failforge.exe");
    assert_eq!(platform::DART_BIN, "bin/dart.exe");
    assert_eq!(platform::FLUTTER_BIN, "bin/flutter.bat");
}
