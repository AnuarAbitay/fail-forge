//! Build and package the native host without copying development state.
#[path = "package_assets.rs"]
mod assets;

use crate::common::{self, Result, Temporary, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
#[cfg(unix)]
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
        "windows" => "-pc-windows-msvc",
        _ => return Err("packaging supports Linux, macOS and Windows MSVC".into()),
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
            path.starts_with(cache)
                && path.file_name().is_some_and(|value| {
                    value == failforge_config::platform::binary_name(name).as_ref()
                }),
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

/// Lower-case names of every DLL a PE32+ image imports, at load time or
/// delay-loaded on first use.
fn pe_imports(image: &[u8]) -> Result<Vec<String>> {
    let bytes = |offset: usize, length: usize| -> Result<&[u8]> {
        offset
            .checked_add(length)
            .and_then(|end| image.get(offset..end))
            .ok_or_else(|| "truncated PE import table".into())
    };
    let u16_at =
        |offset| -> Result<usize> { Ok(u16::from_le_bytes(bytes(offset, 2)?.try_into()?).into()) };
    let u32_at = |offset| -> Result<usize> {
        Ok(u32::from_le_bytes(bytes(offset, 4)?.try_into()?).try_into()?)
    };
    let coff = u32_at(0x3c)? + 4;
    let optional = coff + 20;
    let optional_size = u16_at(coff + 16)?;
    // PE32+ keeps NumberOfRvaAndSizes at 108 and 8-byte directories from 112.
    let directories = if optional_size < 112 {
        0
    } else {
        u32_at(optional + 108)?.min((optional_size - 112) / 8)
    };
    let directory = |index: usize| -> Result<usize> {
        if index < directories {
            u32_at(optional + 112 + 8 * index)
        } else {
            Ok(0)
        }
    };
    // Entry 1 holds 20-byte import descriptors with the name RVA at 12;
    // entry 13 holds 32-byte delay-load descriptors with the name RVA at 4.
    let (import, delayed) = (directory(1)?, directory(13)?);
    if import == 0 && delayed == 0 {
        return Ok(Vec::new());
    }
    // (RVA, mapped size, bytes stored in the file, file offset) per section.
    // A zero VirtualSize means the raw size; only stored bytes can be read.
    let sections = (0..u16_at(coff + 2)?)
        .map(|index| {
            let header = optional + optional_size + 40 * index;
            let stored = u32_at(header + 16)?;
            let mapped = match u32_at(header + 8)? {
                0 => stored,
                size => size,
            };
            Ok((u32_at(header + 12)?, mapped, stored, u32_at(header + 20)?))
        })
        .collect::<Result<Vec<_>>>()?;
    let file_offset = |rva: usize| -> Result<usize> {
        sections
            .iter()
            .find(|(address, mapped, ..)| (*address..address + mapped).contains(&rva))
            .filter(|(address, _, stored, _)| rva - address < *stored)
            .map(|(address, _, _, raw)| raw + (rva - address))
            .ok_or_else(|| "PE import RVA is outside the stored section data".into())
    };
    let mut names = Vec::new();
    for (table, size, name_at) in [(import, 20, 12), (delayed, 32, 4)] {
        if table == 0 {
            continue;
        }
        let table = file_offset(table)?;
        for descriptor in (0..).map(|index| table + size * index) {
            if bytes(descriptor, size)?.iter().all(|byte| *byte == 0) {
                break;
            }
            // Delay-load attribute bit 0 marks RVAs; the old form uses VAs.
            if size == 32 && u32_at(descriptor)? & 1 == 0 {
                return Err("unsupported delay-load descriptor without RVAs".into());
            }
            let name = file_offset(u32_at(descriptor + name_at)?)?;
            let tail = image.get(name..).ok_or("truncated PE import name")?;
            let end = tail
                .iter()
                .take(256)
                .position(|byte| *byte == 0)
                .ok_or("unterminated PE import name")?;
            names.push(String::from_utf8(tail[..end].to_vec())?.to_ascii_lowercase());
        }
    }
    Ok(names)
}

/// DLLs of the redistributable Visual C++ runtime that an image imports.
///
/// Release executables link the CRT statically (see `.cargo/config.toml`).
/// Without the redistributable, a dynamic import ends the process with
/// STATUS_DLL_NOT_FOUND before `main` can report anything.
fn dynamic_crt_imports(image: &[u8]) -> Result<Vec<String>> {
    Ok(pe_imports(image)?
        .into_iter()
        .filter(|name| {
            // msvcr1xx are the pre-2015 runtimes; msvcrt.dll ships with Windows.
            [
                "vcruntime",
                "msvcp",
                "msvcr1",
                "concrt",
                "vccorlib",
                "vcomp",
                "api-ms-win-crt-",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
                || name == "ucrtbase.dll"
        })
        .collect())
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
    } else if target.contains("-pc-windows-") {
        if header[..2] != *b"MZ" {
            false
        } else {
            file.seek(SeekFrom::Start(0x3c))?;
            let mut offset = [0; 4];
            file.read_exact(&mut offset)?;
            file.seek(SeekFrom::Start(u32::from_le_bytes(offset).into()))?;
            let mut pe = [0; 26];
            file.read_exact(&mut pe)?;
            pe[..4] == *b"PE\0\0"
                && u16::from_le_bytes(pe[4..6].try_into()?) == if arm { 0xaa64 } else { 0x8664 }
                && u16::from_le_bytes(pe[24..26].try_into()?) == 0x20b
        }
    } else {
        header[..6] == [0x7f, b'E', b'L', b'F', 2, 1]
            && u16::from_le_bytes(header[18..20].try_into()?) == if arm { 183 } else { 62 }
    };
    ensure(
        valid,
        "built executable format/architecture does not match the native target",
    )?;
    if target.contains("-pc-windows-") {
        let mut image = Vec::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut image)?;
        let runtime = dynamic_crt_imports(&image)?;
        ensure(
            runtime.is_empty(),
            format!(
                "built executable imports the dynamic Visual C++ runtime: {}",
                runtime.join(", ")
            ),
        )?;
    }
    Ok(())
}

#[cfg(unix)]
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

#[cfg(windows)]
fn publish(source: &Path, output: &Path) -> Result<()> {
    common::publish_directory(source, output)
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
            failforge_config::platform::binary_name(name).as_ref(),
            &payload,
            &format!("bin/{}", failforge_config::platform::binary_name(name)),
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

#[cfg(all(test, unix))]
#[path = "package_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn native_windows_target_is_not_mislabeled_as_another_platform() {
        assert_eq!(
            host("host: x86_64-pc-windows-msvc\n", "windows", "x86_64").unwrap(),
            "x86_64-pc-windows-msvc"
        );
        assert!(host("host: x86_64-unknown-linux-gnu\n", "windows", "x86_64").is_err());
        assert!(host("host: aarch64-pc-windows-msvc\n", "windows", "x86_64").is_err());
    }
    #[test]
    fn pe64_signature_machine_and_optional_header_are_all_required() {
        let temporary = Temporary::new("ff-pe").unwrap();
        let path = temporary.0.join("native.exe");
        let mut bytes = vec![0; 256];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&128_u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&0x8664_u16.to_le_bytes());
        bytes[152..154].copy_from_slice(&0x20b_u16.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        binary_format(&path, "x86_64-pc-windows-msvc").unwrap();
        assert!(binary_format(&path, "aarch64-pc-windows-msvc").is_err());
        bytes[152..154].copy_from_slice(&0x10b_u16.to_le_bytes());
        fs::write(&path, &bytes).unwrap();
        assert!(binary_format(&path, "x86_64-pc-windows-msvc").is_err());
        bytes[152..154].copy_from_slice(&0x20b_u16.to_le_bytes());
        bytes[128] = b'X';
        fs::write(&path, &bytes).unwrap();
        assert!(binary_format(&path, "x86_64-pc-windows-msvc").is_err());
    }
    const OPTIONAL: usize = 0x58;
    const SECTION: usize = OPTIONAL + 240;

    /// A PE32+ image with one section holding an import table for `dlls`.
    fn pe_importing(dlls: &[&str]) -> Vec<u8> {
        pe_with(dlls, 240, 16)
    }

    fn pe_with(dlls: &[&str], optional_size: u16, directories: u32) -> Vec<u8> {
        pe_with_delayed(dlls, &[], 1, optional_size, directories)
    }

    /// Also a delay-load table for `delayed` with the given attribute word.
    fn pe_with_delayed(
        dlls: &[&str],
        delayed: &[&str],
        attributes: u32,
        optional_size: u16,
        directories: u32,
    ) -> Vec<u8> {
        const SECTION_RVA: usize = 0x1000;
        const SECTION_FILE: usize = 0x200;
        let mut bytes = vec![0; SECTION_FILE];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&0x8664_u16.to_le_bytes());
        bytes[0x46..0x48].copy_from_slice(&1_u16.to_le_bytes());
        bytes[0x54..0x56].copy_from_slice(&optional_size.to_le_bytes());
        let optional = OPTIONAL;
        bytes[optional..optional + 2].copy_from_slice(&0x20b_u16.to_le_bytes());
        bytes[optional + 108..optional + 112].copy_from_slice(&directories.to_le_bytes());
        let delay_table = 20 * (dlls.len() + 1);
        let delay_size = if delayed.is_empty() {
            0
        } else {
            32 * (delayed.len() + 1)
        };
        let mut section = vec![0; delay_table + delay_size];
        let rva = |offset: usize| u32::try_from(SECTION_RVA + offset).unwrap().to_le_bytes();
        for (index, dll) in dlls.iter().enumerate() {
            let name = rva(section.len());
            section[20 * index + 12..20 * index + 16].copy_from_slice(&name);
            section.extend_from_slice(dll.as_bytes());
            section.push(0);
        }
        for (index, dll) in delayed.iter().enumerate() {
            let descriptor = delay_table + 32 * index;
            let name = rva(section.len());
            section[descriptor..descriptor + 4].copy_from_slice(&attributes.to_le_bytes());
            section[descriptor + 4..descriptor + 8].copy_from_slice(&name);
            section.extend_from_slice(dll.as_bytes());
            section.push(0);
        }
        if !dlls.is_empty() {
            bytes[optional + 120..optional + 124].copy_from_slice(&rva(0));
        }
        if !delayed.is_empty() {
            bytes[optional + 216..optional + 220].copy_from_slice(&rva(delay_table));
        }
        let header = optional + usize::from(optional_size);
        let size = u32::try_from(section.len()).unwrap().to_le_bytes();
        bytes[header + 8..header + 12].copy_from_slice(&size);
        bytes[header + 12..header + 16]
            .copy_from_slice(&u32::try_from(SECTION_RVA).unwrap().to_le_bytes());
        bytes[header + 16..header + 20].copy_from_slice(&size);
        bytes[header + 20..header + 24]
            .copy_from_slice(&u32::try_from(SECTION_FILE).unwrap().to_le_bytes());
        bytes.extend_from_slice(&section);
        bytes
    }

    #[test]
    fn dynamic_visual_cpp_runtime_imports_are_rejected() {
        let image = pe_importing(&[
            "KERNEL32.dll",
            "VCRUNTIME140.dll",
            "msvcrt.dll",
            "api-ms-win-crt-runtime-l1-1-0.dll",
            "MSVCP140.dll",
            "ucrtbase.dll",
        ]);
        assert_eq!(
            pe_imports(&image).unwrap(),
            [
                "kernel32.dll",
                "vcruntime140.dll",
                "msvcrt.dll",
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "msvcp140.dll",
                "ucrtbase.dll"
            ]
        );
        assert_eq!(
            dynamic_crt_imports(&image).unwrap(),
            [
                "vcruntime140.dll",
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "msvcp140.dll",
                "ucrtbase.dll"
            ]
        );
        let temporary = Temporary::new("ff-pe-crt").unwrap();
        let path = temporary.0.join("native.exe");
        fs::write(&path, &image).unwrap();
        let error = binary_format(&path, "x86_64-pc-windows-msvc")
            .unwrap_err()
            .to_string();
        assert!(error.contains("vcruntime140.dll"), "{error}");
        let image = pe_importing(&["KERNEL32.dll", "ntdll.dll", "msvcrt.dll"]);
        assert!(dynamic_crt_imports(&image).unwrap().is_empty());
        fs::write(&path, &image).unwrap();
        binary_format(&path, "x86_64-pc-windows-msvc").unwrap();
    }

    #[test]
    fn import_directory_presence_follows_the_optional_header_bounds() {
        let crt = ["VCRUNTIME140.dll"];
        // The smallest header that still holds the import directory entry.
        assert_eq!(
            pe_imports(&pe_with(&crt, 128, 2)).unwrap(),
            ["vcruntime140.dll"]
        );
        assert!(pe_imports(&pe_with(&crt, 127, 2)).unwrap().is_empty());
        assert!(pe_imports(&pe_with(&crt, 240, 1)).unwrap().is_empty());
        assert!(pe_imports(&pe_with(&crt, 240, 0)).unwrap().is_empty());
    }

    #[test]
    fn delay_loaded_and_older_runtimes_are_rejected_too() {
        let image = pe_with_delayed(
            &["KERNEL32.dll"],
            &["VCRUNTIME140.dll", "user32.dll"],
            1,
            240,
            16,
        );
        assert_eq!(
            pe_imports(&image).unwrap(),
            ["kernel32.dll", "vcruntime140.dll", "user32.dll"]
        );
        assert_eq!(dynamic_crt_imports(&image).unwrap(), ["vcruntime140.dll"]);
        // An image whose import directory is empty can still delay-load.
        let image = pe_with_delayed(&[], &["MSVCR120.dll"], 1, 240, 16);
        assert_eq!(&image[OPTIONAL + 120..OPTIONAL + 124], [0; 4]);
        assert_eq!(dynamic_crt_imports(&image).unwrap(), ["msvcr120.dll"]);
        // Entry 13 needs 14 directories in a 224-byte optional header.
        let delayed = ["VCRUNTIME140.dll"];
        assert_eq!(
            pe_imports(&pe_with_delayed(&[], &delayed, 1, 240, 14))
                .unwrap()
                .len(),
            1
        );
        assert!(
            pe_imports(&pe_with_delayed(&[], &delayed, 1, 240, 13))
                .unwrap()
                .is_empty()
        );
        assert!(
            pe_imports(&pe_with_delayed(&[], &delayed, 1, 223, 16))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            pe_imports(&pe_with_delayed(&[], &delayed, 1, 224, 16))
                .unwrap()
                .len(),
            1
        );
        // The old VA form cannot be resolved without the image base.
        assert!(pe_imports(&pe_with_delayed(&[], &delayed, 0, 240, 16)).is_err());
        let image = pe_importing(&["vcomp140.dll", "msvcr100.dll", "msvcrt.dll"]);
        assert_eq!(
            dynamic_crt_imports(&image).unwrap(),
            ["vcomp140.dll", "msvcr100.dll"]
        );
    }

    #[test]
    fn import_names_are_read_only_from_stored_section_bytes() {
        let crt = ["VCRUNTIME140.dll"];
        let set = |image: &mut Vec<u8>, offset: usize, value: u32| {
            image[SECTION + offset..SECTION + offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        // Descriptors take 40 bytes; the name starts right after them.
        let mut image = pe_importing(&crt);
        set(&mut image, 8, 0);
        assert_eq!(pe_imports(&image).unwrap(), ["vcruntime140.dll"]);
        let mut image = pe_importing(&crt);
        set(&mut image, 8, 0x1000);
        set(&mut image, 16, 40);
        assert!(pe_imports(&image).is_err(), "name beyond the stored bytes");
        let mut image = pe_importing(&crt);
        set(&mut image, 8, 40);
        set(&mut image, 16, 0x1000);
        assert!(
            pe_imports(&image).is_err(),
            "name beyond the mapped section"
        );
    }

    #[test]
    fn malformed_import_tables_are_errors_rather_than_empty() {
        let image = pe_importing(&["VCRUNTIME140.dll"]);
        assert!(pe_imports(&image[..image.len() - 4]).is_err());
        let mut outside = image.clone();
        outside[OPTIONAL + 120..OPTIONAL + 124].copy_from_slice(&0x9000_u32.to_le_bytes());
        assert!(pe_imports(&outside).is_err());
        let mut unterminated = image;
        let last = unterminated.len() - 1;
        unterminated[last] = b'x';
        assert!(pe_imports(&unterminated).is_err());
    }

    #[test]
    fn this_build_links_the_c_runtime_statically() {
        let image = fs::read(std::env::current_exe().unwrap()).unwrap();
        let imports = pe_imports(&image).unwrap();
        assert!(
            imports.iter().any(|name| name == "kernel32.dll"),
            "{imports:?}"
        );
        assert_eq!(dynamic_crt_imports(&image).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn directory_publication_never_replaces_existing_output() {
        let temporary = Temporary::new("ff-publish").unwrap();
        let source = temporary.0.join("source");
        let output = temporary.0.join("output");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("payload"), "new").unwrap();
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep"), "old").unwrap();
        assert!(publish(&source, &output).is_err());
        assert_eq!(fs::read(output.join("keep")).unwrap(), b"old");
        assert_eq!(fs::read(source.join("payload")).unwrap(), b"new");
    }
}
