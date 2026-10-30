#![cfg(windows)]
//! Real CreateProcess/Job Object tests, run by native Windows CI.
use failforge_runner::{
    CancelReason, CapturePolicy, CommandSpec, CompletionCause, EnvSpec, Runner, SpecError,
    TimeoutPolicy,
};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    WaitForSingleObject,
};

fn spec(args: &[&str]) -> CommandSpec {
    CommandSpec {
        program: env!("CARGO_BIN_EXE_failforge-process-fixture").into(),
        args: args.iter().map(OsString::from).collect(),
        cwd: std::env::current_dir().unwrap(),
        env: EnvSpec::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::from_secs(20)),
            grace_period: Duration::from_millis(20),
            forced_wait: Duration::from_secs(5),
            output_drain: Duration::from_secs(5),
        },
        capture_policy: CapturePolicy::default(),
    }
}
async fn run(spec: CommandSpec) -> std::sync::Arc<failforge_runner::CommandResult> {
    let runner = Runner::new();
    let result = runner.start(spec).unwrap().wait().await.unwrap();
    assert!(result.cleanup.is_complete(), "{:?}", result.cleanup);
    runner.shutdown().await.unwrap();
    result
}
fn hex(value: &OsStr) -> String {
    value
        .as_encoded_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn ordinary_drive_path(canonical: &Path) -> PathBuf {
    assert!(matches!(
        canonical.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
    ));
    let wide: Vec<_> = canonical.as_os_str().encode_wide().collect();
    PathBuf::from(OsString::from_wide(&wide[4..]))
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = Self(std::env::temp_dir().join(format!(
            "failforge windows {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        std::fs::create_dir(&directory.0).unwrap();
        directory
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn native_argv_preserves_empty_quotes_metacharacters_unicode_and_surrogates() {
    let mut command = spec(&["argv"]);
    let values = [
        OsString::from(""),
        "space and tab\t".into(),
        "quote\"slash\\".into(),
        "& | > %PATH% !VAR! ^".into(),
        "тест 日本語".into(),
        OsString::from_wide(&[0xd800, b'x' as u16, 0xdc00]),
    ];
    command.args.extend(values.clone());
    let result = run(command).await;
    let expected = values
        .iter()
        .map(|arg| format!("{}\n", hex(arg)))
        .collect::<String>();
    assert_eq!(result.stdout.tail, expected.as_bytes());
    assert!(result.status.unwrap().success());
}

#[tokio::test]
async fn canonical_program_uses_verified_ordinary_loaded_module_and_argv0_spelling() {
    let directory = Directory::new();
    let path = directory.0.join("native module #%.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_failforge-process-fixture"), &path).unwrap();
    let canonical = path.canonicalize().unwrap();
    let ordinary = ordinary_drive_path(&canonical);
    let mut command = spec(&["module-path"]);
    command.program = canonical.as_os_str().into();
    let result = run(command).await;
    assert!(result.status.unwrap().success());
    let expected = hex(ordinary.as_os_str());
    assert_eq!(
        result.stdout.tail,
        format!("{expected}\n{expected}\n").as_bytes()
    );
}

#[tokio::test]
async fn nonzero_exit_is_ordinary_result_and_both_pipes_drain() {
    let result = run(spec(&["exit", "37"])).await;
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(37));
    let mut command = spec(&["bytes", "2097152", "1048576"]);
    command.capture_policy.tail_bytes_per_stream = 128;
    let result = run(command).await;
    assert_eq!(result.stdout.bytes_read, 2_097_152);
    assert_eq!(result.stderr.bytes_read, 1_048_576);
    assert_eq!(result.stdout.tail, vec![b'x'; 128]);
    assert!(result.stdout.tail_truncated && result.stderr.tail_truncated);
}

#[tokio::test]
async fn native_control_c_exit_status_is_not_reclassified_as_cancellation() {
    let code = 0xc000013a_u32;
    let result = run(spec(&["exit-dword", &code.to_string()])).await;
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(
        result.status.unwrap().code().map(|value| value as u32),
        Some(code)
    );
}

#[test]
fn runner_child_does_not_share_its_owners_console() {
    let directory = Directory::new();
    let stdout = directory.0.join("console-stdout");
    let stderr = directory.0.join("console-stderr");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_failforge-process-fixture"))
        .arg("console-owner")
        .creation_flags(CREATE_NEW_CONSOLE)
        .stdin(std::process::Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    // This exact retained handle is the only termination authority on failure.
    // Killing the owner closes its Runner job and terminates its nested child.
    let waited = unsafe { WaitForSingleObject(child.as_raw_handle(), 30_000) };
    if waited != WAIT_OBJECT_0 {
        let _ = child.kill();
        if unsafe { WaitForSingleObject(child.as_raw_handle(), 5_000) } == WAIT_OBJECT_0 {
            let _ = child.wait();
        }
        panic!("console-owner exceeded its bounded lifetime: wait={waited}");
    }
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "console-owner failed: {}",
        std::fs::read_to_string(stderr).unwrap()
    );
    let output = std::fs::read_to_string(stdout).unwrap();
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), 6, "{output}");
    let number =
        |line: &str, prefix: &str| line.strip_prefix(prefix).unwrap().parse::<u32>().unwrap();
    let members = |line: &str, prefix: &str| -> Vec<u32> {
        line.strip_prefix(prefix)
            .unwrap()
            .split_whitespace()
            .map(|value| value.parse().unwrap())
            .collect()
    };
    let owner = number(lines[0], "owner-pid ");
    assert!(members(lines[1], "owner-members").contains(&owner));
    let child = number(lines[2], "console-pid ");
    assert_ne!(owner, child);
    // A private hidden console may have a normal code page. Membership, not
    // code-page value or redirected stdout, establishes console isolation.
    let _code_page = number(lines[3], "console-code-page ");
    let error = number(lines[4], "console-query-error ");
    let child_members = members(lines[5], "console-members");
    assert!(!child_members.contains(&owner), "{output}");
    match error {
        0 => assert!(child_members.contains(&child), "{output}"),
        ERROR_INVALID_HANDLE => assert!(child_members.is_empty(), "{output}"),
        _ => panic!("console membership query failed: {output}"),
    }
}

#[tokio::test]
async fn explicit_stdin_and_default_eof_work() {
    assert!(run(spec(&["stdin-eof"])).await.stdout.tail.is_empty());
    let directory = Directory::new();
    let path = directory.0.join("input");
    std::fs::write(&path, b"input\0\xff").unwrap();
    let runner = Runner::new();
    let result = runner
        .start_with_stdin(spec(&["stdin-eof"]), File::open(path).unwrap())
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(result.stdout.tail, b"input\0\xff");
    assert!(result.cleanup.is_complete());
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_environment_is_case_insensitive_and_uses_child_path() {
    let mut command = spec(&["env", "application_value"]);
    command.env.clear = true;
    command
        .env
        .set
        .insert("APPLICATION_VALUE".into(), "child value".into());
    let result = run(command).await;
    assert_eq!(
        result.stdout.tail,
        format!("{}\n", hex(OsStr::new("child value"))).as_bytes()
    );
    let directory = Directory::new();
    let fixture = directory.0.join("fixture.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_failforge-process-fixture"), &fixture).unwrap();
    let mut command = spec(&["exit", "0"]);
    command.program = "fixture".into();
    command.env.clear = true;
    command
        .env
        .set
        .insert("Path".into(), directory.0.clone().into());
    assert!(run(command).await.status.unwrap().success());
    let mut command = spec(&[]);
    command.env.set.insert("Path".into(), "first".into());
    command.env.remove.insert("PATH".into());
    assert_eq!(command.validate(), Err(SpecError::EnvKeyConflict));
}

#[tokio::test]
async fn cwd_and_capture_paths_with_spaces_are_preserved() {
    let directory = Directory::new();
    let mut command = spec(&["cwd"]);
    command.cwd = directory.0.clone();
    command.capture_policy.capture_dir = Some(directory.0.join("capture bytes"));
    let result = run(command).await;
    let expected = format!(
        "{}\n",
        hex(ordinary_drive_path(&std::fs::canonicalize(&directory.0).unwrap()).as_os_str())
    );
    assert_eq!(result.stdout.tail, expected.as_bytes());
    assert_eq!(
        std::fs::read(result.stdout.file_path.as_ref().unwrap()).unwrap(),
        expected.as_bytes()
    );
}

#[tokio::test]
async fn canonical_cwd_uses_verified_ordinary_spelling_with_relative_child_path() {
    let directory = Directory::new();
    let cwd = directory.0.join("working #%");
    std::fs::create_dir_all(cwd.join("relative bin")).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_failforge-process-fixture"),
        cwd.join("relative bin/fixture.exe"),
    )
    .unwrap();
    let canonical = cwd.canonicalize().unwrap();
    let ordinary = ordinary_drive_path(&canonical);
    let mut command = spec(&["cwd-paths"]);
    command.program = "fixture".into();
    command.cwd = canonical.clone();
    command.env.clear = true;
    command.env.set.insert("Path".into(), "relative bin".into());
    let result = run(command).await;
    assert!(result.status.unwrap().success());
    assert_eq!(
        result.stdout.tail,
        format!(
            "{}\n{}\n",
            hex(ordinary.as_os_str()),
            hex(canonical.as_os_str())
        )
        .as_bytes()
    );
}

#[tokio::test]
async fn batch_scripts_are_rejected_without_shell_execution() {
    let directory = Directory::new();
    let script = directory.0.join("do-not-run.cmd");
    std::fs::write(&script, b"@echo executed>sentinel\r\n").unwrap();
    let mut command = spec(&[]);
    command.program = script.into();
    command.cwd = directory.0.clone();
    let runner = Runner::new();
    assert!(runner.start(command).is_err());
    assert!(!directory.0.join("sentinel").exists());
    runner.shutdown().await.unwrap();
}

fn process(pid: u32) -> OwnedHandle {
    // SAFETY: only fixture PIDs received through our owned output pipe are opened.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | 0x0010_0000,
            0,
            pid,
        )
    };
    assert!(!handle.is_null());
    // SAFETY: successful OpenProcess transferred a unique owned handle.
    unsafe { OwnedHandle::from_raw_handle(handle.cast()) }
}
fn exited(handle: &OwnedHandle) -> bool {
    // SAFETY: handle remains owned; wait only observes process state.
    unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 0) == WAIT_OBJECT_0 }
}
async fn ready(handle: &mut failforge_runner::RunHandle, count: usize) -> Vec<u32> {
    let mut stream = handle.take_stream().unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut chunk = stream.recv().await.expect("fixture output before READY");
            bytes.extend_from_slice(chunk.bytes());
            chunk.mark_displayed(chunk.bytes().len());
            let text = String::from_utf8_lossy(&bytes);
            if text.lines().count() >= count && text.ends_with('\n') {
                return text
                    .lines()
                    .map(|line| line.split_whitespace().nth(1).unwrap().parse().unwrap())
                    .collect();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn cancellation_terminates_leader_and_descendants_in_owned_job() {
    let runner = Runner::new();
    let mut command = spec(&["tree-wait"]);
    command.capture_policy.mode = failforge_runner::OutputMode::StreamAndCapture;
    let mut handle = runner.start(command).unwrap();
    let children: Vec<_> = ready(&mut handle, 2)
        .await
        .into_iter()
        .map(process)
        .collect();
    assert!(children.iter().all(|child| !exited(child)));
    assert!(handle.cancel(CancelReason::User));
    let result = handle.wait().await.unwrap();
    assert_eq!(result.cause, CompletionCause::Cancelled(CancelReason::User));
    assert!(result.cleanup.is_complete(), "{:?}", result.cleanup);
    assert!(children.iter().all(exited));
    runner.shutdown().await.unwrap();
}

#[tokio::test]
async fn timeout_kills_the_job_and_leader_exit_still_cleans_descendants() {
    let mut command = spec(&["tree-wait"]);
    command.timeout_policy.timeout = Some(Duration::from_millis(250));
    let result = run(command).await;
    assert_eq!(result.cause, CompletionCause::TimedOut);
    let result = run(spec(&["tree-exit"])).await;
    assert_eq!(result.cause, CompletionCause::ProcessCompleted);
    assert_eq!(result.status.unwrap().code(), Some(0));
}

#[tokio::test]
async fn dropping_public_handle_keeps_supervisor_cleanup_owned() {
    let runner = Runner::new();
    let mut command = spec(&["tree-wait"]);
    command.capture_policy.mode = failforge_runner::OutputMode::StreamAndCapture;
    let mut handle = runner.start(command).unwrap();
    let children: Vec<_> = ready(&mut handle, 2)
        .await
        .into_iter()
        .map(process)
        .collect();
    drop(handle);
    runner.shutdown().await.unwrap();
    assert!(children.iter().all(exited));
}

fn private_file(path: &std::path::Path) -> File {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::Storage::FileSystem::*;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = std::ptr::null_mut();
    // SAFETY: native pointers reference live initialized storage throughout each
    // call; all returned allocations/handles are released before this returns.
    unsafe {
        assert_ne!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
            0
        );
        let mut length = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut length);
        let mut storage = vec![0_usize; (length as usize).div_ceil(size_of::<usize>())];
        assert_ne!(
            GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                length,
                &mut length
            ),
            0
        );
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut sid = std::ptr::null_mut();
        assert_ne!(ConvertSidToStringSidW(user.User.Sid, &mut sid), 0);
        let mut count = 0;
        while *sid.add(count) != 0 {
            count += 1;
        }
        let sid_text = String::from_utf16(std::slice::from_raw_parts(sid, count)).unwrap();
        LocalFree(sid.cast());
        CloseHandle(token);
        let descriptor_text: Vec<_> = format!("O:{sid_text}D:P(A;;FA;;;{sid_text})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor_text.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut()
            ),
            0
        );
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
        let handle = CreateFileW(
            path.as_ptr(),
            0xc0000000,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        );
        LocalFree(descriptor);
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        File::from_raw_handle(handle)
    }
}

#[tokio::test]
async fn independent_native_lease_survives_controller_handle_close() {
    use failforge_runner::{WORKSPACE_LEASE_LOCK_OFFSET, WORKSPACE_LEASE_MAGIC, WorkspaceLease};
    let directory = Directory::new();
    let path = directory.0.join("lease");
    let mut owner = private_file(&path);
    owner.write_all(WORKSPACE_LEASE_MAGIC).unwrap();
    let lease = WorkspaceLease::from_borrowed(owner.as_handle()).unwrap();
    let runner = Runner::new();
    let mut command = spec(&["lease-hold"]);
    command.capture_policy.mode = failforge_runner::OutputMode::StreamAndCapture;
    let mut handle = runner.start_with_workspace_lease(command, &lease).unwrap();
    let children: Vec<_> = ready(&mut handle, 1)
        .await
        .into_iter()
        .map(process)
        .collect();
    drop(lease);
    drop(owner);
    let peer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let try_exclusive = || {
        // SAFETY: zero is valid OVERLAPPED and offset points beyond marker.
        let mut overlap: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlap.Anonymous.Anonymous.Offset = WORKSPACE_LEASE_LOCK_OFFSET as u32;
        // SAFETY: peer is owned; fail-immediately synchronous lock request keeps
        // OVERLAPPED alive until completion and never waits for another owner.
        unsafe {
            LockFileEx(
                peer.as_raw_handle().cast(),
                LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK,
                0,
                1,
                0,
                &mut overlap,
            ) != 0
        }
    };
    assert!(
        !try_exclusive(),
        "native owner must retain independent shared lock"
    );
    handle.force_cancel(CancelReason::User);
    assert!(handle.wait().await.unwrap().cleanup.is_complete());
    assert!(children.iter().all(exited));
    assert!(try_exclusive(), "owner exit must release its lock");
    runner.shutdown().await.unwrap();
}

#[test]
fn lease_capability_rejects_public_permissions_and_wrong_marker() {
    use failforge_runner::{WORKSPACE_LEASE_MAGIC, WorkspaceLease};
    let directory = Directory::new();
    let public_path = directory.0.join("public");
    let mut public = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&public_path)
        .unwrap();
    public.write_all(WORKSPACE_LEASE_MAGIC).unwrap();
    assert!(WorkspaceLease::from_borrowed(public.as_handle()).is_err());
    let mut private = private_file(&directory.0.join("private"));
    private
        .write_all(b"FAILFORGE_WORKSPACE_LEASE_V2\n")
        .unwrap();
    assert!(WorkspaceLease::from_borrowed(private.as_handle()).is_err());
}

#[tokio::test]
async fn native_override_replaces_inherited_case_alias_inside_nested_job() {
    let mut command = spec(&["env-override"]);
    command
        .env
        .set
        .insert("flutter_root".into(), "inherited wrong SDK".into());
    let result = run(command).await;
    assert!(result.status.unwrap().success());
    assert_eq!(
        result.stdout.tail,
        format!("{}\n", hex(OsStr::new("controlled Flutter SDK"))).as_bytes()
    );
}
