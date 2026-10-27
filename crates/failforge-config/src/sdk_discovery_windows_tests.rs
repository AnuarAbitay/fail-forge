use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "ff Windows SDK {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn sdk(&self, relative: &str) -> PathBuf {
        let root = self.0.join(relative);
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("lib/core")).unwrap();
        fs::write(
            root.join("bin/dart.exe"),
            "metadata fixture; must never execute",
        )
        .unwrap();
        fs::write(root.join("lib/core/core.dart"), "library dart.core;").unwrap();
        root
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn windows_path_order_and_exe_discovery_do_not_execute_candidates() {
    let f = Fixture::new();
    let first = f.sdk("first SDK");
    let second = f.sdk("second SDK");
    let search = std::env::join_paths([first.join("bin"), second.join("bin")]).unwrap();
    assert_eq!(
        discover_path_sdk(&search, &f.0).unwrap().dart_sdk,
        first.canonicalize().unwrap()
    );
}

#[test]
fn flutter_bat_selects_its_bundled_dart_exe() {
    let f = Fixture::new();
    let sdk = f.sdk("flutter/bin/cache/dart-sdk");
    fs::write(f.0.join("flutter/bin/flutter.bat"), "@exit /b 99\r\n").unwrap();
    fs::write(f.0.join("flutter/bin/dart.bat"), "@exit /b 99\r\n").unwrap();
    let search = std::env::join_paths([f.0.join("flutter/bin")]).unwrap();
    let found = discover_path_sdk(&search, &f.0).unwrap();
    assert_eq!(found.dart_sdk, sdk.canonicalize().unwrap());
    assert_eq!(
        found.flutter_sdk,
        Some(f.0.join("flutter").canonicalize().unwrap())
    );
}

#[test]
fn opaque_bat_shims_do_not_hide_a_real_sdk() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("shims")).unwrap();
    fs::write(f.0.join("shims/dart.bat"), "@exit /b 99\r\n").unwrap();
    let sdk = f.sdk("prepared");
    let search = std::env::join_paths([f.0.join("shims"), sdk.join("bin")]).unwrap();
    assert_eq!(
        discover_path_sdk(&search, &f.0).unwrap().dart_sdk,
        sdk.canonicalize().unwrap()
    );
}

#[test]
fn unix_style_extensionless_launchers_are_not_windows_sdks() {
    let f = Fixture::new();
    let sdk = f.sdk("invalid");
    fs::rename(sdk.join("bin/dart.exe"), sdk.join("bin/dart")).unwrap();
    let search = std::env::join_paths([sdk.join("bin")]).unwrap();
    assert!(discover_path_sdk(&search, &f.0).is_none());
}
