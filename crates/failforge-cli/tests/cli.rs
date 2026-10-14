//! Process tests for Linux and macOS. Every CLI process has an external deadline
//! and a guard; controlled waiting helpers have an independent shutdown socket.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const DEADLINE: Duration = Duration::from_secs(8);

struct Observed {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

struct Harness {
    directory: PathBuf,
    listener: UnixListener,
    control_path: PathBuf,
    child: Option<Child>,
    controls: Vec<UnixStream>,
}

impl Harness {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "ff-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let control_path = directory.join("control.sock");
        let listener = UnixListener::bind(&control_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            directory,
            listener,
            control_path,
            child: None,
            controls: Vec::new(),
        }
    }

    fn spawn(&mut self, arguments: &[OsString]) {
        let stdout = File::create(self.directory.join("cli.stdout")).unwrap();
        self.spawn_with_stdout(arguments, Stdio::from(stdout));
    }

    fn spawn_with_stdout(&mut self, arguments: &[OsString], stdout: Stdio) {
        let stderr = File::create(self.directory.join("cli.stderr")).unwrap();
        self.spawn_with_sinks(arguments, stdout, Stdio::from(stderr));
    }

    fn spawn_with_sinks(&mut self, arguments: &[OsString], stdout: Stdio, stderr: Stdio) {
        // Keep a capture file even for pipe-based sink tests, so finish() has a
        // uniform bounded result path and never waits for a pipe EOF itself.
        if !self.directory.join("cli.stdout").exists() {
            File::create(self.directory.join("cli.stdout")).unwrap();
        }
        if !self.directory.join("cli.stderr").exists() {
            File::create(self.directory.join("cli.stderr")).unwrap();
        }
        self.child = Some(
            Command::new(env!("CARGO_BIN_EXE_failforge"))
                .args(arguments)
                .env("FAILFORGE_CONTROL", &self.control_path)
                .env("FF_CLI_PARENT", "parent-value")
                .current_dir(&self.directory)
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr)
                .spawn()
                .unwrap(),
        );
    }

    fn spawn_fixture(&mut self, options: &[&str], fixture_args: &[&str]) {
        let mut arguments: Vec<OsString> = std::iter::once("run")
            .chain(options.iter().copied())
            .map(OsString::from)
            .collect();
        arguments.extend([
            OsString::from("--"),
            OsString::from(env!("CARGO_BIN_EXE_failforge-cli-process-fixture")),
        ]);
        arguments.extend(fixture_args.iter().map(OsString::from));
        self.spawn(&arguments);
    }

    fn ready(&mut self) -> (u32, u32) {
        let deadline = Instant::now() + DEADLINE;
        let stream = loop {
            assert!(
                Instant::now() < deadline,
                "helper never connected to control socket"
            );
            match self.listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "helper never connected to control socket"
                    );
                    std::thread::park_timeout(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("control accept: {error}"),
            }
        };
        // Own the exact connection before any assertion can unwind. No socket
        // timeout is installed after a fast helper may already have closed it.
        self.controls.push(stream);
        let stream = self.controls.last_mut().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut ready = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "helper READY read deadline");
            let mut byte = [0];
            match stream.read(&mut byte) {
                Ok(1) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    ready.push(byte[0]);
                    assert!(ready.len() < 128, "helper READY record too large");
                }
                Ok(_) => panic!("helper ended before READY"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::park_timeout(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("control READY read: {error}"),
            }
        }
        let ready = std::str::from_utf8(&ready).unwrap();
        let fields: Vec<_> = ready.split_whitespace().collect();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields.first(), Some(&"READY"));
        (fields[1].parse().unwrap(), fields[2].parse().unwrap())
    }

    fn signal(&self, signal: i32) {
        let pid = self.child.as_ref().unwrap().id() as libc::pid_t;
        // SAFETY: Child is owned by this guard and has not been reaped; kill is
        // applied only to this live CLI, never to a retired numeric process group.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    }

    fn wait_for_diagnostic(&self, token: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let bytes = fs::read(self.directory.join("cli.stderr")).unwrap();
            if String::from_utf8_lossy(&bytes).contains(token) {
                return;
            }
            assert!(Instant::now() < deadline, "CLI did not acknowledge {token}");
            std::thread::park_timeout(Duration::from_millis(2));
        }
    }

    fn assert_controls_closed(&self) {
        for control in &self.controls {
            let mut control = control.try_clone().unwrap();
            control.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match control.read(&mut [0]) {
                    Ok(0) => break,
                    Ok(_) => panic!("unexpected control data after READY"),
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        assert!(
                            Instant::now() < deadline,
                            "waiting helper retained its control channel after CLI cleanup"
                        );
                        std::thread::park_timeout(Duration::from_millis(2));
                    }
                    Err(error) => panic!("control EOF verification failed: {error}"),
                }
            }
        }
    }

    fn finish(&mut self) -> Observed {
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "CLI exceeded external deadline");
            std::thread::park_timeout(Duration::from_millis(2));
        };
        Observed {
            status,
            stdout: fs::read(self.directory.join("cli.stdout")).unwrap(),
            stderr: fs::read(self.directory.join("cli.stderr")).unwrap(),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // This channel is independent of signals, stdout, and the runner: wait
        // and ignore fixtures exit even if an assertion or CLI cleanup fails.
        for control in &mut self.controls {
            let _ = control.write_all(b"exit 0\n");
            let _ = control.shutdown(std::net::Shutdown::Both);
        }
        // EOF is the independent fallback if a nonblocking command write failed.
        self.controls.clear();
        if let Some(child) = &mut self.child {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::park_timeout(Duration::from_millis(2))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn run_fixture(options: &[&str], fixture: &[&str]) -> Observed {
    let mut harness = Harness::new();
    harness.spawn_fixture(options, fixture);
    harness.finish()
}

fn raw_pattern(length: usize) -> Vec<u8> {
    [0, 255, 97, 254, 10]
        .into_iter()
        .cycle()
        .take(length)
        .collect()
}

struct FullPipe {
    reader: File,
    writer: File,
    observer: File,
    prefilled: usize,
    flags: i32,
}

fn full_nonblocking_pipe() -> FullPipe {
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors is initialized storage for the two newly owned FDs.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: successful pipe returned fresh FDs with no previous Rust owner.
    let (reader, writer) = unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    };
    for descriptor in [&reader, &writer] {
        // SAFETY: descriptors have live owners; fcntl only takes integer flags.
        assert_eq!(
            unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    descriptor.as_raw_fd(),
                    libc::F_SETFL,
                    flags | libc::O_NONBLOCK,
                )
            },
            0
        );
    }
    let mut writer = File::from(writer);
    let observer = writer.try_clone().unwrap();
    let deadline = Instant::now() + DEADLINE;
    let mut prefilled = 0;
    loop {
        assert!(Instant::now() < deadline, "pipe never reached EAGAIN");
        match writer.write(&[b'p'; 4096]) {
            Ok(0) => panic!("pipe prefill wrote zero"),
            Ok(bytes) => prefilled += bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("pipe prefill failed: {error}"),
        }
    }
    assert!(prefilled >= 4096);
    // SAFETY: writer still owns the FD; query the actual post-configuration flags.
    let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    FullPipe {
        reader: File::from(reader),
        writer,
        observer,
        prefilled,
        flags,
    }
}

fn drain_available(reader: &mut File, bytes: &mut Vec<u8>, limit: usize) {
    loop {
        let mut buffer = [0; 4096];
        match reader.read(&mut buffer) {
            Ok(0) => return,
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                assert!(
                    bytes.len() <= limit,
                    "test sink exceeded its bounded output"
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) => panic!("test sink read failed: {error}"),
        }
    }
}

#[test]
fn forwards_zero_nonzero_and_reserved_child_codes() {
    for code in [0, 1, 37, 124, 125, 130, 143, 255] {
        let observed = run_fixture(&[], &["exit", &code.to_string()]);
        assert_eq!(observed.status.code(), Some(code));
        assert!(observed.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&observed.stderr)
                .contains(&format!("cause=ProcessCompleted status=exit:{code}"))
        );
    }
}

#[test]
fn clap_usage_help_and_invalid_duration_have_standard_codes() {
    for (args, code) in [
        (vec!["--help"], 0),
        (vec!["run", "--help"], 0),
        (vec!["run"], 2),
        (vec!["run", "tool"], 2),
        (vec!["run", "--timeout", "0s", "--", "tool"], 2),
        (
            vec!["run", "--timeout", "18446744073709551616m", "--", "tool"],
            2,
        ),
    ] {
        let mut harness = Harness::new();
        harness.spawn(&args.into_iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(harness.finish().status.code(), Some(code));
    }
}

#[test]
fn configuration_errors_are_redacted() {
    let mut harness = Harness::new();
    harness.spawn(
        &[
            "run",
            "--env",
            "TOKEN=SECRET_VALUE",
            "--env-remove",
            "TOKEN",
            "--",
            "/bin/true",
        ]
        .map(OsString::from),
    );
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&observed.stderr).contains("SECRET_VALUE"));
}

#[test]
fn parser_errors_do_not_echo_raw_invalid_values_or_misplaced_child_arguments() {
    let secret = "PRIVATE_PARSE_SECRET";
    let mut disclosures = Vec::new();
    let mut cases: Vec<Vec<OsString>> = [
        vec!["run", "--timeout", secret, "--", "/bin/true"],
        vec!["run", "--grace-period", secret, "--", "/bin/true"],
        vec!["run", secret],
        vec![secret],
        vec!["run", "--PRIVATE_PARSE_SECRET", "--", "/bin/true"],
        vec!["run", "--timeout=PRIVATE_PARSE_SECRET", "--", "/bin/true"],
        vec![
            "run",
            "--grace-period=PRIVATE_PARSE_SECRET",
            "--",
            "/bin/true",
        ],
        vec!["run", "--stream=PRIVATE_PARSE_SECRET", "--", "/bin/true"],
        vec!["run", "--env", "TOKEN=PRIVATE_PARSE_SECRET", "--timeout"],
        vec![
            "run",
            "--timeout",
            "1s",
            "--timeout",
            "2s",
            "--env",
            "TOKEN=PRIVATE_PARSE_SECRET",
            "--",
            "/bin/true",
        ],
    ]
    .into_iter()
    .map(|arguments| arguments.into_iter().map(OsString::from).collect())
    .collect();
    for native in [
        b"PRIVATE_PARSE_SECRET\xff".as_slice(),
        b"--timeout=PRIVATE_PARSE_SECRET\xff".as_slice(),
        b"--PRIVATE_PARSE_SECRET\xff".as_slice(),
    ] {
        let mut arguments = vec![OsString::from("run")];
        if !native.starts_with(b"--") {
            arguments.push("--timeout".into());
        }
        arguments.push(OsString::from_vec(native.to_vec()));
        arguments.extend(["--", "/bin/true"].map(OsString::from));
        cases.push(arguments);
    }
    for arguments in cases {
        let mut harness = Harness::new();
        harness.spawn(&arguments);
        let observed = harness.finish();
        assert_eq!(observed.status.code(), Some(2));
        assert!(observed.stdout.is_empty());
        let diagnostic = String::from_utf8_lossy(&observed.stderr);
        if diagnostic.contains(secret) {
            disclosures.push(diagnostic.into_owned());
        }
    }
    assert!(
        disclosures.is_empty(),
        "parser diagnostics disclosed raw supplied values in {} cases:\n{}",
        disclosures.len(),
        disclosures.join("\n")
    );
}

#[test]
fn missing_executable_and_invalid_cwd_are_runner_errors() {
    for args in [
        vec!["run", "--", "/missing/failforge-no-such-executable"],
        vec![
            "run",
            "--cwd",
            "/missing/failforge-no-such-cwd",
            "--",
            "/bin/true",
        ],
    ] {
        let mut harness = Harness::new();
        harness.spawn(&args.into_iter().map(OsString::from).collect::<Vec<_>>());
        let observed = harness.finish();
        assert_eq!(observed.status.code(), Some(125));
        assert!(observed.stdout.is_empty());
    }
}

#[test]
fn deleted_invocation_directory_and_closed_stderr_keep_error_code_125() {
    let mut harness = Harness::new();
    let deleted = harness.directory.join("deleted-cwd");
    fs::create_dir(&deleted).unwrap();
    File::create(harness.directory.join("cli.stdout")).unwrap();
    File::create(harness.directory.join("cli.stderr")).unwrap();
    // Only the spawned shell changes cwd. Its input gate prevents exec until
    // the harness has closed stderr, so this exercises BrokenPipe deterministically.
    let child = Command::new("/bin/sh")
        .args([
            "-c",
            "IFS= read -r gate && cd \"$1\" && /bin/rmdir \"$1\" && exec \"$2\" run -- /bin/true",
            "failforge-test-launch",
        ])
        .arg(&deleted)
        .arg(env!("CARGO_BIN_EXE_failforge"))
        .current_dir(&harness.directory)
        .stdin(Stdio::piped())
        .stdout(File::create(harness.directory.join("cli.stdout")).unwrap())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    harness.child = Some(child);
    drop(harness.child.as_mut().unwrap().stderr.take().unwrap());
    let mut gate = harness.child.as_mut().unwrap().stdin.take().unwrap();
    gate.write_all(b"go\n").unwrap();
    drop(gate);
    assert_eq!(harness.finish().status.code(), Some(125));
}

#[test]
fn capture_is_quiet_and_stream_preserves_exact_binary_bytes() {
    let captured = run_fixture(&[], &["bytes", "257", "513"]);
    assert_eq!(captured.status.code(), Some(0));
    assert!(captured.stdout.is_empty());
    assert!(!captured.stderr.starts_with(&raw_pattern(5)));
    assert!(
        String::from_utf8_lossy(&captured.stderr).contains("stdout_bytes=257 stderr_bytes=513")
    );
    let streamed = run_fixture(&["--stream"], &["bytes", "257", "513"]);
    assert_eq!(streamed.status.code(), Some(0));
    assert_eq!(streamed.stdout, raw_pattern(257));
    assert!(streamed.stderr.starts_with(&raw_pattern(513)));
    assert!(streamed.stderr[513..].starts_with(b"failforge:"));
}

fn stream_arguments(bytes: &str) -> Vec<OsString> {
    [
        "run",
        "--stream",
        "--",
        env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
        "bytes",
        bytes,
        "0",
    ]
    .map(OsString::from)
    .to_vec()
}

#[test]
fn blocked_terminal_does_not_prevent_cleanup_or_cli_exit() {
    let mut harness = Harness::new();
    harness.spawn_with_stdout(&stream_arguments("4194304"), Stdio::piped());
    // Keeping this reader open without consuming it deliberately blocks writes.
    // It is independent of the child's pipes, which the runner must still drain.
    let _blocked_terminal = harness.child.as_mut().unwrap().stdout.take().unwrap();
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(0));
    let summary = String::from_utf8_lossy(&observed.stderr);
    assert!(summary.contains("cleanup=complete"));
    let dropped = summary
        .split("display_dropped_bytes=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let unconfirmed = summary
        .split("display_unconfirmed_bytes=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(dropped.saturating_add(unconfirmed) > 0);
}

#[test]
fn full_nonblocking_stdout_is_temporary_backpressure_like_blocking_stdout() {
    let count = 32 * 1024;
    for nonblocking in [false, true] {
        let mut harness = Harness::new();
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors stores the two fresh descriptors returned by pipe.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: successful pipe produced fresh descriptors with no Rust owner.
        let (reader, writer) = unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                OwnedFd::from_raw_fd(descriptors[1]),
            )
        };
        for descriptor in [&reader, &writer] {
            // SAFETY: the descriptors are owned and F_SETFD takes integer flags.
            assert_eq!(
                unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        let mut filler = File::from(writer);
        let observer = filler.try_clone().unwrap();
        // SAFETY: filler owns this pipe writer; these integer-only operations
        // change only the test sink, before handing it to the CLI.
        let original = unsafe { libc::fcntl(filler.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(original, -1);
        assert_eq!(
            unsafe {
                libc::fcntl(
                    filler.as_raw_fd(),
                    libc::F_SETFL,
                    original | libc::O_NONBLOCK,
                )
            },
            0
        );
        let deadline = Instant::now() + DEADLINE;
        let mut prefilled = 0usize;
        loop {
            match filler.write(&[b'p'; 4096]) {
                Ok(0) => panic!("prefill unexpectedly wrote zero"),
                Ok(bytes) => prefilled += bytes,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("pipe prefill: {error}"),
            }
            assert!(
                Instant::now() < deadline,
                "pipe prefill never reached EAGAIN"
            );
        }
        assert!(prefilled >= 4096);
        let requested_flags = if nonblocking {
            original | libc::O_NONBLOCK
        } else {
            original
        };
        // SAFETY: the writer remains owned; restoration affects the test sink
        // and deliberately supplies either blocking or nonblocking stdio.
        assert_eq!(
            unsafe { libc::fcntl(filler.as_raw_fd(), libc::F_SETFL, requested_flags) },
            0
        );
        // SAFETY: filler still owns the writer. Snapshot the actual status flags
        // after our own changes, including any native flags the OS added.
        let expected_flags = unsafe { libc::fcntl(filler.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(expected_flags, -1);
        let arguments = [
            "run",
            "--stream",
            "--capture-dir",
            "capture",
            "--",
            env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
            "bytes",
            "32768",
            "0",
        ]
        .map(OsString::from);
        harness.spawn_with_stdout(&arguments, Stdio::from(filler));
        // Fixture-owned watchdog remains independent of output backpressure.
        harness.ready();
        let observed = harness.finish();
        let captured_stdout = fs::read(harness.directory.join("capture/stdout.bin")).unwrap();
        let captured_stderr = fs::read(harness.directory.join("capture/stderr.bin")).unwrap();
        eprintln!(
            "nonblocking={nonblocking}, exit_code={:?}, captured_stdout_bytes={}, captured_stderr_bytes={}, stderr={}",
            observed.status.code(),
            captured_stdout.len(),
            captured_stderr.len(),
            String::from_utf8_lossy(&observed.stderr)
        );
        harness.assert_controls_closed();
        // SAFETY: observer owns an unchanged duplicate of the same open file
        // description. This query must not see status-flag changes by the CLI.
        assert_eq!(
            unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) },
            expected_flags
        );
        assert_eq!(captured_stdout, raw_pattern(count));
        assert!(captured_stderr.is_empty());
        assert_eq!(
            observed.status.code(),
            Some(0),
            "nonblocking={nonblocking}, exit_code={:?}, captured_stdout_bytes={}, captured_stderr_bytes={}, stderr={}",
            observed.status.code(),
            captured_stdout.len(),
            captured_stderr.len(),
            String::from_utf8_lossy(&observed.stderr)
        );
        let summary = String::from_utf8_lossy(&observed.stderr);
        assert!(summary.contains("cleanup=complete"));
        assert!(!summary.contains("stream-error"));
    }
}

#[test]
fn nonblocking_stdout_and_stderr_recover_when_the_healthy_reader_drains() {
    let count = 32 * 1024;
    for stderr_sink in [false, true] {
        let mut harness = Harness::new();
        let FullPipe {
            mut reader,
            writer,
            observer,
            prefilled,
            flags,
        } = full_nonblocking_pipe();
        let amounts = if stderr_sink {
            ["0", "32768"]
        } else {
            ["32768", "0"]
        };
        let mut arguments = [
            "run",
            "--stream",
            "--capture-dir",
            "capture",
            "--",
            env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
            "bytes",
        ]
        .map(OsString::from)
        .to_vec();
        arguments.extend(amounts.map(OsString::from));
        if stderr_sink {
            let stdout = File::create(harness.directory.join("cli.stdout")).unwrap();
            harness.spawn_with_sinks(&arguments, Stdio::from(stdout), Stdio::from(writer));
        } else {
            harness.spawn_with_stdout(&arguments, Stdio::from(writer));
        }
        harness.ready();
        let capture_name = if stderr_sink {
            "stderr.bin"
        } else {
            "stdout.bin"
        };
        let capture_path = harness.directory.join("capture").join(capture_name);
        let deadline = Instant::now() + DEADLINE;
        // Positive raw capture proves output has begun while the destination
        // remains full. READY alone is not an output-start observation.
        while !fs::metadata(&capture_path).is_ok_and(|metadata| metadata.len() > 0) {
            assert!(
                Instant::now() < deadline,
                "fixture output never reached capture"
            );
            std::thread::park_timeout(Duration::from_millis(2));
        }
        let mut displayed = Vec::new();
        loop {
            drain_available(&mut reader, &mut displayed, prefilled + count + 64 * 1024);
            if harness
                .child
                .as_mut()
                .unwrap()
                .try_wait()
                .unwrap()
                .is_some()
            {
                drain_available(&mut reader, &mut displayed, prefilled + count + 64 * 1024);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not recover after EAGAIN"
            );
            std::thread::park_timeout(Duration::from_millis(2));
        }
        let observed = harness.finish();
        assert_eq!(
            observed.status.code(),
            Some(0),
            "stderr_sink={stderr_sink}, diagnostics={}",
            String::from_utf8_lossy(&observed.stderr)
        );
        harness.assert_controls_closed();
        // SAFETY: observer retains the same writer's open file description.
        assert_eq!(
            unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) },
            flags
        );
        assert_eq!(fs::read(capture_path).unwrap(), raw_pattern(count));
        assert_eq!(&displayed[..prefilled], vec![b'p'; prefilled]);
        let child_output = &displayed[prefilled..];
        let summary = if stderr_sink {
            assert_eq!(&child_output[..count], raw_pattern(count));
            assert!(observed.stdout.is_empty());
            String::from_utf8_lossy(&child_output[count..]).into_owned()
        } else {
            assert_eq!(child_output, raw_pattern(count));
            String::from_utf8_lossy(&observed.stderr).into_owned()
        };
        assert!(summary.contains("cleanup=complete"), "{summary}");
        assert!(
            summary.contains("display_dropped_bytes=0 display_unconfirmed_bytes=0"),
            "{summary}"
        );
        assert!(!summary.contains("stream-error"), "{summary}");
    }
}

#[test]
fn nonblocking_stdout_still_reports_a_closed_reader_as_fatal() {
    let mut harness = Harness::new();
    let FullPipe {
        reader,
        writer,
        observer,
        flags,
        ..
    } = full_nonblocking_pipe();
    drop(reader);
    harness.spawn_with_stdout(&stream_arguments("4194304"), Stdio::from(writer));
    harness.ready();
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(125));
    harness.assert_controls_closed();
    assert!(String::from_utf8_lossy(&observed.stderr).contains("stdout_issue=stream-error"));
    // SAFETY: observer owns a live writer duplicate even after its reader closed.
    assert_eq!(
        unsafe { libc::fcntl(observer.as_raw_fd(), libc::F_GETFL) },
        flags
    );
}

#[test]
fn blocked_stderr_does_not_prevent_capture_cleanup_or_cli_exit() {
    let mut harness = Harness::new();
    let mut descriptors = [-1; 2];
    // SAFETY: descriptors provides writable storage for two fresh pipe FDs.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    // SAFETY: pipe succeeded and these values adopt its two FDs exactly once.
    let (reader, writer) = unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    };
    for descriptor in [&reader, &writer] {
        // SAFETY: each descriptor has a live owner; F_SETFD takes no pointers.
        assert_eq!(
            unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    // Retain a CLOEXEC writer only to observe POLLOUT without consuming data.
    // The actual stderr sink is a native pipe, shared by display and diagnostics.
    let observer = writer.try_clone().unwrap();
    let mut reader = File::from(reader);
    File::create(harness.directory.join("cli.stderr")).unwrap();
    harness.child = Some(
        Command::new(env!("CARGO_BIN_EXE_failforge"))
            .args([
                "run",
                "--stream",
                "--capture-dir",
                "capture",
                "--",
                env!("CARGO_BIN_EXE_failforge-cli-process-fixture"),
                "bytes",
                "0",
                "4194304",
            ])
            .env("FAILFORGE_CONTROL", &harness.control_path)
            .current_dir(&harness.directory)
            .stdin(Stdio::null())
            .stdout(File::create(harness.directory.join("cli.stdout")).unwrap())
            .stderr(Stdio::from(writer))
            .spawn()
            .unwrap(),
    );
    // READY and emergency EOF cleanup use a separate fixture-owned socket.
    harness.ready();
    let deadline = Instant::now() + DEADLINE;
    loop {
        let mut readiness = libc::pollfd {
            fd: observer.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: readiness is valid storage for one pollfd; timeout is zero.
        let ready = unsafe { libc::poll(&mut readiness, 1, 0) };
        if ready == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
        {
            continue;
        }
        assert!(ready >= 0, "stderr readiness probe failed");
        assert_eq!(readiness.revents & (libc::POLLERR | libc::POLLNVAL), 0);
        if readiness.revents & libc::POLLOUT == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "stderr pipe never filled");
        std::thread::park_timeout(Duration::from_millis(2));
    }
    // Keep the pipe unread until the entire CLI exits. A diagnostic delivery
    // deadline may expire, but capture and process cleanup must still finish.
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(0));
    assert!(observed.stdout.is_empty());
    harness.assert_controls_closed();
    assert_eq!(
        fs::read(harness.directory.join("capture/stderr.bin")).unwrap(),
        raw_pattern(4 * 1024 * 1024)
    );
    assert!(
        fs::read(harness.directory.join("capture/stdout.bin"))
            .unwrap()
            .is_empty()
    );

    drop(observer);
    // SAFETY: reader owns this FD; these integer-only queries change only the
    // test reader after CLI exit, never controller or child stdio flags.
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let mut displayed = Vec::new();
    reader.read_to_end(&mut displayed).unwrap();
    assert!(
        displayed.len() >= 4096,
        "stderr pipe had no meaningful backpressure"
    );
    assert_eq!(&displayed[..4096], raw_pattern(4096));
    // A missing final summary is permitted when stderr delivery is blocked.
}

#[test]
fn broken_terminal_sink_is_a_runner_failure_with_partial_result() {
    let mut harness = Harness::new();
    harness.spawn_with_stdout(&stream_arguments("4194304"), Stdio::piped());
    // Closing the read end makes the display writer receive BrokenPipe.
    drop(harness.child.as_mut().unwrap().stdout.take().unwrap());
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&observed.stderr).contains("partial result"));
    assert!(String::from_utf8_lossy(&observed.stderr).contains("cause="));
}

#[test]
fn capture_directory_is_relative_to_invocation_and_existing_logs_are_preserved() {
    let mut harness = Harness::new();
    fs::create_dir(harness.directory.join("child")).unwrap();
    harness.spawn_fixture(
        &["--cwd", "child", "--capture-dir", "artifacts"],
        &["bytes", "71", "33"],
    );
    assert_eq!(harness.finish().status.code(), Some(0));
    assert_eq!(
        fs::read(harness.directory.join("artifacts/stdout.bin")).unwrap(),
        raw_pattern(71)
    );
    assert_eq!(
        fs::read(harness.directory.join("artifacts/stderr.bin")).unwrap(),
        raw_pattern(33)
    );
    assert!(!harness.directory.join("child/artifacts").exists());
    harness.spawn_fixture(&["--capture-dir", "artifacts"], &["bytes", "1", "1"]);
    assert_eq!(harness.finish().status.code(), Some(125));
    assert_eq!(
        fs::read(harness.directory.join("artifacts/stdout.bin")).unwrap(),
        raw_pattern(71)
    );
    assert_eq!(
        fs::read(harness.directory.join("artifacts/stderr.bin")).unwrap(),
        raw_pattern(33)
    );
}

#[test]
fn native_arguments_and_empty_arguments_reach_the_child() {
    let mut harness = Harness::new();
    harness.spawn(&[
        OsString::from("run"),
        OsString::from("--stream"),
        OsString::from("--"),
        OsString::from(env!("CARGO_BIN_EXE_failforge-cli-process-fixture")),
        OsString::from("argv"),
        OsString::from("a b"),
        OsString::new(),
        OsString::from("*;$()"),
        OsString::from_vec(vec![255, 128]),
    ]);
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(0));
    assert_eq!(observed.stdout, b"612062\n\n2a3b242829\nff80\n");
}

#[test]
fn environment_set_remove_clear_and_last_assignment_work() {
    let inherited = run_fixture(&["--stream"], &["env", "FF_CLI_PARENT"]);
    assert_eq!(inherited.status.code(), Some(0));
    assert_eq!(inherited.stdout, b"706172656e742d76616c7565\n");
    let set = run_fixture(
        &[
            "--stream",
            "--env",
            "FF_CLI_VALUE=old",
            "--env",
            "FF_CLI_VALUE=a=b",
        ],
        &["env", "FF_CLI_VALUE"],
    );
    assert_eq!(set.status.code(), Some(0));
    assert_eq!(set.stdout, b"613d62\n");
    let removed = run_fixture(&["--stream", "--env-remove", "PATH"], &["env", "PATH"]);
    assert_eq!(removed.status.code(), Some(0));
    assert_eq!(removed.stdout, b"<unset>\n");
    let clear = run_fixture(&["--stream", "--clear-env"], &["env", "PATH"]);
    assert_eq!(clear.status.code(), Some(0));
    assert_eq!(clear.stdout, b"<unset>\n");
    let empty = run_fixture(
        &["--stream", "--env", "FF_CLI_VALUE="],
        &["env", "FF_CLI_VALUE"],
    );
    assert_eq!(empty.stdout, b"\n");
}

#[test]
fn timeout_terminates_an_ignoring_child_and_keeps_original_cause() {
    let mut harness = Harness::new();
    harness.spawn_fixture(&["--timeout", "2s", "--grace-period", "0ms"], &["ignore"]);
    harness.ready();
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(124));
    harness.assert_controls_closed();
    assert!(String::from_utf8_lossy(&observed.stderr).contains("cause=TimedOut status=signal:9"));
    assert!(String::from_utf8_lossy(&observed.stderr).contains("cleanup=complete"));
}

#[test]
fn cli_ctrl_c_and_sigterm_request_managed_cancellation() {
    for (signal, code, reason) in [
        (libc::SIGINT, 130, "CtrlC"),
        (libc::SIGTERM, 143, "SigTerm"),
    ] {
        let mut harness = Harness::new();
        harness.spawn_fixture(&["--grace-period", "0ms"], &["ignore"]);
        harness.ready();
        harness.signal(signal);
        let observed = harness.finish();
        assert_eq!(observed.status.code(), Some(code));
        harness.assert_controls_closed();
        assert!(
            String::from_utf8_lossy(&observed.stderr)
                .contains(&format!("cause=Cancelled({reason})"))
        );
        assert!(String::from_utf8_lossy(&observed.stderr).contains("cleanup=complete"));
    }
}

#[test]
fn graceful_ctrl_c_keeps_the_actual_zero_status() {
    let mut harness = Harness::new();
    harness.spawn_fixture(&["--grace-period", "2s"], &["graceful"]);
    harness.ready();
    harness.signal(libc::SIGINT);
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(130));
    harness.assert_controls_closed();
    assert!(
        String::from_utf8_lossy(&observed.stderr).contains("cause=Cancelled(CtrlC) status=exit:0")
    );
}

#[test]
fn sigkill_of_controller_is_outside_cleanup_guarantee() {
    let mut harness = Harness::new();
    harness.spawn_fixture(&[], &["ignore"]);
    harness.ready();
    // Killing only the owned CLI is intentional. The independent helper
    // control channel below is the external cleanup harness for this boundary.
    harness.signal(libc::SIGKILL);
    let observed = harness.finish();
    assert_eq!(observed.status.signal(), Some(libc::SIGKILL));

    let control = &mut harness.controls[0];
    control.set_nonblocking(true).unwrap();
    let still_open = control.read(&mut [0]);
    assert!(
        matches!(still_open, Err(error) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
        "the managed helper unexpectedly disappeared with its controller"
    );
    // No numeric PID/PGID is used for a late kill. Fixture-controlled exit and
    // socket EOF verify cleanup independently even after the CLI was reaped.
    control.write_all(b"exit 0\n").unwrap();
    harness.assert_controls_closed();
}

#[test]
fn second_ctrl_c_skips_a_long_grace_period() {
    let mut harness = Harness::new();
    harness.spawn_fixture(&["--grace-period", "30s"], &["ignore"]);
    harness.ready();
    harness.signal(libc::SIGINT);
    harness.wait_for_diagnostic("cancellation requested (Ctrl+C)");
    let before = Instant::now();
    harness.signal(libc::SIGINT);
    let observed = harness.finish();
    assert!(before.elapsed() < DEADLINE);
    assert_eq!(observed.status.code(), Some(130));
    harness.assert_controls_closed();
    assert!(String::from_utf8_lossy(&observed.stderr).contains("status=signal:9"));
}

#[test]
fn signal_status_is_mapped_without_reclassifying_as_cancellation() {
    let mut harness = Harness::new();
    harness.spawn(&["run", "--", "/bin/sh", "-c", "kill -TERM $$"].map(OsString::from));
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(143));
    assert!(String::from_utf8_lossy(&observed.stderr).contains(&format!(
        "cause=ProcessCompleted status=signal:{}",
        libc::SIGTERM
    )));
}

#[test]
fn stdin_is_null_and_cwd_is_effective() {
    let eof = run_fixture(&["--stream"], &["stdin-eof"]);
    assert_eq!(eof.status.code(), Some(0));
    // The fixture uses an empty stdout and exit 0 to confirm EOF.
    assert!(eof.stdout.is_empty());
    let mut harness = Harness::new();
    fs::create_dir(harness.directory.join("child")).unwrap();
    harness.spawn_fixture(&["--cwd", "child", "--stream"], &["cwd"]);
    let observed = harness.finish();
    assert_eq!(observed.status.code(), Some(0));
    let effective = fs::canonicalize(harness.directory.join("child")).unwrap();
    let expected = effective
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        + "\n";
    assert_eq!(observed.stdout, expected.as_bytes());
}
