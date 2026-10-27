use crate::common::{self, Result, ensure};
use serde_json::Value;
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::time::Duration;
pub const VERSION: &str = "3.47.7";
const DOWNLOAD_BASE: &str = "https://storage.googleapis.com/flutter_infra_release/releases";
fn stage<T>(result: Result<T>, operation: &str) -> Result<T> {
    result.map_err(|error| format!("Flutter SDK {operation}: {error}").into())
}

#[cfg(unix)]
fn publish_directory(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result = -1;
    ensure(
        result == 0,
        format!(
            "cannot publish SDK without replacement: {}",
            std::io::Error::last_os_error()
        ),
    )
}
#[cfg(windows)]
fn publish_directory(source: &Path, destination: &Path) -> Result<()> {
    common::publish_directory(source, destination)
}
fn platform(os: &str, arch: &str) -> Result<(&'static str, &'static str)> {
    let os = match os {
        "macos" => "macos",
        "linux" => "linux",
        "windows" => "windows",
        _ => return Err("test SDK installer supports macOS, Linux and Windows".into()),
    };
    let arch = match arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x64",
        _ => return Err("unsupported architecture".into()),
    };
    Ok((os, arch))
}
fn select<'a>(manifest: &'a Value, arch: &str, version: &str) -> Result<&'a Value> {
    ensure(
        version == VERSION || version == "stable",
        "test SDK version must be the CI pin or stable",
    )?;
    ensure(
        manifest["base_url"].as_str() == Some(DOWNLOAD_BASE),
        "unexpected Flutter download host",
    )?;
    let stable_hash = if version == "stable" {
        Some(
            manifest["current_release"]["stable"]
                .as_str()
                .filter(|hash| !hash.is_empty())
                .ok_or("missing current stable Flutter release")?,
        )
    } else {
        None
    };
    let releases = manifest["releases"].as_array().ok_or("missing releases")?;
    let entries: Vec<_> = releases
        .iter()
        .filter(|r| {
            stable_hash.map_or_else(
                || r["version"] == VERSION,
                |hash| r["hash"].as_str() == Some(hash),
            ) && r["channel"] == "stable"
                && r["dart_sdk_arch"].as_str().unwrap_or("x64") == arch
        })
        .collect();
    ensure(
        entries.len() == 1,
        "requested test SDK unavailable or ambiguous",
    )?;
    let entry = entries[0];
    let archive = entry["archive"].as_str().ok_or("missing archive")?;
    ensure(
        !archive.is_empty()
            && !archive.contains('\\')
            && Path::new(archive)
                .components()
                .all(|p| matches!(p, Component::Normal(_))),
        "unsafe archive path",
    )?;
    ensure(
        archive.ends_with(".zip") || archive.ends_with(".tar.xz"),
        "unexpected archive format",
    )?;
    let hash = entry["sha256"].as_str().ok_or("missing archive SHA-256")?;
    ensure(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid archive SHA-256",
    )?;
    ensure(
        entry["dart_sdk_version"].as_str().is_some(),
        "missing Dart SDK version",
    )?;
    Ok(entry)
}
pub fn install(destination: &Path, version: &str) -> Result<()> {
    let (os, arch) = platform(std::env::consts::OS, std::env::consts::ARCH)?;
    fs::create_dir_all(destination)?;
    ensure(
        fs::symlink_metadata(destination.join("flutter"))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "destination already contains Flutter; refusing to replace it",
    )?;
    let temp = common::Temporary::new("ff-sdk")?;
    let manifest_file = temp.0.join("releases.json");
    let mut curl = common::command(Path::new("curl"));
    curl.args([
        "--fail",
        "--location",
        "--retry",
        "3",
        "--max-time",
        "60",
        "--output",
    ])
    .arg(&manifest_file)
    .arg(format!(
        "https://storage.googleapis.com/flutter_infra_release/releases/releases_{os}.json"
    ));
    stage(
        common::execute(
            &mut curl,
            &temp.0.join("manifest"),
            Duration::from_secs(240),
            0,
        ),
        "manifest download",
    )?;
    let manifest = common::read_json(&manifest_file)?;
    let entry = select(&manifest, arch, version)?;
    let name = entry["archive"].as_str().unwrap();
    let archive = temp.0.join(Path::new(name).file_name().unwrap());
    let mut download = common::command(Path::new("curl"));
    download
        .args([
            "--fail",
            "--location",
            "--retry",
            "3",
            "--max-time",
            "1200",
            "--output",
        ])
        .arg(&archive)
        .arg(format!("{}/{name}", manifest["base_url"].as_str().unwrap()));
    stage(
        common::execute(
            &mut download,
            &temp.0.join("download"),
            Duration::from_secs(4800),
            0,
        ),
        "archive download",
    )?;
    ensure(
        stage(common::hash(&archive), "archive verification")? == entry["sha256"],
        "Flutter archive digest mismatch",
    )?;
    // Extraction happens in a private staging directory; a failed extraction never
    // leaves a partial SDK in the requested installation directory.
    let staging = destination.join(format!(".ff-sdk-{}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        let mut extract = common::command(Path::new(if cfg!(windows) {
            "tar.exe"
        } else if name.ends_with(".zip") {
            "unzip"
        } else {
            "tar"
        }));
        if name.ends_with(".zip") && !cfg!(windows) {
            extract.arg("-q").arg(&archive).arg("-d").arg(&staging);
        } else {
            extract.arg("-xf").arg(&archive).arg("-C").arg(&staging);
        }
        stage(
            common::execute(
                &mut extract,
                &temp.0.join("extract"),
                Duration::from_secs(600),
                0,
            ),
            "archive extraction",
        )?;
        ensure(
            staging
                .join(if cfg!(windows) {
                    "flutter/bin/cache/dart-sdk/bin/dart.exe"
                } else {
                    "flutter/bin/flutter"
                })
                .is_file(),
            "archive does not contain Flutter executable",
        )?;
        ensure(
            fs::symlink_metadata(destination.join("flutter"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "destination Flutter appeared during installation",
        )?;
        stage(
            publish_directory(&staging.join("flutter"), &destination.join("flutter")),
            "atomic publication",
        )?;
        Ok(())
    })();
    let cleanup = if common::cleanup_confirmed() {
        fs::remove_dir_all(&staging)
    } else {
        Ok(())
    };
    result?;
    stage(cleanup.map_err(Into::into), "staging cleanup")?;
    println!(
        "{}",
        serde_json::json!({"version":entry["version"],"dartVersion":entry["dart_sdk_version"],"archive":entry["archive"],"sha256":entry["sha256"]})
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn manifest() -> Value {
        json!({"base_url":"https://storage.googleapis.com/flutter_infra_release/releases","releases":[{"version":VERSION,"channel":"stable","dart_sdk_arch":"arm64","archive":"stable/macos/flutter_macos_arm64.zip","sha256":"a".repeat(64),"dart_sdk_version":"3.13.0"}]})
    }
    #[test]
    fn host_mapping() {
        assert_eq!(platform("macos", "aarch64").unwrap(), ("macos", "arm64"));
        assert_eq!(platform("linux", "x86_64").unwrap(), ("linux", "x64"));
        assert_eq!(platform("windows", "x86_64").unwrap(), ("windows", "x64"));
        assert!(platform("linux", "mips").is_err());
    }
    #[test]
    fn pin_arch_channel_are_exact() {
        assert!(select(&manifest(), "arm64", VERSION).is_ok());
        assert!(select(&manifest(), "x64", VERSION).is_err());
        for field in ["version", "channel"] {
            let mut m = manifest();
            m["releases"][0][field] = "other".into();
            assert!(select(&m, "arm64", VERSION).is_err());
        }
    }
    #[test]
    fn duplicate_pin_is_rejected() {
        let mut m = manifest();
        let row = m["releases"][0].clone();
        m["releases"].as_array_mut().unwrap().push(row);
        assert!(select(&m, "arm64", VERSION).is_err());
    }
    #[test]
    fn stable_follows_current_release_hash_instead_of_version_or_order() {
        let mut m = manifest();
        let mut latest = m["releases"][0].clone();
        latest["version"] = "3.50.0".into();
        latest["hash"] = "current-stable-hash".into();
        m["current_release"] = json!({"stable": "current-stable-hash"});
        m["releases"].as_array_mut().unwrap().push(latest);
        assert_eq!(select(&m, "arm64", "stable").unwrap()["version"], "3.50.0");
        assert_eq!(select(&m, "arm64", VERSION).unwrap()["version"], VERSION);
        m["releases"].as_array_mut().unwrap().reverse();
        assert_eq!(select(&m, "arm64", "stable").unwrap()["version"], "3.50.0");
    }
    #[test]
    fn stable_requires_one_matching_hash_channel_and_architecture() {
        let mut m = manifest();
        m["current_release"] = json!({"stable": "current-stable-hash"});
        m["releases"][0]["hash"] = "current-stable-hash".into();
        assert!(select(&m, "arm64", "stable").is_ok());
        assert!(select(&m, "x64", "stable").is_err());
        for field in ["hash", "channel", "dart_sdk_arch"] {
            let mut mismatch = m.clone();
            mismatch["releases"][0][field] = "other".into();
            assert!(select(&mismatch, "arm64", "stable").is_err(), "{field}");
        }
        let row = m["releases"][0].clone();
        m["releases"].as_array_mut().unwrap().push(row);
        assert!(select(&m, "arm64", "stable").is_err());
    }
    #[test]
    fn stable_does_not_fall_back_when_current_release_is_missing() {
        for pointer in [Value::Null, json!({}), json!({"stable": ""})] {
            let mut m = manifest();
            m["current_release"] = pointer;
            assert!(select(&m, "arm64", "stable").is_err());
        }
        assert!(select(&manifest(), "arm64", "beta").is_err());
    }
    #[test]
    fn official_linux_manifest_selects_the_pinned_archive_url() {
        let m = json!({
            "base_url":"https://storage.googleapis.com/flutter_infra_release/releases",
            "releases":[{
                "version":"3.47.7","channel":"stable","dart_sdk_arch":"x64",
                "archive":"stable/linux/flutter_linux_3.47.7-stable.tar.xz",
                "sha256":"af7455f540df951184c7953fbe3cf884766e7541dacf8571f9f582db03dd6230",
                "dart_sdk_version":"3.13.5"
            }]
        });
        let entry = select(&m, "x64", VERSION).unwrap();
        assert_eq!(
            format!(
                "{}/{}",
                m["base_url"].as_str().unwrap(),
                entry["archive"].as_str().unwrap()
            ),
            "https://storage.googleapis.com/flutter_infra_release/releases/stable/linux/flutter_linux_3.47.7-stable.tar.xz"
        );
        assert_eq!(entry["dart_sdk_version"], "3.13.5");
    }
    #[test]
    fn remote_paths_and_hash_are_validated() {
        for path in [
            "../archive.zip",
            "/archive.zip",
            "a/../../archive.zip",
            "a\\b.zip",
            "bad.exe",
        ] {
            let mut m = manifest();
            m["releases"][0]["archive"] = path.into();
            assert!(select(&m, "arm64", VERSION).is_err());
        }
        let mut m = manifest();
        m["base_url"] = "https://other.example".into();
        assert!(select(&m, "arm64", VERSION).is_err());
        let mut m = manifest();
        m["releases"][0]["sha256"] = "invalid".into();
        assert!(select(&m, "arm64", VERSION).is_err());
    }
    #[test]
    fn concurrent_destination_cannot_be_replaced_even_when_empty() {
        let t = common::Temporary::new("ff-sdk-publish").unwrap();
        let source = t.0.join("staging");
        let destination = t.0.join("flutter");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("owned"), b"SDK").unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(publish_directory(&source, &destination).is_err());
        assert!(!destination.join("owned").exists());
        assert!(source.join("owned").is_file());
        fs::remove_dir(&destination).unwrap();
        publish_directory(&source, &destination).unwrap();
        assert!(destination.join("owned").is_file());
    }
}
