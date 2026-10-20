use crate::common::{self, Result, ensure};
use serde_json::Value;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};
use std::time::Duration;
const VERSION: &str = "3.47.7";
const DOWNLOAD_BASE: &str = "https://storage.googleapis.com/flutter_infra_release/releases";

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
fn platform(os: &str, arch: &str) -> Result<(&'static str, &'static str)> {
    let os = match os {
        "macos" => "macos",
        "linux" => "linux",
        _ => return Err("test SDK installer supports macOS and Linux".into()),
    };
    let arch = match arch {
        "aarch64" | "arm64" => "arm64",
        "x86_64" => "x64",
        _ => return Err("unsupported architecture".into()),
    };
    Ok((os, arch))
}
fn select<'a>(manifest: &'a Value, arch: &str) -> Result<&'a Value> {
    ensure(
        manifest["base_url"].as_str() == Some(DOWNLOAD_BASE),
        "unexpected Flutter download host",
    )?;
    let releases = manifest["releases"].as_array().ok_or("missing releases")?;
    let entries: Vec<_> = releases
        .iter()
        .filter(|r| {
            r["version"] == VERSION
                && r["channel"] == "stable"
                && r["dart_sdk_arch"].as_str().unwrap_or("x64") == arch
        })
        .collect();
    ensure(
        entries.len() == 1,
        "pinned test SDK unavailable or ambiguous",
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
pub fn install(destination: &Path) -> Result<()> {
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
    common::execute(
        &mut curl,
        &temp.0.join("manifest"),
        Duration::from_secs(240),
        0,
    )?;
    let manifest = common::read_json(&manifest_file)?;
    let entry = select(&manifest, arch)?;
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
    common::execute(
        &mut download,
        &temp.0.join("download"),
        Duration::from_secs(4800),
        0,
    )?;
    ensure(
        common::hash(&archive)? == entry["sha256"],
        "Flutter archive digest mismatch",
    )?;
    // Extraction happens in a private staging directory; a failed extraction never
    // leaves a partial SDK in the requested installation directory.
    let staging = destination.join(format!(".ff-sdk-{}", std::process::id()));
    fs::create_dir(&staging)?;
    let result = (|| -> Result<()> {
        let mut extract = common::command(Path::new(if name.ends_with(".zip") {
            "unzip"
        } else {
            "tar"
        }));
        if name.ends_with(".zip") {
            extract.arg("-q").arg(&archive).arg("-d").arg(&staging);
        } else {
            extract.arg("-xf").arg(&archive).arg("-C").arg(&staging);
        }
        common::execute(
            &mut extract,
            &temp.0.join("extract"),
            Duration::from_secs(600),
            0,
        )?;
        ensure(
            staging.join("flutter/bin/flutter").is_file(),
            "archive does not contain Flutter executable",
        )?;
        ensure(
            fs::symlink_metadata(destination.join("flutter"))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "destination Flutter appeared during installation",
        )?;
        publish_directory(&staging.join("flutter"), &destination.join("flutter"))?;
        Ok(())
    })();
    let cleanup = if common::cleanup_confirmed() {
        fs::remove_dir_all(&staging)
    } else {
        Ok(())
    };
    result?;
    cleanup?;
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
        assert!(platform("windows", "x86_64").is_err());
        assert!(platform("linux", "mips").is_err());
    }
    #[test]
    fn pin_arch_channel_are_exact() {
        assert!(select(&manifest(), "arm64").is_ok());
        assert!(select(&manifest(), "x64").is_err());
        for field in ["version", "channel"] {
            let mut m = manifest();
            m["releases"][0][field] = "other".into();
            assert!(select(&m, "arm64").is_err());
        }
    }
    #[test]
    fn duplicate_pin_is_rejected() {
        let mut m = manifest();
        let row = m["releases"][0].clone();
        m["releases"].as_array_mut().unwrap().push(row);
        assert!(select(&m, "arm64").is_err());
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
        let entry = select(&m, "x64").unwrap();
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
            assert!(select(&m, "arm64").is_err());
        }
        let mut m = manifest();
        m["base_url"] = "https://other.example".into();
        assert!(select(&m, "arm64").is_err());
        let mut m = manifest();
        m["releases"][0]["sha256"] = "invalid".into();
        assert!(select(&m, "arm64").is_err());
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
