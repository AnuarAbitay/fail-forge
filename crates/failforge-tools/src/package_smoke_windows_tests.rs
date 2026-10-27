use super::*;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;

fn ordinary_drive_path(canonical: &Path) -> PathBuf {
    assert!(matches!(
        canonical.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
    ));
    let wide: Vec<_> = canonical.as_os_str().encode_wide().collect();
    PathBuf::from(OsString::from_wide(&wide[4..]))
}

#[test]
fn sdk_child_observes_verified_cwd_and_cache_paths_while_receipt_keeps_canonical_cwd() {
    let temporary = common::Temporary::new("ff-smoke-process-paths").unwrap();
    for path in [
        "installation/adapters/dart",
        "sdk",
        "output",
        "cache",
        "work/foreign cwd",
        "work/temporary",
        "work/tool-state",
    ] {
        fs::create_dir_all(temporary.0.join(path)).unwrap();
    }
    let mut harness = Harness {
        installation: temporary.0.join("installation"),
        sdk: temporary.0.join("sdk"),
        output: temporary.0.join("output"),
        work: temporary.0.join("work"),
        cache: temporary.0.join("cache"),
        offline: false,
        commands: Vec::new(),
    };
    let canonical_cwd = harness.installation.join("adapters/dart");
    let canonical_values = [
        canonical_cwd.clone(),
        harness.cache.clone(),
        harness.work.join("temporary"),
        harness.work.join("temporary"),
        harness.work.join("temporary"),
        harness.work.join("tool-state"),
    ];
    let mut command = harness.command(Path::new("cmd.exe"));
    // Match pub_get's cwd override after Harness::command has been built.
    command
        .current_dir(&canonical_cwd)
        .env_remove("CD")
        .args(["/u", "/d", "/c"])
        .raw_arg(
            r#"echo "%CD%"&echo "%PUB_CACHE%"&echo "%TMPDIR%"&echo "%TMP%"&echo "%TEMP%"&echo "%XDG_CONFIG_HOME%""#,
        );
    harness.execute("sdk-paths", &mut command, 30).unwrap();
    let logs = harness.output.join("commands/sdk-paths");
    let receipt = common::read_json(&logs.join("command.json")).unwrap();
    assert_eq!(receipt["cwd"], json!(canonical_cwd));
    assert_eq!(harness.commands[0]["cwd"], json!(canonical_cwd));
    assert_eq!(harness.cache, canonical_values[1]);
    assert_eq!(harness.installation.join("adapters/dart"), canonical_cwd);
    assert_eq!(command.get_program(), "cmd.exe");
    let output = fs::read(logs.join("stdout.log")).unwrap();
    // cmd /u emits UTF-16 even for a Unicode caller temp directory.
    let mut expected = Vec::<u16>::new();
    for canonical in canonical_values {
        let ordinary = ordinary_drive_path(&canonical);
        assert_eq!(ordinary.canonicalize().unwrap(), canonical);
        expected.push(b'"' as u16);
        expected.extend(ordinary.as_os_str().encode_wide());
        expected.extend([b'"' as u16, b'\r' as u16, b'\n' as u16]);
    }
    let expected: Vec<_> = expected.into_iter().flat_map(u16::to_le_bytes).collect();
    assert_eq!(output, expected);
}
