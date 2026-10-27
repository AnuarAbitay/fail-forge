use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
static NEXT: AtomicU64 = AtomicU64::new(0);
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
const DRAIN_GRACE: Duration = Duration::from_secs(30);
thread_local! {
    static RETAIN_TEMPORARIES: Cell<bool> = const { Cell::new(false) };
}

pub fn cleanup_confirmed() -> bool {
    !RETAIN_TEMPORARIES.get()
}

#[cfg(unix)]
extern "C" fn interrupted(signal: libc::c_int) {
    let _ = INTERRUPTED.compare_exchange(0, signal, Ordering::Relaxed, Ordering::Relaxed);
}

#[cfg(unix)]
pub struct SignalGuard(Vec<(libc::c_int, libc::sigaction)>);
#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, action) in &self.0 {
            unsafe {
                libc::sigaction(*signal, action, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(unix)]
pub fn install_interrupts() -> Result<SignalGuard> {
    let mut guard = SignalGuard(Vec::new());
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = interrupted as *const () as usize;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        guard.0.push((signal, previous));
    }
    Ok(guard)
}

#[cfg(windows)]
pub struct SignalGuard;
#[cfg(windows)]
unsafe extern "system" fn console_interrupt(event: u32) -> windows_sys::core::BOOL {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
        let _ = INTERRUPTED.compare_exchange(0, libc::SIGINT, Ordering::Relaxed, Ordering::Relaxed);
        1
    } else {
        0
    }
}
#[cfg(windows)]
pub fn install_interrupts() -> Result<SignalGuard> {
    if unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_interrupt), 1)
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(SignalGuard)
}
#[cfg(windows)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_interrupt), 0);
        }
    }
}

pub fn status_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1))
    }
    #[cfg(windows)]
    {
        status.code().unwrap_or(1)
    }
}
pub fn executable(path: &Path, metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = path;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        metadata.is_file()
            && path
                .extension()
                .is_some_and(|value| value.eq_ignore_ascii_case("exe"))
    }
}
pub fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        let parent = failforge_workspace::windows_fs::directory(
            path.parent()
                .ok_or_else(|| io::Error::other("directory has no parent"))?,
        )?;
        failforge_workspace::windows_fs::create_directory_at(
            &parent,
            path.file_name()
                .ok_or_else(|| io::Error::other("directory has no name"))?,
        )?;
        Ok(())
    }
}
#[cfg(windows)]
pub fn publish_directory(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        let file = failforge_workspace::windows_fs::directory(source)?;
        let parent = failforge_workspace::windows_fs::directory(
            destination.parent().ok_or("destination has no parent")?,
        )?;
        failforge_workspace::windows_fs::rename_handle(
            &file,
            &parent,
            destination.file_name().ok_or("destination has no name")?,
            false,
        )?;
        Ok(())
    }
}

pub fn interrupt_signal() -> Option<i32> {
    match INTERRUPTED.load(Ordering::Relaxed) {
        0 => None,
        signal => Some(signal),
    }
}

pub fn check_interrupt() -> Result<()> {
    if let Some(signal) = interrupt_signal() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            format!("interrupted by signal {signal}"),
        )
        .into())
    } else {
        Ok(())
    }
}

pub fn ensure(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.into()).into())
    }
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned()
}

pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn read_json(path: &Path) -> Result<Value> {
    Ok(failforge_workspace::parse_json(&fs::read(path)?)?)
}

pub fn create_output(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(path)?;
    Ok(())
}

pub fn hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        check_interrupt()?;
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        digest.update(&bytes[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn tree(path: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, path: &Path, output: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in fs::read_dir(path)? {
            check_interrupt()?;
            let entry = entry?;
            let kind = entry.file_type()?;
            #[cfg(windows)]
            ensure(
                !failforge_workspace::windows_fs::is_reparse(&entry.metadata()?),
                "fixture contains a reparse point",
            )?;
            if kind.is_dir() {
                walk(root, &entry.path(), output)?;
            } else if kind.is_file() {
                output.insert(
                    entry
                        .path()
                        .strip_prefix(root)?
                        .components()
                        .map(|part| part.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                    hash(&entry.path())?,
                );
            } else {
                return Err(io::Error::other("fixture contains a symlink or special entry").into());
            }
        }
        Ok(())
    }
    let mut output = BTreeMap::new();
    walk(path, path, &mut output)?;
    Ok(output)
}

pub struct Temporary(pub PathBuf);
impl Temporary {
    pub fn new(prefix: &str) -> Result<Self> {
        Self::new_in(&std::env::temp_dir(), prefix)
    }
    pub fn new_in(parent: &Path, prefix: &str) -> Result<Self> {
        for _ in 0..100 {
            let path = parent.join(format!(
                "{prefix}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match private_directory(&path) {
                Ok(()) => return Ok(Self(path.canonicalize()?)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::other("cannot allocate temporary fixture").into())
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if cleanup_confirmed() {
            let _ = fs::remove_dir_all(&self.0);
        } else {
            eprintln!(
                "retaining temporary inputs after unconfirmed cleanup: {}",
                self.0.display()
            );
        }
    }
}

/// SIGINT requests native cleanup before reaping. A broken owner that exceeds
/// the independent drain grace is reported explicitly; its temporary inputs
/// remain available and the harness does not pretend cleanup succeeded.
pub struct OwnedChild(pub Child, bool, #[cfg(windows)] Option<File>);
impl OwnedChild {
    #[cfg(unix)]
    pub fn new(child: Child) -> Self {
        Self(child, false)
    }
    pub fn spawn(command: &mut Command) -> Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self::new(command.spawn()?))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::Foundation::{ERROR_PIPE_LISTENING, INVALID_HANDLE_VALUE};
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_OUTBOUND,
            };
            use windows_sys::Win32::System::Pipes::*;
            let path = format!(
                r"\\.\pipe\failforge-cli-cancel-{}",
                failforge_workspace::windows_fs::random_token()?
            );
            let name: Vec<u16> = std::ffi::OsStr::new(&path)
                .encode_wide()
                .chain(Some(0))
                .collect();
            let mut security = failforge_workspace::windows_fs::PrivateSecurity::new()?;
            let handle = unsafe {
                CreateNamedPipeW(
                    name.as_ptr(),
                    PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    128,
                    128,
                    0,
                    security.as_ptr(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error().into());
            }
            let pipe = unsafe { File::from_raw_handle(handle) };
            use std::os::windows::io::AsRawHandle;
            if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), std::ptr::null_mut()) } == 0
                && io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_LISTENING as i32)
            {
                return Err(io::Error::last_os_error().into());
            }
            command.env("FAILFORGE_CLI_CANCEL_PIPE", path);
            Ok(Self(command.spawn()?, false, Some(pipe)))
        }
    }
    pub fn signal(&mut self, signal: i32) -> Result<()> {
        if self.0.try_wait()?.is_none() {
            // The child is still owned and unreaped here, preventing PID reuse.
            #[cfg(unix)]
            if unsafe { libc::kill(self.0.id() as i32, signal) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            #[cfg(windows)]
            {
                if signal == 9 {
                    self.0.kill()?;
                    return Ok(());
                }
                let pipe = self
                    .2
                    .as_ref()
                    .ok_or("native cancellation capability is absent")?;
                let mut writer = pipe;
                writer.write_all(if signal == libc::SIGTERM { b"T" } else { b"I" })?;
            }
        }
        Ok(())
    }
    pub fn wait(&mut self, budget: Duration) -> Result<i32> {
        self.wait_with_grace(budget, DRAIN_GRACE)
    }
    fn drain(&mut self, grace: Duration) -> bool {
        let start = Instant::now();
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if start.elapsed() < grace => std::thread::sleep(Duration::from_millis(1)),
                _ => {
                    self.1 = true;
                    RETAIN_TEMPORARIES.set(true);
                    return false;
                }
            }
        }
    }
    fn wait_with_grace(&mut self, budget: Duration, grace: Duration) -> Result<i32> {
        let start = Instant::now();
        loop {
            if let Some(signal) = interrupt_signal() {
                self.signal(signal)?;
                let drained = self.drain(grace);
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!(
                        "interrupted by signal {signal}; {}",
                        if drained {
                            "owner drained".to_owned()
                        } else {
                            format!(
                                "cleanup unconfirmed, owner PID {}, temporary inputs retained",
                                self.0.id()
                            )
                        }
                    ),
                )
                .into());
            }
            if let Some(status) = self.0.try_wait()? {
                return Ok(status_code(status));
            }
            if start.elapsed() >= budget {
                self.signal(libc::SIGINT)?;
                let drained = self.drain(grace);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    if drained {
                        "native command exceeded external deadline; owner drained".to_owned()
                    } else {
                        format!("native command exceeded deadline and drain grace; cleanup unconfirmed, owner PID {}, temporary inputs retained", self.0.id())
                    },
                )
                .into());
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.1 {
            return;
        }
        match self.0.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                if self
                    .signal(interrupt_signal().unwrap_or(libc::SIGINT))
                    .is_err()
                {
                    // Only the exact retained child handle is terminated. An
                    // unavailable cooperative channel never proves cleanup.
                    RETAIN_TEMPORARIES.set(true);
                    let _ = self.0.kill();
                }
                if !self.drain(DRAIN_GRACE) {
                    eprintln!(
                        "cleanup unconfirmed for owner PID {}; temporary inputs retained",
                        self.0.id()
                    );
                }
            }
            Err(_) => RETAIN_TEMPORARIES.set(true),
        }
    }
}

pub fn command(program: &Path) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
    command
        .stdin(Stdio::null())
        .env("CI", "true")
        .env("DART_SUPPRESS_ANALYTICS", "true")
        .env("FLUTTER_SUPPRESS_ANALYTICS", "true")
        .env_remove("FAILFORGE_CONTROL")
        .env_remove("FAILFORGE_WORKSPACE_LEASE_FD");
    command
}

pub fn execute(
    command: &mut Command,
    directory: &Path,
    budget: Duration,
    expected: i32,
) -> Result<Value> {
    check_interrupt()?;
    fs::create_dir_all(directory)?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");
    command
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?);
    let argv: Vec<String> = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|s| s.to_string_lossy().into_owned())
        .collect();
    let started = Instant::now();
    let mut child = OwnedChild::spawn(command)?;
    let waited = child.wait(budget);
    let code = match &waited {
        Ok(code) => Some(*code),
        Err(_) => child.0.try_wait()?.map(status_code),
    };
    let receipt = json!({"command":argv,"exitCode":code,
        "wallSeconds":started.elapsed().as_secs_f64(),
        "error": waited.as_ref().err().map(ToString::to_string),
        "cleanupUnconfirmed": child.1, "ownerPid": child.0.id()});
    write_json(&directory.join("command.json"), &receipt)?;
    let code = waited?;
    ensure(
        code == expected,
        format!(
            "command returned {code}, expected {expected}; see {}",
            stderr.display()
        ),
    )?;
    Ok(receipt)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn temporary_is_unique_and_cleaned() {
        let a = Temporary::new("ff-tools-test").unwrap();
        let b = Temporary::new("ff-tools-test").unwrap();
        assert_ne!(a.0, b.0);
        let path = a.0.clone();
        drop(a);
        assert!(!path.exists());
    }
    #[test]
    fn hashes_detect_changed_file() {
        let t = Temporary::new("ff-tools-test").unwrap();
        fs::write(t.0.join("file"), b"first").unwrap();
        let before = tree(&t.0).unwrap();
        fs::write(t.0.join("file"), b"other").unwrap();
        assert_ne!(before, tree(&t.0).unwrap());
    }
    #[test]
    fn tree_refuses_symlinks() {
        let t = Temporary::new("ff-tools-test").unwrap();
        std::os::unix::fs::symlink("/tmp", t.0.join("link")).unwrap();
        assert!(tree(&t.0).is_err());
    }
    #[test]
    fn failed_command_retains_receipt() {
        let t = Temporary::new("ff-tools-test").unwrap();
        let mut cmd = command(Path::new("/bin/sh"));
        cmd.args(["-c", "exit 7"]);
        assert!(execute(&mut cmd, &t.0, Duration::from_secs(2), 0).is_err());
        assert_eq!(read_json(&t.0.join("command.json")).unwrap()["exitCode"], 7);
    }
    #[test]
    fn deadline_drains_owned_process() {
        let mut cmd = command(Path::new("/bin/sleep"));
        cmd.arg("10");
        let mut child = OwnedChild::new(cmd.spawn().unwrap());
        assert!(child.wait(Duration::from_millis(10)).is_err());
        assert!(child.0.try_wait().unwrap().is_some());
    }
    #[test]
    fn timeout_retains_failed_receipt() {
        let t = Temporary::new("ff-tools-timeout").unwrap();
        let mut cmd = command(Path::new("/bin/sleep"));
        cmd.arg("10");
        assert!(execute(&mut cmd, &t.0, Duration::from_millis(10), 0).is_err());
        let receipt = read_json(&t.0.join("command.json")).unwrap();
        assert_eq!(receipt["exitCode"], -libc::SIGINT);
        assert!(receipt["error"].as_str().unwrap().contains("deadline"));
    }
    #[test]
    fn output_requires_new_directory_and_creates_parents() {
        let t = Temporary::new("ff-tools-output").unwrap();
        let path = t.0.join("parent/output");
        create_output(&path).unwrap();
        fs::write(path.join("retain"), b"original").unwrap();
        assert!(create_output(&path).is_err());
        assert_eq!(fs::read(path.join("retain")).unwrap(), b"original");
    }
    #[test]
    fn json_rejects_duplicate_fields() {
        let t = Temporary::new("ff-tools-json").unwrap();
        fs::write(t.0.join("input"), br#"{"version":1,"version":2}"#).unwrap();
        assert!(read_json(&t.0.join("input")).is_err());
    }
    #[test]
    fn ignored_interrupt_has_bounded_drain_and_preserves_temporary_inputs() {
        let t = Temporary::new("ff-tools-undrained").unwrap();
        let path = t.0.clone();
        let marker = path.join("ready");
        let mut cmd = command(Path::new("/bin/sh"));
        cmd.args([
            "-c",
            "trap '' INT; : > \"$1\"; exec /bin/sleep 2",
            "fixture",
        ])
        .arg(&marker);
        let mut child = OwnedChild::new(cmd.spawn().unwrap());
        let ready = Instant::now();
        while !marker.exists() {
            assert!(ready.elapsed() < Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(1));
        }
        let start = Instant::now();
        let error = child
            .wait_with_grace(Duration::from_millis(10), Duration::from_millis(20))
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(error.to_string().contains("cleanup unconfirmed"));
        assert!(child.0.try_wait().unwrap().is_none());
        drop(t);
        assert!(marker.is_file());
        // This is the test's single /bin/sleep fixture, not a native owner.
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        RETAIN_TEMPORARIES.set(false);
        fs::remove_dir_all(path).unwrap();
    }
    #[test]
    #[ignore = "subprocess fixture for real SIGINT/SIGTERM forwarding tests"]
    fn interrupt_probe() {
        let marker = PathBuf::from(std::env::var_os("FAILFORGE_TOOLS_INTERRUPT_MARKER").unwrap());
        let phase = std::env::var("FAILFORGE_TOOLS_INTERRUPT_PHASE").unwrap();
        let _guard = install_interrupts().unwrap();
        let mut command = command(Path::new("/bin/sleep"));
        command.arg("10");
        let mut owner = OwnedChild::new(command.spawn().unwrap());
        let pid = owner.0.id();
        let (mut stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        fs::write(&marker, b"ready").unwrap();
        let result = match phase.as_str() {
            "wait" => owner.wait(Duration::from_secs(10)).map(|_| ()),
            "ready" => crate::benchmark::receive_ready(&mut stream, Duration::from_secs(10)),
            "author" => loop {
                if let Err(error) = check_interrupt() {
                    break Err(error);
                }
                std::thread::sleep(Duration::from_millis(1));
            },
            _ => panic!("unknown interrupt probe phase"),
        };
        assert!(result.unwrap_err().to_string().contains("interrupted"));
        assert!(interrupt_signal().is_some());
        drop(owner);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        fs::write(&marker, b"drained").unwrap();
    }
    #[test]
    fn sigint_and_sigterm_drain_waiting_ready_and_author_barrier_owners() {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            for phase in ["wait", "ready", "author"] {
                let t = Temporary::new("ff-tools-interrupt").unwrap();
                let marker = t.0.join("state");
                let mut command = command(&std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "common::tests::interrupt_probe",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("FAILFORGE_TOOLS_INTERRUPT_MARKER", &marker)
                    .env("FAILFORGE_TOOLS_INTERRUPT_PHASE", phase)
                    .stdout(File::create(t.0.join("stdout")).unwrap())
                    .stderr(File::create(t.0.join("stderr")).unwrap());
                let mut probe = OwnedChild::new(command.spawn().unwrap());
                let start = Instant::now();
                while !marker.exists() {
                    assert!(start.elapsed() < Duration::from_secs(5));
                    assert!(probe.0.try_wait().unwrap().is_none());
                    std::thread::sleep(Duration::from_millis(1));
                }
                probe.signal(signal).unwrap();
                assert_eq!(
                    probe.wait(Duration::from_secs(5)).unwrap(),
                    0,
                    "{phase} signal {signal}: {}",
                    fs::read_to_string(t.0.join("stderr")).unwrap()
                );
                assert_eq!(fs::read(marker).unwrap(), b"drained");
            }
        }
    }
}

pub fn summary(raw: &[u8]) -> Result<BTreeMap<String, String>> {
    Ok(failforge_dart::parse_cli_summary(raw)?)
}

#[cfg(unix)]
pub fn account_id() -> Result<String> {
    Ok(unsafe { libc::geteuid() }.to_string())
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    #[test]
    fn nested_keys_allow_removing_generated_profile_without_changing_inputs() {
        let temporary = Temporary::new("ff-tools-tree-keys").unwrap();
        fs::create_dir(temporary.0.join("lib")).unwrap();
        fs::write(temporary.0.join("lib").join("value.dart"), b"original").unwrap();
        let before = tree(&temporary.0).unwrap();
        assert_eq!(
            before.keys().map(String::as_str).collect::<Vec<_>>(),
            ["lib/value.dart"]
        );

        fs::create_dir(temporary.0.join("profiles")).unwrap();
        fs::write(temporary.0.join("profiles").join("value.yml"), b"generated").unwrap();
        let mut after = tree(&temporary.0).unwrap();
        assert!(after.remove("profiles/value.yml").is_some());
        assert_eq!(after, before);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn temporary_directory_is_private_and_unique() {
        let first = Temporary::new("ff-native-tools").unwrap();
        let second = Temporary::new("ff-native-tools").unwrap();
        assert_ne!(first.0, second.0);
        failforge_workspace::windows_fs::require_private(
            &failforge_workspace::windows_fs::directory(&first.0).unwrap(),
            true,
        )
        .unwrap();
        let path = first.0.clone();
        drop(first);
        assert!(!path.exists());
    }
    #[test]
    #[ignore = "subprocess fixture for capability-owned cancellation"]
    fn cancellation_client() {
        let path = std::env::var_os("FAILFORGE_TOOLS_TEST_MARKER").unwrap();
        let pipe = std::env::var_os("FAILFORGE_CLI_CANCEL_PIPE").unwrap();
        let mut client = fs::OpenOptions::new().read(true).open(pipe).unwrap();
        fs::write(&path, b"ready").unwrap();
        let mut signal = [0];
        client.read_exact(&mut signal).unwrap();
        fs::write(path, signal).unwrap();
    }
    #[test]
    fn cancellation_uses_private_owned_pipe_and_preserves_interrupt_kind() {
        for (signal, byte) in [(libc::SIGINT, b'I'), (libc::SIGTERM, b'T')] {
            let temporary = Temporary::new("ff-tools-pipe").unwrap();
            let marker = temporary.0.join("marker");
            let mut command = command(&std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "common::windows_tests::cancellation_client",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FAILFORGE_TOOLS_TEST_MARKER", &marker);
            let mut child = OwnedChild::spawn(&mut command).unwrap();
            let start = Instant::now();
            while !marker.exists() {
                assert!(start.elapsed() < Duration::from_secs(10));
                assert!(child.0.try_wait().unwrap().is_none());
                std::thread::sleep(Duration::from_millis(5));
            }
            child.signal(signal).unwrap();
            assert_eq!(child.wait(Duration::from_secs(10)).unwrap(), 0);
            assert_eq!(fs::read(marker).unwrap(), vec![byte]);
        }
    }
}
