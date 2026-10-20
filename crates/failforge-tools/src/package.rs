//! Build and package the native host without copying development state.
#[path = "package_assets.rs"]
mod assets;

use crate::common::{self, Result, Temporary, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const BINARIES: &[&str] = &["failforge", "failforge-adapter-campaign"];

#[derive(Args)]
pub struct Options {
    /// Source checkout to build; defaults to the developer tool's checkout.
    #[arg(long)]
    installation: Option<PathBuf>,
    /// New directory for the archive, manifest and SHA256SUMS; never overwritten.
    #[arg(long)]
    output: PathBuf,
    /// Require this exact freshly built CLI version; never relabel a binary.
    #[arg(long)]
    expected_version: Option<String>,
}

fn invoke(command: &mut Command, logs: &Path, name: &str, seconds: u64) -> Result<Vec<u8>> {
    let directory = logs.join(name);
    if let Err(error) = common::execute(command, &directory, Duration::from_secs(seconds), 0) {
        let stderr = fs::read(directory.join("stderr.log")).unwrap_or_default();
        let tail = &stderr[stderr.len().saturating_sub(4096)..];
        return Err(format!("{error}: {}", String::from_utf8_lossy(tail)).into());
    }
    ensure(
        fs::metadata(directory.join("stdout.log"))?.len() <= 64 * 1024 * 1024,
        "packaging command output exceeds 64 MiB",
    )?;
    Ok(fs::read(directory.join("stdout.log"))?)
}

fn git(root: &Path, logs: &Path, name: &str, arguments: &[&str]) -> Result<Vec<u8>> {
    invoke(
        common::command(Path::new("git"))
            .args(["-c", "core.fsmonitor=false", "-C"])
            .arg(root)
            .args(arguments),
        logs,
        name,
        60,
    )
}

fn tracked_paths(bytes: &[u8]) -> Result<Vec<String>> {
    let paths = bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8(p.to_vec()).map_err(Into::into))
        .collect::<Result<Vec<_>>>()?;
    ensure(
        !paths.is_empty() && paths.iter().all(|p| assets::safe_relative(p)),
        "invalid tracked file inventory",
    )?;
    Ok(paths)
}

fn host(verbose: &str, os: &str, arch: &str) -> Result<String> {
    let values: Vec<_> = verbose
        .lines()
        .filter_map(|l| l.strip_prefix("host: "))
        .collect();
    ensure(values.len() == 1, "rustc did not identify one host target")?;
    let target = values[0];
    ensure(
        target
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid Rust host target",
    )?;
    let architecture = match arch {
        "aarch64" => "aarch64-",
        "x86_64" => "x86_64-",
        _ => return Err("packaging supports aarch64 and x86_64 hosts".into()),
    };
    let platform = match os {
        "macos" => "-apple-darwin",
        "linux" => "-unknown-linux-",
        _ => return Err("packaging supports Linux and macOS".into()),
    };
    ensure(
        target.starts_with(architecture) && target.contains(platform),
        "selected Rust toolchain does not match the native host",
    )?;
    Ok(target.into())
}

fn version(stdout: &[u8], expected: Option<&str>) -> Result<String> {
    let text = std::str::from_utf8(stdout)?.trim();
    let value = text
        .strip_prefix("failforge ")
        .ok_or("unexpected CLI version response")?;
    ensure(
        !value.is_empty()
            && value.len() <= 100
            && value.starts_with(|c: char| c.is_ascii_digit())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)),
        "unsafe CLI version",
    )?;
    ensure(
        expected.is_none_or(|expected| expected == value),
        "built CLI does not match --expected-version",
    )?;
    Ok(value.into())
}

fn artifacts(raw: &[u8], cache: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut result = BTreeMap::new();
    for line in raw.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
        let message: Value = serde_json::from_slice(line)?;
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(name) = message["target"]["name"]
            .as_str()
            .filter(|name| BINARIES.contains(name))
        else {
            continue;
        };
        let Some(executable) = message["executable"].as_str() else {
            continue;
        };
        ensure(
            message["profile"]["opt_level"] == "3"
                && message["profile"]["test"] == false
                && message["profile"]["debug_assertions"] == false
                && message["profile"]["overflow_checks"] == false,
            "product binaries must use the optimized ordinary build profile",
        )?;
        ensure(
            fs::symlink_metadata(executable)?.is_file(),
            "Cargo executable cannot be an alias or special file",
        )?;
        let path = PathBuf::from(executable).canonicalize()?;
        ensure(
            path.starts_with(cache) && path.file_name().is_some_and(|value| value == name),
            "unexpected Cargo executable location",
        )?;
        ensure(
            result.insert(name.to_owned(), path).is_none(),
            "duplicate Cargo executable artifact",
        )?;
    }
    ensure(
        result.len() == BINARIES.len(),
        "Cargo did not produce both product executables",
    )?;
    Ok(result)
}

fn binary_format(path: &Path, target: &str) -> Result<()> {
    let parent = path.parent().ok_or("missing binary parent")?;
    let mut file = assets::regular(
        parent,
        path.file_name()
            .and_then(|n| n.to_str())
            .ok_or("invalid binary name")?,
    )?;
    let mut header = [0; 32];
    file.read_exact(&mut header)?;
    let arm = target.starts_with("aarch64-");
    let valid = if target.contains("-apple-darwin") {
        header[..4] == [0xcf, 0xfa, 0xed, 0xfe]
            && u32::from_le_bytes(header[4..8].try_into()?)
                == if arm { 0x0100000c } else { 0x01000007 }
    } else {
        header[..6] == [0x7f, b'E', b'L', b'F', 2, 1]
            && u16::from_le_bytes(header[18..20].try_into()?) == if arm { 183 } else { 62 }
    };
    ensure(
        valid,
        "built executable format/architecture does not match the native target",
    )
}

fn publish(source: &Path, output: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let output = CString::new(output.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renamex_np(source.as_ptr(), output.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            output.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let status = -1;
    ensure(
        status == 0,
        format!(
            "cannot publish package without replacing output: {}",
            std::io::Error::last_os_error()
        ),
    )
}

fn fresh_output(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    match fs::symlink_metadata(&absolute) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err("package output must not already exist".into()),
    }
    let parent = absolute.parent().ok_or("package output needs a parent")?;
    fs::create_dir_all(parent)?;
    Ok(parent
        .canonicalize()?
        .join(absolute.file_name().ok_or("invalid output name")?))
}

pub fn run(options: Options) -> Result<()> {
    let root = options
        .installation
        .unwrap_or_else(common::root)
        .canonicalize()?;
    let output = fresh_output(&options.output)?;
    let logs = Temporary::new("ff-package-commands")?;
    let top = String::from_utf8(git(
        &root,
        &logs.0,
        "git-root",
        &["rev-parse", "--show-toplevel"],
    )?)?;
    ensure(
        Path::new(top.trim()).canonicalize()? == root,
        "--installation must identify the repository root",
    )?;
    let revision = String::from_utf8(git(&root, &logs.0, "git-revision", &["rev-parse", "HEAD"])?)?
        .trim()
        .to_owned();
    ensure(
        [40, 64].contains(&revision.len()) && revision.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Git revision",
    )?;
    let dirty = !git(
        &root,
        &logs.0,
        "git-status",
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?
    .is_empty();
    let tracked = tracked_paths(&git(&root, &logs.0, "git-files", &["ls-files", "-z"])?)?;
    let selected = assets::select(&tracked)?;
    assets::validate_notices(&root, &selected)?;
    let bound: std::collections::BTreeSet<_> =
        tracked.iter().chain(selected.iter()).cloned().collect();
    let before = assets::snapshot(&root, &bound)?;
    let tracked_hash = assets::tree_digest(&assets::snapshot(&root, &tracked)?)?;
    let verbose = String::from_utf8(invoke(
        common::command(Path::new("rustc"))
            .arg("-vV")
            .current_dir(&root),
        &logs.0,
        "rustc",
        60,
    )?)?;
    let target = host(&verbose, std::env::consts::OS, std::env::consts::ARCH)?;
    let sysroot = String::from_utf8(invoke(
        common::command(Path::new("rustc"))
            .args(["--print", "sysroot"])
            .current_dir(&root),
        &logs.0,
        "sysroot",
        60,
    )?)?;
    let sysroot = Path::new(sysroot.trim()).canonicalize()?;
    for (relative, _) in assets::RUNTIME {
        assets::regular(&sysroot, relative).map_err(|error| format!("missing Rust runtime notice {relative}; prepare rust-docs and rust-src for the selected Rust toolchain: {error}"))?;
    }
    let runtime_before = assets::snapshot(&sysroot, assets::RUNTIME.iter().map(|(p, _)| *p))?;
    let cargo_version = String::from_utf8(invoke(
        common::command(Path::new("cargo"))
            .arg("--version")
            .current_dir(&root),
        &logs.0,
        "cargo-version",
        60,
    )?)?;
    for relative in ["target", "target/package"] {
        let path = root.join(relative);
        if path.exists() {
            ensure(
                fs::symlink_metadata(&path)?.is_dir(),
                "packaging build cache cannot be an alias",
            )?;
        } else {
            fs::create_dir(&path)?;
        }
    }
    let cache = root.join("target/package").canonicalize()?;
    eprintln!("failforge-tools: building optimized product executables for {target}");
    let build = invoke(
        common::command(Path::new("cargo"))
            .current_dir(&root)
            .env("RUSTC", "rustc")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .args(["build", "--locked", "--target"])
            .arg(&target)
            .arg("--target-dir")
            .arg(&cache)
            .args([
                "--package",
                "failforge-cli",
                "--package",
                "failforge-dart",
                "--bin",
                "failforge",
                "--bin",
                "failforge-adapter-campaign",
                "--message-format=json-render-diagnostics",
            ]),
        &logs.0,
        "build",
        3600,
    )?;
    let executables = artifacts(&build, &cache)?;
    for executable in executables.values() {
        binary_format(executable, &target)?;
    }
    let binary_hashes: BTreeMap<_, _> = executables
        .iter()
        .map(|(name, path)| Ok((name.clone(), common::hash(path)?)))
        .collect::<Result<_>>()?;
    let version = version(
        &invoke(
            common::command(&executables["failforge"]).arg("--version"),
            &logs.0,
            "cli-version",
            30,
        )?,
        options.expected_version.as_deref(),
    )?;
    ensure(
        before == assets::snapshot(&root, &bound)?,
        "source inputs changed during package build",
    )?;
    let archive_root = format!("failforge-{version}-{target}");
    let archive = format!("{archive_root}.tar.gz");
    let staging = Temporary::new_in(
        output.parent().ok_or("missing output parent")?,
        ".ff-package",
    )?;
    let payload = staging.0.join(&archive_root);
    let distribution = staging.0.join("distribution");
    fs::create_dir(&payload)?;
    fs::create_dir(&distribution)?;
    let mut files = vec![];
    for source in &selected {
        let file = assets::copy(&root, source, &payload, source, false)?;
        ensure(
            file["sha256"] == before[source],
            "copied source differs from bound build inputs",
        )?;
        files.push(file);
    }
    for (name, executable) in &executables {
        let file = assets::copy(
            executable.parent().unwrap(),
            name,
            &payload,
            &format!("bin/{name}"),
            true,
        )?;
        ensure(
            file["sha256"] == binary_hashes[name],
            "product executable changed during packaging",
        )?;
        files.push(file);
    }
    let (runtime_files, runtime_notices) = assets::runtime(&sysroot, &payload)?;
    for notice in &runtime_notices {
        ensure(
            notice["sha256"] == runtime_before[notice["sourcePath"].as_str().unwrap()],
            "Rust runtime notice changed during packaging",
        )?;
    }
    files.extend(runtime_files);
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    let manifest = json!({"schemaVersion":1,"archive":archive,"archiveRoot":archive_root,"version":version,"target":target,
        "source":{"revision":revision,"dirty":dirty,"trackedTreeSha256":tracked_hash},
        "build":{"profile":"dev","optLevel":"3","rustcVerbose":verbose.trim(),"cargoVersion":cargo_version.trim()},
        "files":files,"runtimeNotices":runtime_notices,
        "runtimeNoticeScope":"Conservative notices from the selected Rust toolchain; does not assert every listed component is linked. Dart/Flutter SDKs, pub dependencies and system libraries are not bundled.",
        "fileManifestScope":"Every payload file except package-manifest.json itself; the external SHA256SUMS covers the archive and this manifest."});
    common::write_json(&payload.join("package-manifest.json"), &manifest)?;
    common::write_json(&distribution.join("package-manifest.json"), &manifest)?;
    let archive_path = distribution.join(&archive);
    invoke(
        common::command(Path::new("tar"))
            .env("COPYFILE_DISABLE", "1")
            .args(["--no-xattrs", "--no-acls", "-czf"])
            .arg(&archive_path)
            .arg("-C")
            .arg(&staging.0)
            .arg(&archive_root),
        &logs.0,
        "archive",
        300,
    )?;
    ensure(
        before == assets::snapshot(&root, &bound)?,
        "source inputs changed during package assembly",
    )?;
    let after_revision =
        String::from_utf8(git(&root, &logs.0, "git-final", &["rev-parse", "HEAD"])?)?;
    ensure(
        after_revision.trim() == revision,
        "Git revision changed during packaging",
    )?;
    let sums = format!(
        "{}  {}\n{}  package-manifest.json\n",
        common::hash(&archive_path)?,
        archive,
        common::hash(&distribution.join("package-manifest.json"))?
    );
    fs::write(distribution.join("SHA256SUMS"), sums)?;
    common::check_interrupt()?;
    publish(&distribution, &output)?;
    println!("{}", output.join(archive).display());
    Ok(())
}

#[cfg(test)]
#[path = "package_tests.rs"]
mod tests;
