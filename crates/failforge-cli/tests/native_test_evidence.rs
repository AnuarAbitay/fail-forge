//! Native owner/evidence transport contract, using short real child commands.
#![cfg(unix)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const ENV: &str = "FAILFORGE_TEST_EVIDENCE_FD";
const CONTEXT_ENV: &str = "FAILFORGE_TEST_CONTEXT_FD";
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf, Option<Child>);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff-evidence-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path, None)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_failforge"));
        command
            .current_dir(&self.0)
            .stdin(Stdio::null())
            .env_remove(ENV)
            .env_remove(CONTEXT_ENV)
            .env_remove("FAILFORGE_WORKSPACE_LEASE_FD")
            .env_remove("FAILFORGE_RUNNER_READY_FD");
        command
    }

    fn execute(&mut self, command: &mut Command) -> Output {
        command
            .stdout(File::create(self.0.join("owner.stdout")).unwrap())
            .stderr(File::create(self.0.join("owner.stderr")).unwrap());
        self.1 = Some(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(8);
        let status = loop {
            if let Some(status) = self.1.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "native evidence owner exceeded deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        self.1.take();
        Output {
            status,
            stdout: fs::read(self.0.join("owner.stdout")).unwrap(),
            stderr: fs::read(self.0.join("owner.stderr")).unwrap(),
        }
    }

    fn evidence(&self, command: &mut Command) -> File {
        let path = self.0.join("private");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        fs::remove_file(path).unwrap();
        let descriptor = file.as_raw_fd();
        command.env(ENV, descriptor.to_string());
        // SAFETY: the closure performs only async-signal-safe fcntl calls on
        // this retained descriptor. The parent keeps its own CLOEXEC flag.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(descriptor, libc::F_GETFD);
                if flags < 0
                    || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        file
    }

    fn context(&self, command: &mut Command, bytes: &[u8]) -> File {
        let path = self.0.join("context");
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        fs::remove_file(path).unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        let descriptor = file.as_raw_fd();
        command.env(CONTEXT_ENV, descriptor.to_string());
        // SAFETY: only async-signal-safe fcntl calls on a retained descriptor.
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(descriptor, libc::F_GETFD);
                if flags < 0
                    || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        file
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.1 {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn raw(failed: bool, flutter: bool) -> Vec<u8> {
    let mut events = vec![
        json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":if flutter {Value::Null}else {"1.33.0".into()},"pid":100}),
        json!({"type":"suite","time":1,"suite":{"id":0,"platform":"vm","path":"test/value_test.dart"}}),
        json!({"type":"group","time":2,"group":{"id":1,"suiteID":0,"parentID":null,"name":"","testCount":1,"metadata":{"skip":false}}}),
        json!({"type":"testStart","time":3,"test":{"id":2,"suiteID":0,"groupIDs":[1],"name":"author","metadata":{"skip":false}}}),
    ];
    if failed {
        events.push(json!({"type":"error","time":4,"testID":2,"error":"Expected 1, actual 2","stackTrace":"test/value_test.dart 1:1","isFailure":true}));
    }
    events.extend([
        json!({"type":"testDone","time":5,"testID":2,"result":if failed {"failure"}else {"success"},"hidden":false,"skipped":false}),
        json!({"type":"allSuites","time":6,"count":1}),
        json!({"type":"done","time":7,"success":!failed}),
    ]);
    events
        .into_iter()
        .flat_map(|event| {
            let mut bytes = serde_json::to_vec(&event).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect()
}

fn report(file: &mut File) -> Value {
    file.rewind().unwrap();
    let mut raw = Vec::new();
    file.read_to_end(&mut raw).unwrap();
    assert_eq!(raw.last(), Some(&b'\n'), "envelope is not committed");
    serde_json::from_slice(&raw).unwrap()
}

#[test]
fn real_native_runs_publish_bound_dart_and_flutter_outcomes_without_changing_exit_codes() {
    for (failed, flutter) in [(false, false), (true, false), (false, true)] {
        let mut fixture = Fixture::new();
        let bytes = raw(failed, flutter);
        fs::write(fixture.0.join("events"), &bytes).unwrap();
        let mode = if flutter { "flutter" } else { "dart" };
        let code = if failed { "1" } else { "0" };
        let mut command = fixture.command();
        command.args([
            "run",
            "--timeout",
            "3s",
            "--capture-dir",
            "capture",
            "--test-reporter",
            mode,
            "--",
            "/bin/sh",
            "-c",
            "cat events; printf diagnostic >&2; exit \"$1\"",
            "sh",
            code,
        ]);
        let mut handoff = fixture.evidence(&mut command);
        let output = fixture.execute(&mut command);
        assert_eq!(
            output.status.code(),
            Some(if failed { 1 } else { 0 }),
            "{:?}",
            output.stderr
        );
        let evidence = report(&mut handoff);
        assert_eq!(
            evidence["classification"]["outcome"],
            if failed { "KILLED" } else { "SURVIVED" }
        );
        assert_eq!(evidence["reporterMode"], mode);
        assert_eq!(evidence["stdoutSha256"], failforge_dart::sha256_hex(&bytes));
        assert_eq!(
            evidence["stderrSha256"],
            failforge_dart::sha256_hex(b"diagnostic")
        );
        assert_eq!(evidence["ownerExitCode"], if failed { 1 } else { 0 });
        assert_eq!(evidence["version"], 3);
        assert_eq!(
            evidence["classification"]["testIdentities"],
            json!([["test/value_test.dart", "author"]])
        );
        assert_eq!(evidence["catalogue"]["rawSha256"], evidence["stdoutSha256"]);
    }
}

#[test]
fn overflowing_integer_metadata_cannot_publish_a_killed_verdict() {
    for (integer, rejected) in [
        ("18446744073709551615", false),
        ("18446744073709551616", true),
        ("18446744073709551617", true),
        ("-9223372036854775808", false),
        ("-9223372036854775809", true),
    ] {
        let mut fixture = Fixture::new();
        // Keep the literal numeric token: constructing serde_json::Value first
        // would already lose precision before the native reader sees the test.
        let events = String::from_utf8(raw(true, false)).unwrap().replace(
            "\"metadata\":{\"skip\":false}",
            &format!("\"metadata\":{{\"identity\":{integer},\"skip\":false}}"),
        );
        assert!(events.contains(&format!("\"identity\":{integer}")));
        fs::write(fixture.0.join("events"), events).unwrap();
        let mut command = fixture.command();
        command.args([
            "run",
            "--timeout",
            "3s",
            "--capture-dir",
            "capture",
            "--test-reporter",
            "dart",
            "--",
            "/bin/sh",
            "-c",
            "cat events; exit 1",
        ]);
        let mut handoff = fixture.evidence(&mut command);
        let output = fixture.execute(&mut command);
        assert_eq!(output.status.code(), Some(1));
        let evidence = report(&mut handoff);
        assert_eq!(
            evidence["classification"]["outcome"],
            if rejected { "INFRA_ERROR" } else { "KILLED" },
            "{integer}"
        );
        if rejected {
            assert_eq!(
                evidence["classification"]["reason"],
                "integer_limit_exceeded"
            );
            assert!(evidence["catalogue"].is_null());
        }
    }
}

#[test]
fn timeout_preserves_partial_reporter_evidence_and_owner_code() {
    let mut fixture = Fixture::new();
    let mut command = fixture.command();
    command.args([
        "run",
        "--timeout",
        "100ms",
        "--grace-period",
        "10ms",
        "--capture-dir",
        "capture",
        "--test-reporter",
        "dart",
        "--",
        "/bin/sh",
        "-c",
        "printf '{'; exec /bin/sleep 5",
    ]);
    let mut handoff = fixture.evidence(&mut command);
    let output = fixture.execute(&mut command);
    assert_eq!(output.status.code(), Some(124));
    let evidence = report(&mut handoff);
    assert_eq!(evidence["classification"]["outcome"], "TIMEOUT");
    assert_eq!(evidence["classification"]["testsPartial"], true);
    assert_eq!(evidence["ownerExitCode"], 124);
}

#[test]
fn missing_capability_fails_before_spawning_a_child_and_requires_capture() {
    for capture in [true, false] {
        let mut fixture = Fixture::new();
        let mut command = fixture.command();
        command.args(["run", "--test-reporter", "dart"]);
        if capture {
            command.args(["--capture-dir", "capture"]);
        }
        command.args(["--", "/bin/sh", "-c", "touch child-started"]);
        let output = fixture.execute(&mut command);
        assert_eq!(output.status.code(), Some(if capture { 125 } else { 2 }));
        assert!(!fixture.0.join("child-started").exists());
    }
}

#[test]
fn child_cannot_inherit_private_descriptor_or_explicit_environment_override() {
    let mut fixture = Fixture::new();
    let mut command = fixture.command();
    command.args([
        "run",
        "--timeout",
        "3s",
        "--capture-dir",
        "capture",
        "--test-reporter",
        "dart",
        "--env",
        "FAILFORGE_TEST_EVIDENCE_FD=leak",
        "--env",
        "FAILFORGE_TEST_CONTEXT_FD=leak",
        "--",
        "/bin/sh",
        "-c",
        "test -z \"${FAILFORGE_TEST_EVIDENCE_FD+x}\" && test -z \"${FAILFORGE_TEST_CONTEXT_FD+x}\" && test ! -e \"/dev/fd/$1\" && test ! -e \"/dev/fd/$2\"; exit $?",
        "sh",
    ]);
    let mut handoff = fixture.evidence(&mut command);
    let context = fixture.context(
        &mut command,
        b"{\"expected\":null,\"baseline\":null,\"mutant\":null,\"alias\":\"subject\"}\n",
    );
    command
        .arg(handoff.as_raw_fd().to_string())
        .arg(context.as_raw_fd().to_string());
    let output = fixture.execute(&mut command);
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(
        report(&mut handoff)["classification"]["outcome"],
        "INFRA_ERROR"
    );
}

#[test]
fn same_length_capture_replacement_before_publication_cannot_forge_a_verdict() {
    for stream in ["stdout", "stderr"] {
        let mut fixture = Fixture::new();
        let mut actual = raw(false, false);
        let forged = raw(true, false);
        actual.resize(forged.len(), b' ');
        assert_eq!(actual.len(), forged.len());
        fs::write(fixture.0.join("events"), &actual).unwrap();
        fs::write(
            fixture.0.join("forged"),
            if stream == "stdout" {
                forged.as_slice()
            } else {
                b"alteration"
            },
        )
        .unwrap();
        let mut command = fixture.command();
        command.args(["run", "--timeout", "3s", "--capture-dir", "capture",
            "--test-reporter", "dart", "--", "/bin/sh", "-c",
            "cat events; printf diagnostic >&2; while [ \"$(wc -c < \"capture/$1.bin\")\" -lt \"$2\" ]; do sleep 0.01; done; cat forged > \"capture/$1.bin\"; exit \"$3\"", "sh", stream]);
        command
            .arg(if stream == "stdout" { actual.len() } else { 10 }.to_string())
            .arg(if stream == "stdout" { "1" } else { "0" });
        let mut handoff = fixture.evidence(&mut command);
        let output = fixture.execute(&mut command);
        assert_eq!(output.status.code(), Some(125), "{:?}", output.stderr);
        assert_eq!(
            fs::read(fixture.0.join(format!("capture/{stream}.bin"))).unwrap(),
            fs::read(fixture.0.join("forged")).unwrap()
        );
        handoff.rewind().unwrap();
        let mut envelope = Vec::new();
        handoff.read_to_end(&mut envelope).unwrap();
        assert!(
            envelope.is_empty(),
            "changed capture must never publish a verdict"
        );
    }
}

#[test]
fn malformed_context_cannot_start_tests_or_publish_a_verdict() {
    for bytes in [
        b"{}".as_slice(),
        b"{\"alias\":\"a\",\"alias\":\"b\"}\n",
        b"[]\n",
    ] {
        let mut fixture = Fixture::new();
        let mut command = fixture.command();
        command.args([
            "run",
            "--capture-dir",
            "capture",
            "--test-reporter",
            "dart",
            "--",
            "/bin/sh",
            "-c",
            "touch child-started",
        ]);
        let handoff = fixture.evidence(&mut command);
        let _context = fixture.context(&mut command, bytes);
        let output = fixture.execute(&mut command);
        assert_eq!(output.status.code(), Some(125));
        assert!(!fixture.0.join("child-started").exists());
        assert_eq!(handoff.metadata().unwrap().len(), 0);
    }
    let mut fixture = Fixture::new();
    let mut command = fixture.command();
    command.args(["run", "--", "/bin/sh", "-c", "touch child-started"]);
    let _context = fixture.context(&mut command, b"{\"alias\":\"subject\"}\n");
    let output = fixture.execute(&mut command);
    assert_eq!(output.status.code(), Some(125));
    assert!(!fixture.0.join("child-started").exists());
}

#[test]
fn native_catalogue_comparison_survives_baseline_cleanup_and_rejects_changed_selection() {
    let baseline = {
        let mut fixture = Fixture::new();
        fs::write(fixture.0.join("events"), raw(false, false)).unwrap();
        let mut command = fixture.command();
        command.args([
            "run",
            "--capture-dir",
            "capture",
            "--test-reporter",
            "dart",
            "--",
            "/bin/cat",
            "events",
        ]);
        let mut handoff = fixture.evidence(&mut command);
        assert!(fixture.execute(&mut command).status.success());
        report(&mut handoff)["catalogue"].clone()
    };
    assert!(!PathBuf::from(baseline["cwd"].as_str().unwrap()).exists());
    for changed in [false, true] {
        let mut fixture = Fixture::new();
        let raw = String::from_utf8(raw(true, false)).unwrap();
        fs::write(
            fixture.0.join("events"),
            if changed {
                raw.replace("author", "different author")
            } else {
                raw
            },
        )
        .unwrap();
        let mut command = fixture.command();
        command.args([
            "run",
            "--capture-dir",
            "capture",
            "--test-reporter",
            "dart",
            "--",
            "/bin/sh",
            "-c",
            "cat events; exit 1",
        ]);
        let mut handoff = fixture.evidence(&mut command);
        let mut context = serde_json::to_vec(&json!({
            "alias":"subject", "expected": [["test/value_test.dart", "author"]],
            "baseline":baseline, "mutant":{"file":"lib/value.dart"},
        }))
        .unwrap();
        context.push(b'\n');
        let _context = fixture.context(&mut command, &context);
        let output = fixture.execute(&mut command);
        assert_eq!(output.status.code(), Some(1), "{:?}", output.stderr);
        let classification = report(&mut handoff)["classification"].clone();
        assert_eq!(
            classification["outcome"],
            if changed { "INFRA_ERROR" } else { "KILLED" }
        );
        assert_eq!(
            classification["reason"],
            if changed {
                "author_test_selection_changed"
            } else {
                "user_test_failure"
            }
        );
    }
}

fn initialization_run(bytes: &[u8], context: Option<&Value>, failed: bool) -> Value {
    let mut fixture = Fixture::new();
    fs::write(fixture.0.join("events"), bytes).unwrap();
    let mut command = fixture.command();
    command.args([
        "run",
        "--capture-dir",
        "capture",
        "--test-reporter",
        "dart",
        "--",
        "/bin/sh",
        "-c",
        "cat events; exit \"$1\"",
        "sh",
        if failed { "1" } else { "0" },
    ]);
    let mut handoff = fixture.evidence(&mut command);
    let _context = context.map(|context| {
        let mut bytes = serde_json::to_vec(context).unwrap();
        bytes.push(b'\n');
        fixture.context(&mut command, &bytes)
    });
    let output = fixture.execute(&mut command);
    assert_eq!(
        output.status.code(),
        Some(i32::from(failed)),
        "{:?}",
        output.stderr
    );
    report(&mut handoff)
}

fn loader_failure() -> Vec<u8> {
    [
        json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":100}),
        json!({"type":"suite","time":1,"suite":{"id":0,"platform":"vm","path":"test/value_test.dart"}}),
        json!({"type":"testStart","time":2,"test":{"id":1,"suiteID":0,"groupIDs":[],"name":"loading test/value_test.dart","metadata":{"skip":false}}}),
        json!({"type":"error","time":3,"testID":1,"error":"Failed to load \"test/value_test.dart\": Bad state: initialization failed","stackTrace":"package:subject/value.dart 1:1 initialize\ntest/value_test.dart 3:1 main\n","isFailure":false}),
        json!({"type":"testDone","time":4,"testID":1,"result":"error","hidden":false,"skipped":false}),
        json!({"type":"allSuites","time":5,"count":1}),
        json!({"type":"done","time":6,"success":false}),
    ].into_iter().flat_map(|event| format!("{event}\n").into_bytes()).collect()
}

#[test]
fn native_initialization_requires_ordered_controls_and_rejects_rebound_state_before_spawn() {
    let baseline = initialization_run(&raw(false, false), None, false);
    let mut context = json!({
        "alias":"subject", "expected": [["test/value_test.dart", "author"]],
        "baseline": baseline["catalogue"], "mutant":{"id":"candidate", "file":"lib/value.dart"},
        "initialization":{"role":"initial", "candidateValid":true,
            "baseline":baseline["initialization"]["observation"]},
    });
    let initial = initialization_run(&loader_failure(), Some(&context), true);
    assert_eq!(initial["classification"]["outcome"], "LOAD_FAILURE");
    assert_eq!(
        initial["initialization"]["decision"]["nextRole"],
        "original-before"
    );
    assert_eq!(initial["initialization"]["decision"]["confirmed"], false);
    let mut state = initial["initialization"]["state"].clone();

    for change in ["role", "candidate", "alias", "expected"] {
        let mut invalid = context.clone();
        invalid["initialization"] = json!({"role":"original-before", "state":state});
        match change {
            "role" => invalid["initialization"]["role"] = "original-after".into(),
            "candidate" => invalid["mutant"]["id"] = "different".into(),
            "alias" => invalid["alias"] = "different".into(),
            _ => invalid["expected"][0][1] = "different".into(),
        }
        let mut fixture = Fixture::new();
        let mut command = fixture.command();
        command.args([
            "run",
            "--capture-dir",
            "capture",
            "--test-reporter",
            "dart",
            "--",
            "/bin/sh",
            "-c",
            "touch child-started",
        ]);
        let handoff = fixture.evidence(&mut command);
        let bytes = format!("{invalid}\n");
        let _context = fixture.context(&mut command, bytes.as_bytes());
        let output = fixture.execute(&mut command);
        assert_eq!(output.status.code(), Some(125), "{change}");
        assert!(!fixture.0.join("child-started").exists(), "{change}");
        assert_eq!(handoff.metadata().unwrap().len(), 0, "{change}");
    }
    for (role, next, failed) in [
        ("original-before", Some("mutant-replay"), false),
        ("mutant-replay", Some("original-after"), true),
        ("original-after", None, false),
    ] {
        context["initialization"] = json!({"role":role, "state":state});
        let bytes = if failed {
            loader_failure()
        } else {
            raw(false, false)
        };
        let result = initialization_run(&bytes, Some(&context), failed);
        let decision = &result["initialization"]["decision"];
        assert_eq!(decision["nextRole"], json!(next));
        assert_eq!(decision["confirmed"], next.is_none());
        assert_eq!(
            decision["outcome"],
            if next.is_none() {
                "KILLED"
            } else {
                "UNCLASSIFIED"
            }
        );
        if next.is_none() {
            assert_eq!(decision["reason"], "mutation_caused_initialization_failure");
        }
        state = result["initialization"]["state"].clone();
    }
    assert!(state.is_null());
}
