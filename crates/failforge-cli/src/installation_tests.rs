use super::*;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

#[test]
fn packaged_executable_selects_its_own_installation() {
    assert_eq!(
        from_executable(Path::new("/opt/FailForge beta/bin/failforge")),
        Some(PathBuf::from("/opt/FailForge beta"))
    );
}

#[test]
fn source_builds_select_the_checkout_containing_the_executable() {
    for profile in ["debug", "release"] {
        let executable = PathBuf::from(format!("/moved/checkout/target/{profile}/failforge"));
        assert_eq!(
            from_executable(&executable),
            Some(PathBuf::from("/moved/checkout"))
        );
    }
}

#[test]
fn unsupported_locations_do_not_search_ancestor_installations() {
    for executable in [
        "/opt/FailForge/failforge",
        "/opt/FailForge/bin/other/failforge",
        "/checkout/build/debug/failforge",
        "/checkout/target/custom/failforge",
        "/checkout/target/aarch64-apple-darwin/release/failforge",
        "/checkout/target/debug/deps/failforge-test",
        "/failforge",
        "/",
        "bin/failforge",
        "",
    ] {
        assert_eq!(from_executable(Path::new(executable)), None, "{executable}");
    }
}

#[test]
fn explicit_paths_are_preserved_for_context_resolution() {
    for path in ["/custom/install", "../custom install", ".", "/missing"] {
        assert_eq!(resolve(Some(Path::new(path))).unwrap(), PathBuf::from(path));
    }
}

#[test]
fn installation_names_need_not_be_unicode() {
    let root = PathBuf::from(OsString::from_vec(b"/opt/install-\xff".to_vec()));
    assert_eq!(
        from_executable(&root.join("bin/failforge")),
        Some(root.clone())
    );
    assert_eq!(
        from_executable(&root.join("target/debug/failforge")),
        Some(root.clone())
    );
    assert_eq!(resolve(Some(&root)).unwrap(), root);
}
