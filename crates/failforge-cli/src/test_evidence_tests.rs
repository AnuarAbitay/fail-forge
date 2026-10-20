use super::*;
use failforge_runner::{CleanupReport, CompletionCause, GroupState};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff-evidence-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str, anonymous: bool) -> File {
        let path = self.0.join(name);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        if anonymous {
            std::fs::remove_file(path).unwrap();
        }
        file
    }

    fn capture(&self, name: &str, raw: &[u8]) -> CaptureSummary {
        let path = self.0.join(name);
        std::fs::write(&path, raw).unwrap();
        CaptureSummary {
            sha256: Some(Sha256::digest(raw).into()),
            bytes_read: raw.len() as u64,
            file_bytes_written: raw.len() as u64,
            file_path: Some(path),
            eof: true,
            writer_finished: true,
            ..Default::default()
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn adopt(file: &File) -> io::Result<EvidenceHandoff> {
    EvidenceHandoff::from_value(
        Some(OsStr::new(&file.as_raw_fd().to_string())),
        Some(TestReporter::Dart),
    )
    .map(Option::unwrap)
}

#[test]
fn optional_capability_requires_explicit_reporter_and_private_descriptor() {
    assert!(EvidenceHandoff::from_value(None, None).unwrap().is_none());
    assert!(EvidenceHandoff::from_value(None, Some(TestReporter::Dart)).is_err());
    for text in [
        "",
        "0",
        "1",
        "2",
        "+3",
        "-3",
        " 3",
        "3 ",
        "99999999999999999999",
    ] {
        assert!(
            EvidenceHandoff::from_value(Some(OsStr::new(text)), Some(TestReporter::Dart)).is_err()
        );
    }
    let fixture = Fixture::new();
    let file = fixture.file("private", true);
    assert!(
        EvidenceHandoff::from_value(Some(OsStr::new(&file.as_raw_fd().to_string())), None).is_err()
    );
    let evidence = adopt(&file).unwrap();
    for fd in [file.as_raw_fd(), evidence.file.as_raw_fd()] {
        // SAFETY: both descriptors have live owners; flags are only queried.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
    }
}

#[test]
fn named_nonempty_seeked_or_public_files_cannot_receive_evidence() {
    let fixture = Fixture::new();
    let named = fixture.file("named", false);
    assert!(adopt(&named).is_err());
    let mut nonempty = fixture.file("nonempty", true);
    nonempty.write_all(b"old").unwrap();
    nonempty.rewind().unwrap();
    assert!(adopt(&nonempty).is_err());
    let mut seeked = fixture.file("seeked", true);
    seeked.seek(std::io::SeekFrom::Start(1)).unwrap();
    assert!(adopt(&seeked).is_err());
    let public = fixture.file("public", true);
    public
        .set_permissions(std::fs::Permissions::from_mode(0o640))
        .unwrap();
    assert!(adopt(&public).is_err());
    let directory = File::open(&fixture.0).unwrap();
    assert!(adopt(&directory).is_err());
}

#[test]
fn child_cannot_observe_even_an_explicit_evidence_environment_override() {
    let fixture = Fixture::new();
    let file = fixture.file("private", true);
    let evidence = adopt(&file).unwrap();
    let mut spec = CommandSpec {
        program: "unused".into(),
        args: vec![],
        cwd: fixture.0.clone(),
        env: Default::default(),
        timeout_policy: Default::default(),
        capture_policy: Default::default(),
    };
    spec.env.set.insert(EVIDENCE_ENV.into(), "override".into());
    spec.env
        .set
        .insert(crate::test_context::CONTEXT_ENV.into(), "override".into());
    evidence.protect_child(&mut spec);
    assert!(!spec.env.set.contains_key(OsStr::new(EVIDENCE_ENV)));
    assert!(spec.env.remove.contains(OsStr::new(EVIDENCE_ENV)));
    assert!(
        !spec
            .env
            .set
            .contains_key(OsStr::new(crate::test_context::CONTEXT_ENV))
    );
    assert!(
        spec.env
            .remove
            .contains(OsStr::new(crate::test_context::CONTEXT_ENV))
    );
}

#[test]
fn captures_require_complete_regular_files_and_hash_exact_binary_bytes() {
    let fixture = Fixture::new();
    let summary = fixture.capture("stdout", b"\0\xff\r\n");
    let capture = read_capture(&summary, true).unwrap();
    assert_eq!(capture.bytes, b"\0\xff\r\n");
    assert_eq!(capture.hash, failforge_dart::sha256_hex(b"\0\xff\r\n"));
    assert!(read_capture(&summary, false).unwrap().bytes.is_empty());
    let mut wrong = summary.clone();
    wrong.bytes_read += 1;
    assert!(read_capture(&wrong, true).is_err());
    wrong = summary.clone();
    wrong.file_bytes_written -= 1;
    assert!(read_capture(&wrong, true).is_err());
    let alias = fixture.0.join("alias");
    symlink(summary.file_path.as_ref().unwrap(), &alias).unwrap();
    wrong = summary.clone();
    wrong.file_path = Some(alias);
    assert!(read_capture(&wrong, true).is_err());
    wrong.file_path = Some(fixture.0.clone());
    assert!(read_capture(&wrong, true).is_err());
    wrong.file_path = Some(fixture.0.join("absent"));
    assert!(read_capture(&wrong, true).is_err());
    let mut over_limit = fixture.capture("large", b"");
    File::options()
        .write(true)
        .open(over_limit.file_path.as_ref().unwrap())
        .unwrap()
        .set_len(MAX_CAPTURE + 1)
        .unwrap();
    over_limit.bytes_read = MAX_CAPTURE + 1;
    over_limit.file_bytes_written = MAX_CAPTURE + 1;
    assert!(read_capture(&over_limit, true).is_err());
    let mut missing_digest = summary.clone();
    missing_digest.sha256 = None;
    assert!(read_capture(&missing_digest, true).is_err());
    std::fs::write(summary.file_path.as_ref().unwrap(), b"fake").unwrap();
    assert!(read_capture(&summary, true).is_err());
}

#[test]
fn publication_binds_both_streams_and_fails_closed_on_native_owner_failure() {
    let fixture = Fixture::new();
    let result = CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: Some(ExitStatus::from_raw(0)),
        timings: Default::default(),
        stdout: fixture.capture("stdout", b""),
        stderr: fixture.capture("stderr", b"diagnostic"),
        cleanup: CleanupReport {
            child_reaped: true,
            output_finished: true,
            group: GroupState::NoLiveMembers { zombies: 0 },
            ..Default::default()
        },
    };
    for (index, native_ok) in [true, false].into_iter().enumerate() {
        let mut file = fixture.file(&format!("report-{index}"), true);
        adopt(&file)
            .unwrap()
            .publish(
                &result,
                if native_ok { 0 } else { 125 },
                native_ok,
                &fixture.0,
            )
            .unwrap();
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["version"], 3);
        assert!(value["catalogue"].is_null());
        assert_eq!(value["stdoutSha256"], failforge_dart::sha256_hex(b""));
        assert_eq!(
            value["stderrSha256"],
            failforge_dart::sha256_hex(b"diagnostic")
        );
        assert_eq!(value["stderrBytes"], 10);
        assert_eq!(value["classification"]["outcome"], "INFRA_ERROR");
        assert_eq!(
            value["classification"]["reason"],
            if native_ok {
                "missing_done_event"
            } else {
                "native_owner_failed"
            }
        );
    }
}

#[test]
fn changed_handoff_never_receives_a_report() {
    let fixture = Fixture::new();
    let mut file = fixture.file("report", true);
    let evidence = adopt(&file).unwrap();
    file.write_all(b"tampered").unwrap();
    let result = CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: None,
        timings: Default::default(),
        stdout: fixture.capture("stdout", b""),
        stderr: fixture.capture("stderr", b""),
        cleanup: Default::default(),
    };
    assert!(evidence.publish(&result, 125, false, &fixture.0).is_err());
    assert_eq!(file.metadata().unwrap().len(), 8);
}

#[test]
fn in_memory_analysis_matches_private_handoff_without_publishing_private_state() {
    let fixture = Fixture::new();
    let raw = [
        json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":10}),
        json!({"type":"suite","time":1,"suite":{"id":0,"platform":"vm","path":"test/author_test.dart"}}),
        json!({"type":"group","time":2,"group":{"id":1,"suiteID":0,"parentID":null,"name":"","testCount":1,"metadata":{"skip":false}}}),
        json!({"type":"testStart","time":3,"test":{"id":2,"suiteID":0,"groupIDs":[1],"name":"author","metadata":{"skip":false}}}),
        json!({"type":"testDone","time":4,"testID":2,"result":"success","hidden":false,"skipped":false}),
        json!({"type":"allSuites","time":5,"count":1}),
        json!({"type":"done","time":6,"success":true}),
    ].into_iter().flat_map(|event| {
        let mut bytes = serde_json::to_vec(&event).unwrap();
        bytes.push(b'\n');
        bytes
    }).collect::<Vec<_>>();
    let context = SelectionContext {
        alias: "subject".into(),
        ..Default::default()
    };
    let result = CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: Some(ExitStatus::from_raw(0)),
        timings: Default::default(),
        stdout: fixture.capture("stdout", &raw),
        stderr: fixture.capture("stderr", b"\0\xff"),
        cleanup: CleanupReport {
            child_reaped: true,
            output_finished: true,
            group: GroupState::NoLiveMembers { zombies: 0 },
            ..Default::default()
        },
    };
    let native = analyze(&result, true, &fixture.0, TestReporter::Dart, &context).unwrap();
    assert_eq!(native.classification["outcome"], "SURVIVED");
    assert_eq!(
        native.classification["testIdentities"],
        json!([["test/author_test.dart", "author"]])
    );
    assert!(native.catalogue.is_some());
    assert!(native.initialization.observation.observation.is_some());
    let mut file = fixture.file("private-result", true);
    let mut handoff = adopt(&file).unwrap();
    handoff.context = Some(context);
    handoff.publish(&result, 0, true, &fixture.0).unwrap();
    file.rewind().unwrap();
    let mut encoded = Vec::new();
    file.read_to_end(&mut encoded).unwrap();
    assert!(encoded.ends_with(b"\n"));
    let envelope: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        envelope,
        json!({
            "version":3, "reporterMode":"dart", "ownerExitCode":0,
            "stdoutBytes":native.stdout_length, "stderrBytes":native.stderr_length,
            "stdoutSha256":native.stdout_hash, "stderrSha256":native.stderr_hash,
            "classification":native.classification, "catalogue":native.catalogue,
            "initialization":native.initialization,
        })
    );
}

#[test]
fn invalid_typed_context_and_fifo_capture_cannot_supply_evidence() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    let result = CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: None,
        timings: Default::default(),
        stdout: fixture.capture("stdout", b""),
        stderr: fixture.capture("stderr", b""),
        cleanup: Default::default(),
    };
    let context = SelectionContext {
        baseline: Some(CatalogSnapshot {
            raw_sha256: "invalid".into(),
            cwd: "/".into(),
            identities: vec![],
            view: None,
        }),
        ..Default::default()
    };
    assert!(analyze(&result, true, &fixture.0, TestReporter::Dart, &context).is_err());
    let path = fixture.0.join("fifo");
    let native = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: native is a terminated private fixture pathname; no FD is opened
    // here. The tested reader must reject this FIFO without waiting for writers.
    assert_eq!(unsafe { libc::mkfifo(native.as_ptr(), 0o600) }, 0);
    let mut fifo = result.stdout;
    fifo.file_path = Some(path);
    assert!(read_capture(&fifo, true).is_err());
}
