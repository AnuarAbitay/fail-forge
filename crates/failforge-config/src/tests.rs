use super::*;
use std::fs;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ff-config-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("project/profiles/nested")).unwrap();
        fs::create_dir_all(root.join("installation")).unwrap();
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
    fn profile(&self) -> PathBuf {
        let path = self.0.join("project/profiles/nested/run.yml");
        fs::write(&path, "mutations:\n  classic:\n    enabled: true\n    operators: {BOOLEAN_LITERAL_FLIP: true}\n").unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn direct_and_yaml_paths_use_their_respective_bases() {
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
    let path = f.profile();
    let text = fs::read_to_string(&path).unwrap() + "output: ../result\n";
    fs::write(&path, text).unwrap();
    let profile = load_yaml(&path, &f.context()).unwrap();
    assert_eq!(
        profile.to_value()["project"],
        f.0.join("project").to_str().unwrap()
    );
    assert_eq!(
        profile.to_value()["output"],
        f.0.join("project/profiles/result").to_str().unwrap()
    );
}

#[test]
fn normalized_config_does_not_require_python_or_sdk_files() {
    let f = Fixture::new();
    let value = resolve_config(&json!({"family":"language_specific"}), &f.context()).unwrap();
    assert_eq!(value.to_value()["language"], "dart");
    assert_eq!(
        value.to_value()["timeouts"],
        json!({"test":"30000ms", "command":"120000ms"})
    );
    assert!(!f.0.join("installation/scripts").exists());
    assert_eq!(
        load_yaml(&f.profile(), &f.context())
            .unwrap()
            .to_transport_value()["campaigns"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn symlinked_profile_uses_its_real_parent_and_finds_nearest_package() {
    let f = Fixture::new();
    let source = f.profile();
    fs::write(
        source.parent().unwrap().join("pubspec.yaml"),
        "name: nearest\n",
    )
    .unwrap();
    let alias = f.0.join("alias.yml");
    symlink(&source, &alias).unwrap();
    let result = load_yaml(&alias, &f.context()).unwrap();
    assert_eq!(
        result.to_value()["project"],
        source.parent().unwrap().to_str().unwrap()
    );
}

#[test]
fn path_normalization_follows_existing_links_then_preserves_missing_suffixes() {
    let f = Fixture::new();
    symlink(f.0.join("project"), f.0.join("alias")).unwrap();
    let value = paths::resolve_path(&json!("alias/missing/../new/file"), "output", &f.0).unwrap();
    assert_eq!(value, f.0.join("project/new/file"));
    symlink("loop", f.0.join("loop")).unwrap();
    assert!(paths::resolve_path(&json!("loop/file"), "output", &f.0).is_err());
}

#[test]
fn long_finite_symlink_chains_and_repeated_nonrecursive_links_are_allowed() {
    let f = Fixture::new();
    for index in 0..150 {
        let target = if index == 149 {
            "project".into()
        } else {
            format!("chain-{}", index + 1)
        };
        symlink(target, f.0.join(format!("chain-{index}"))).unwrap();
    }
    assert_eq!(
        paths::resolve_path(&json!("chain-0/new"), "output", &f.0).unwrap(),
        f.0.join("project/new")
    );
    symlink("project", f.0.join("alias")).unwrap();
    assert_eq!(
        paths::resolve_path(&json!("alias/../alias/new"), "output", &f.0).unwrap(),
        f.0.join("project/new")
    );
}

#[test]
fn repeated_slashes_after_tilde_preserve_the_home_directory() {
    let f = Fixture::new();
    let home = paths::resolve_path(&json!("~"), "output", &f.0).unwrap();
    for text in ["~/subject", "~//subject", "~///subject"] {
        assert_eq!(
            paths::resolve_path(&json!(text), "output", &f.0).unwrap(),
            home.join("subject")
        );
    }
    assert_eq!(
        paths::resolve_path(&json!("~//"), "output", &f.0).unwrap(),
        home
    );
}

#[test]
fn invalid_paths_are_errors_before_any_file_is_created() {
    let f = Fixture::new();
    for input in [
        Value::Null,
        json!(true),
        json!(3),
        json!(""),
        json!("bad\0path"),
    ] {
        assert!(
            paths::resolve_path(&input, "output", &f.0).is_err(),
            "{input:?}"
        );
    }
    assert_eq!(fs::read_dir(f.0.join("installation")).unwrap().count(), 0);
}

#[test]
fn configuration_and_pubspec_reads_are_bounded_regular_files() {
    let f = Fixture::new();
    let path = f.profile();
    fs::write(&path, vec![b' '; MAX_YAML_BYTES + 1]).unwrap();
    assert!(
        load_yaml(&path, &f.context())
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(
        load_yaml(&path, &f.context())
            .unwrap_err()
            .to_string()
            .contains("UTF-8")
    );
    assert!(paths::read_regular_bytes(path.parent().unwrap(), MAX_YAML_BYTES, true).is_err());
    let path = f.profile();
    fs::remove_file(f.0.join("project/pubspec.yaml")).unwrap();
    symlink(&path, f.0.join("project/pubspec.yaml")).unwrap();
    assert!(
        load_yaml(&path, &f.context())
            .unwrap_err()
            .to_string()
            .contains("regular file")
    );
}

#[test]
fn fifo_configuration_is_rejected_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let f = Fixture::new();
    let fifo = f.0.join("pipe.yml");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a live NUL-terminated path, mode is an ordinary permission mask.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(load_yaml(&fifo, &f.context()).is_err());
}

#[test]
fn explicit_profile_cannot_be_mixed_with_even_empty_semantic_flags() {
    let f = Fixture::new();
    for (key, value) in [
        ("include", json!([])),
        ("workers", json!(0)),
        ("family", json!("classic")),
    ] {
        let mut args = json!({"config": f.profile()});
        args[key] = value;
        assert!(
            resolve_config(&args, &f.context())
                .unwrap_err()
                .to_string()
                .contains("cannot be combined")
        );
    }
    assert!(resolve_config(&json!({"family":"classic", "extra":true}), &f.context()).is_err());
}

#[test]
fn absent_config_does_not_discover_profiles_in_the_current_directory() {
    let f = Fixture::new();
    fs::write(f.0.join("failforge.yml"), "invalid: true").unwrap();
    let config = resolve_config(
        &json!({"family":"classic", "installation":"ignored", "runner":"ignored"}),
        &f.context(),
    )
    .unwrap();
    assert_eq!(config.to_value()["family"], "classic");
    assert!(resolve_config(&json!({}), &f.context()).is_err());
}

#[test]
fn single_and_multi_family_transport_preserve_effective_documents() {
    let f = Fixture::new();
    let config = resolve_config(&json!({"family":"classic"}), &f.context()).unwrap();
    let transport = config.to_transport_value();
    assert_eq!(transport["configuration"], transport["campaigns"][0]);
    assert_eq!(transport["kind"], "campaign");
    let path = f.profile();
    let text = fs::read_to_string(&path).unwrap()
        + "  language_specific:\n    language: dart\n    enabled: true\n    operators: {ENM-001: true}\n";
    fs::write(&path, text).unwrap();
    let transport = load_yaml(&path, &f.context()).unwrap().to_transport_value();
    assert_eq!(transport["kind"], "profile");
    assert_eq!(transport["campaigns"].as_array().unwrap().len(), 2);
    assert_eq!(transport["campaigns"][1]["language"], "dart");
}
