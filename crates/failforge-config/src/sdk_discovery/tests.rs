use super::*;
use crate::{Context, from_mapping, load_yaml};
use serde_json::json;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ff-sdk-discovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn executable(&self, path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Running a candidate would create a marker and fail. Discovery only
        // reads metadata, so even shell scripts are safe filesystem fixtures.
        fs::write(
            path,
            format!(
                "#!/bin/sh\ntouch '{}'\nexit 99\n",
                self.0.join("executable-ran").display()
            ),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn sdk(&self, relative: &str) -> PathBuf {
        let root = self.0.join(relative);
        self.executable(&root.join("bin/dart"));
        fs::create_dir_all(root.join("lib/core")).unwrap();
        fs::write(root.join("lib/core/core.dart"), "library dart.core;\n").unwrap();
        root
    }

    fn context(&self, search_path: Option<OsString>) -> Context {
        Context::new(&self.0, &self.0.join("installation"))
            .unwrap()
            .with_sdk_search_path(search_path)
    }

    fn configure(&self, search_path: Option<OsString>) -> crate::CampaignConfig {
        from_mapping(
            &json!({"family":"classic"}),
            &self.context(search_path),
            None,
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn search_uses_path_directory_order_without_running_executables() {
    let f = Fixture::new();
    let first = f.sdk("first");
    let second = f.sdk("second");
    for (entries, expected) in [
        ([first.join("bin"), second.join("bin")], &first),
        ([second.join("bin"), first.join("bin")], &second),
    ] {
        let config = f.configure(Some(std::env::join_paths(entries).unwrap()));
        assert_eq!(&config.dart_sdk, expected);
        assert_eq!(config.flutter_sdk, None);
    }
    assert!(!f.0.join("executable-ran").exists());
}

#[test]
fn dart_precedes_flutter_in_the_same_path_directory() {
    let f = Fixture::new();
    let dart = f.sdk("dart-sdk");
    let flutter_dart = f.sdk("flutter/bin/cache/dart-sdk");
    let bin = f.0.join("commands");
    fs::create_dir(&bin).unwrap();
    symlink(dart.join("bin/dart"), bin.join("dart")).unwrap();
    f.executable(&f.0.join("flutter/bin/flutter"));
    symlink(f.0.join("flutter/bin/flutter"), bin.join("flutter")).unwrap();
    assert_eq!(f.configure(Some(bin.clone().into())).dart_sdk, dart);
    fs::remove_file(bin.join("dart")).unwrap();
    assert_eq!(f.configure(Some(bin.into())).dart_sdk, flutter_dart);
}

#[test]
fn flutter_in_an_earlier_directory_precedes_dart_in_a_later_directory() {
    let f = Fixture::new();
    let flutter_sdk = f.sdk("flutter/bin/cache/dart-sdk");
    let dart_sdk = f.sdk("dart-sdk");
    f.executable(&f.0.join("flutter/bin/flutter"));
    let search_path =
        std::env::join_paths([f.0.join("flutter/bin"), dart_sdk.join("bin")]).unwrap();
    let config = f.configure(Some(search_path));
    assert_eq!(config.dart_sdk, flutter_sdk);
    assert_eq!(config.flutter_sdk, Some(f.0.join("flutter")));
}

#[test]
fn flutter_dart_wrapper_and_flutter_command_use_cached_dart_sdk() {
    let f = Fixture::new();
    let sdk = f.sdk("flutter/bin/cache/dart-sdk");
    let bin = f.0.join("flutter/bin");
    f.executable(&bin.join("flutter"));
    f.executable(&bin.join("dart"));
    for with_dart_wrapper in [true, false] {
        if !with_dart_wrapper {
            fs::remove_file(bin.join("dart")).unwrap();
        }
        let config = f.configure(Some(bin.clone().into()));
        assert_eq!(config.dart_sdk, sdk);
        assert_eq!(config.flutter_sdk, Some(f.0.join("flutter")));
    }
    assert!(!f.0.join("executable-ran").exists());
}

#[test]
fn symlinked_flutter_cache_returns_the_canonical_dart_sdk() {
    let f = Fixture::new();
    let sdk = f.sdk("shared-dart-sdk");
    let bin = f.0.join("flutter/bin");
    f.executable(&bin.join("flutter"));
    fs::create_dir_all(bin.join("cache")).unwrap();
    symlink(&sdk, bin.join("cache/dart-sdk")).unwrap();
    let config = f.configure(Some(bin.into()));
    assert_eq!(config.dart_sdk, sdk);
    assert_eq!(config.flutter_sdk, Some(f.0.join("flutter")));
}

#[test]
fn flutter_dart_symlink_to_an_external_sdk_preserves_the_launcher_root() {
    let f = Fixture::new();
    let sdk = f.sdk("external-sdk");
    let flutter = f.0.join("flutter");
    let bin = flutter.join("bin");
    f.executable(&bin.join("flutter"));
    fs::create_dir_all(bin.join("cache")).unwrap();
    symlink(&sdk, bin.join("cache/dart-sdk")).unwrap();
    symlink(sdk.join("bin/dart"), bin.join("dart")).unwrap();
    let alias = f.0.join("linked-flutter-bin");
    symlink(&bin, &alias).unwrap();
    for directory in [bin, alias] {
        let config = f.configure(Some(directory.into()));
        assert_eq!(config.dart_sdk, sdk);
        assert_eq!(config.flutter_sdk, Some(flutter.clone()));
    }
}

#[test]
fn a_bundled_dart_sdk_recognizes_its_owning_flutter_root() {
    let f = Fixture::new();
    let sdk = f.sdk("flutter/bin/cache/dart-sdk");
    f.executable(&f.0.join("flutter/bin/flutter"));
    let config = f.configure(Some(sdk.join("bin").into()));
    assert_eq!(config.dart_sdk, sdk);
    assert_eq!(config.flutter_sdk, Some(f.0.join("flutter")));
}

#[test]
fn a_dart_link_does_not_select_flutter_when_the_cached_sdk_differs() {
    let f = Fixture::new();
    let selected = f.sdk("external-sdk");
    f.sdk("flutter/bin/cache/dart-sdk");
    let bin = f.0.join("flutter/bin");
    f.executable(&bin.join("flutter"));
    symlink(selected.join("bin/dart"), bin.join("dart")).unwrap();
    let config = f.configure(Some(bin.into()));
    assert_eq!(config.dart_sdk, selected);
    assert_eq!(config.flutter_sdk, None);
}

#[test]
fn flutter_root_requires_an_executable_flutter_launcher() {
    let f = Fixture::new();
    let sdk = f.sdk("flutter/bin/cache/dart-sdk");
    let config = f.configure(Some(sdk.join("bin").into()));
    assert_eq!(config.dart_sdk, sdk);
    assert_eq!(config.flutter_sdk, None);
    let flutter = f.0.join("flutter/bin/flutter");
    f.executable(&flutter);
    fs::set_permissions(&flutter, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(f.configure(Some(sdk.join("bin").into())).flutter_sdk, None);
}

#[test]
fn executable_and_directory_symlinks_resolve_to_canonical_sdk_root() {
    let f = Fixture::new();
    let sdk = f.sdk("canonical-sdk");
    let aliases = f.0.join("aliases");
    fs::create_dir(&aliases).unwrap();
    symlink("../canonical-sdk/bin/dart", aliases.join("dart")).unwrap();
    assert_eq!(f.configure(Some(aliases.into())).dart_sdk, sdk);
    let linked_bin = f.0.join("linked-bin");
    symlink("canonical-sdk/bin", &linked_bin).unwrap();
    assert_eq!(f.configure(Some(linked_bin.into())).dart_sdk, sdk);
}

#[test]
fn malformed_nonexecutable_and_uncached_candidates_do_not_hide_later_sdk() {
    let f = Fixture::new();
    let shim = f.0.join("opaque-shim/bin");
    f.executable(&shim.join("dart"));
    let uncached = f.0.join("uncached-flutter/bin");
    f.executable(&uncached.join("flutter"));
    let nonexecutable = f.sdk("nonexecutable");
    fs::set_permissions(
        nonexecutable.join("bin/dart"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let missing_core = f.sdk("missing-core");
    fs::remove_file(missing_core.join("lib/core/core.dart")).unwrap();
    let directory_core = f.sdk("directory-core");
    fs::remove_file(directory_core.join("lib/core/core.dart")).unwrap();
    fs::create_dir(directory_core.join("lib/core/core.dart")).unwrap();
    let directory_executable = f.sdk("directory-executable");
    fs::remove_file(directory_executable.join("bin/dart")).unwrap();
    fs::create_dir(directory_executable.join("bin/dart")).unwrap();
    let valid = f.sdk("valid");
    let entries = [
        f.0.join("missing"),
        shim,
        uncached,
        nonexecutable.join("bin"),
        missing_core.join("bin"),
        directory_core.join("bin"),
        directory_executable.join("bin"),
        valid.join("bin"),
    ];
    let config = f.configure(Some(std::env::join_paths(entries).unwrap()));
    assert_eq!(config.dart_sdk, valid);
    assert!(!f.0.join("executable-ran").exists());
}

#[test]
fn owner_inaccessible_executables_do_not_mask_a_later_usable_sdk() {
    // Root can execute files with any execute bit, so mode 0401 only denies
    // execution to an ordinary owning user.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let f = Fixture::new();
    let blocked = f.sdk("blocked-sdk");
    let cached = f.sdk("blocked-flutter/bin/cache/dart-sdk");
    let flutter_bin = f.0.join("blocked-flutter/bin");
    f.executable(&flutter_bin.join("dart"));
    f.executable(&flutter_bin.join("flutter"));
    for path in [
        blocked.join("bin/dart"),
        flutter_bin.join("dart"),
        flutter_bin.join("flutter"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o401)).unwrap();
    }
    let valid = f.sdk("usable-sdk");
    let search_path =
        std::env::join_paths([blocked.join("bin"), flutter_bin, valid.join("bin")]).unwrap();
    let config = f.configure(Some(search_path));
    assert_eq!(config.dart_sdk, valid);
    assert_eq!(config.flutter_sdk, None);
    let cached_config = f.configure(Some(cached.join("bin").into()));
    assert_eq!(cached_config.dart_sdk, cached);
    assert_eq!(cached_config.flutter_sdk, None);
}

#[test]
fn relative_and_empty_path_entries_use_context_cwd() {
    let f = Fixture::new();
    let sdk = f.sdk("relative-sdk");
    assert_eq!(
        f.configure(Some(OsString::from("relative-sdk/bin")))
            .dart_sdk,
        sdk
    );
    symlink(sdk.join("bin/dart"), f.0.join("dart")).unwrap();
    for path in ["", ":missing", "missing:"] {
        assert_eq!(f.configure(Some(OsString::from(path))).dart_sdk, sdk);
    }
}

#[test]
fn absent_search_path_or_no_valid_candidate_preserves_installed_fallback() {
    let f = Fixture::new();
    let sdk = f.sdk("available-sdk");
    symlink(sdk.join("bin/dart"), f.0.join("dart")).unwrap();
    let expected =
        f.0.join("installation/.toolchain/flutter/bin/cache/dart-sdk");
    assert_eq!(f.configure(None).dart_sdk, expected);
    assert_eq!(
        f.configure(Some(OsString::from("missing/bin"))).dart_sdk,
        expected
    );
    fs::remove_file(f.0.join("dart")).unwrap();
    assert_eq!(f.configure(Some(OsString::new())).dart_sdk, expected);
    assert_eq!(f.context(None).sdk_search_path, None);
}

#[test]
fn explicit_sdk_inputs_have_priority_even_when_the_path_does_not_exist() {
    let f = Fixture::new();
    f.sdk("available-flutter/bin/cache/dart-sdk");
    f.executable(&f.0.join("available-flutter/bin/flutter"));
    let context = f.context(Some(f.0.join("available-flutter/bin").into()));
    for (value, expected, flutter) in [
        (
            json!({"family":"classic","dart_sdk":"chosen-dart"}),
            f.0.join("chosen-dart"),
            None,
        ),
        (
            json!({"family":"classic","flutter_sdk":"chosen-flutter"}),
            f.0.join("chosen-flutter/bin/cache/dart-sdk"),
            Some(f.0.join("chosen-flutter")),
        ),
    ] {
        let config = from_mapping(&value, &context, None).unwrap();
        assert_eq!(config.dart_sdk, expected);
        assert_eq!(config.flutter_sdk, flutter);
    }
    for value in [
        json!({"family":"classic","dart_sdk":""}),
        json!({"family":"classic","flutter_sdk":true}),
        json!({"family":"classic","dart_sdk":"dart","flutter_sdk":"flutter"}),
    ] {
        assert!(from_mapping(&value, &context, None).is_err());
    }
}

#[test]
fn yaml_profiles_preserve_search_path_and_keep_its_relative_base_at_context_cwd() {
    let f = Fixture::new();
    let sdk = f.sdk("sdk");
    let project = f.0.join("project");
    fs::create_dir_all(project.join("profiles")).unwrap();
    fs::write(project.join("pubspec.yaml"), "name: discovery_fixture\n").unwrap();
    let profile = project.join("profiles/run.yaml");
    fs::write(&profile, "mutations:\n  classic:\n    enabled: true\n    operators:\n      BOOLEAN_LITERAL_FLIP: true\n").unwrap();
    let context = f.context(Some(OsString::from("sdk/bin")));
    let config = load_yaml(&profile, &context).unwrap();
    assert_eq!(config.to_value()["dart_sdk"], json!(sdk));
    assert_eq!(config.to_value()["flutter_sdk"], json!(null));
    fs::write(
        &profile,
        fs::read_to_string(&profile).unwrap() + "dart_sdk: ../../chosen-sdk\n",
    )
    .unwrap();
    assert_eq!(
        load_yaml(&profile, &context).unwrap().to_value()["dart_sdk"],
        json!(f.0.join("chosen-sdk"))
    );
}

#[test]
fn yaml_profiles_keep_a_discovered_flutter_root_with_an_external_cache() {
    let f = Fixture::new();
    let sdk = f.sdk("external-sdk");
    let flutter = f.0.join("flutter");
    f.executable(&flutter.join("bin/flutter"));
    fs::create_dir_all(flutter.join("bin/cache")).unwrap();
    symlink(&sdk, flutter.join("bin/cache/dart-sdk")).unwrap();
    let project = f.0.join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("pubspec.yaml"), "name: flutter_discovery\n").unwrap();
    let profile = project.join("run.yaml");
    fs::write(&profile, "mutations:\n  classic:\n    enabled: true\n    operators:\n      BOOLEAN_LITERAL_FLIP: true\n").unwrap();
    let context = f.context(Some(flutter.join("bin").into()));
    let config = load_yaml(&profile, &context).unwrap();
    assert_eq!(config.to_value()["dart_sdk"], json!(sdk));
    assert_eq!(config.to_value()["flutter_sdk"], json!(flutter));
}

#[test]
fn nonunicode_path_entries_are_gracefully_skipped() {
    let f = Fixture::new();
    let invalid = f.0.join(OsString::from_vec(b"sdk-\xff".to_vec()));
    let valid = f.sdk("unicode-sdk");
    let entries = [invalid.join("bin"), valid.join("bin")];
    assert_eq!(
        f.configure(Some(std::env::join_paths(entries).unwrap()))
            .dart_sdk,
        valid
    );
}

#[test]
#[cfg(target_os = "linux")]
fn nonunicode_sdk_roots_are_skipped_but_aliases_to_unicode_sdks_are_supported() {
    // macOS filesystems reject invalid UTF-8 names before discovery runs.
    let f = Fixture::new();
    let invalid = f.0.join(OsString::from_vec(b"sdk-\xff".to_vec()));
    f.executable(&invalid.join("bin/dart"));
    fs::create_dir_all(invalid.join("lib/core")).unwrap();
    fs::write(invalid.join("lib/core/core.dart"), "library dart.core;\n").unwrap();
    let valid = f.sdk("unicode-sdk");
    let entries = [invalid.join("bin"), valid.join("bin")];
    assert_eq!(
        f.configure(Some(std::env::join_paths(entries).unwrap()))
            .dart_sdk,
        valid
    );
    let alias = f.0.join(OsString::from_vec(b"alias-\xff".to_vec()));
    symlink(valid.join("bin"), &alias).unwrap();
    assert_eq!(f.configure(Some(alias.into())).dart_sdk, valid);
}

#[test]
fn broken_or_recursive_executable_links_are_skipped() {
    let f = Fixture::new();
    let bad_bin = f.0.join("bad-bin");
    fs::create_dir(&bad_bin).unwrap();
    symlink("dart", bad_bin.join("dart")).unwrap();
    symlink("missing-flutter", bad_bin.join("flutter")).unwrap();
    let valid = f.sdk("valid");
    assert_eq!(
        f.configure(Some(
            std::env::join_paths([bad_bin, valid.join("bin")]).unwrap()
        ))
        .dart_sdk,
        valid
    );
}
