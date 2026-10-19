//! A private pipe cancels the native owner even before it installs handlers.
//! The owner retains its lease and drains its Runner when the controller dies.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use failforge_dart::{MAX_LINE_BYTES, Mutant, ValidationState, read_and_apply_mutant, sha256_hex};
use serde_json::Value;
use tokio::io::{Interest, unix::AsyncFd};

use super::inputs;
use super::workspaces::{Recovery, Workspace};
use super::{Cancellation, FamilyApplicability, MAX_DISCOVERY_CANDIDATES, Runtime, failure};
use crate::ProfileError;

const CANCEL_ENV: &str = "FAILFORGE_ADAPTER_CANCEL_FD";

struct NativeOwner {
    child: Option<Child>,
    control: Option<File>,
    recovery: Option<Recovery>,
}

type ReaperState = Arc<Mutex<Option<(Child, Option<Recovery>)>>>;

impl Drop for NativeOwner {
    fn drop(&mut self) {
        self.control.take(); // EOF tells the native helper to drain its Runner.
        let Some(child) = self.child.take() else {
            return;
        };
        let recovery = self.recovery.take();
        let state = Arc::new(Mutex::new(Some((child, recovery))));
        let retained = state.clone();
        let reap = |state: ReaperState| {
            let value = state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some((mut child, recovery)) = value {
                if child.wait().is_ok() {
                    if let Some(recovery) = recovery {
                        let _ = recovery.recover();
                    }
                }
            }
        };
        if std::thread::Builder::new()
            .name("profile-owner-reaper".into())
            .spawn(move || reap(retained))
            .is_err()
        {
            // Thread exhaustion must not abandon a child. This exceptional
            // fallback blocks only until the helper has drained on pipe EOF.
            reap(state);
        }
    }
}

fn pipe() -> io::Result<(File, File)> {
    let mut raw = [-1; 2];
    // SAFETY: raw has space for the two fresh returned descriptors.
    if unsafe { libc::pipe(raw.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe returned two unique, live descriptors.
    let pair = unsafe { (File::from_raw_fd(raw[0]), File::from_raw_fd(raw[1])) };
    fn private(file: File) -> io::Result<File> {
        // Duplicate away from standard streams; keep all parent descriptors
        // CLOEXEC. Only the two intended capabilities are inherited in pre_exec.
        // SAFETY: fcntl borrows file and returns a new uniquely owned descriptor.
        let copied = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: copied is the fresh successful descriptor above.
        Ok(unsafe { File::from_raw_fd(copied) })
    }
    Ok((private(pair.0)?, private(pair.1)?))
}

fn nonblocking(file: &File) -> io::Result<()> {
    // SAFETY: fcntl operations use only a live integer descriptor/flags.
    let status = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if status < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, status | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn request_cancel(writer: &mut Option<File>, signal: i32) -> Result<(), ProfileError> {
    if let Some(mut pipe) = writer.take() {
        let byte = if signal == libc::SIGTERM { b'T' } else { b'I' };
        match pipe.write_all(&[byte]) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => (),
            Err(error) => return Err(error.into()),
        }
        // Closing the sole controller writer also makes controller death an
        // observable EOF, never a PID-based guess at cancellation readiness.
    }
    Ok(())
}

pub(super) async fn discover(
    runtime: &Runtime,
    package: &Path,
    workspace: &Workspace,
    payload: &Value,
    family: &str,
    requested: &[String],
    cancel: &Cancellation,
) -> Result<FamilyApplicability, ProfileError> {
    cancel.check()?;
    let source = payload["rules"]["includePaths"]
        .as_array()
        .and_then(|values| values.first())
        .and_then(Value::as_str)
        .ok_or_else(|| failure("analysis request has no selected file"))?;
    let output = workspace.path.join(format!("discovery-{family}"));
    fs::create_dir(&output)?;
    let payload_path = output.join("payload.json");
    fs::write(
        &payload_path,
        serde_json::to_vec(payload).map_err(|_| failure("cannot serialize analysis request"))?,
    )?;
    let errors = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(output.join("adapter.stderr"))?;
    let (control_read, control_write) = pipe()?;
    nonblocking(&control_write)?;
    let (stdout_read, stdout_write) = pipe()?;
    nonblocking(&stdout_read)?;
    let stdout = AsyncFd::with_interest(stdout_read, Interest::READABLE)?;
    let control_fd = control_read.as_raw_fd();
    let lease_fd = workspace.lease_fd();
    let mut command = Command::new(&runtime.helper);
    command
        .args([
            runtime.dart_sdk.join("bin/dart"),
            runtime.adapter.clone(),
            payload_path,
        ])
        .env_clear()
        .envs(runtime.environment())
        .env(CANCEL_ENV, control_fd.to_string())
        .env(failforge_runner::WORKSPACE_LEASE_ENV, lease_fd.to_string())
        .stdin(Stdio::null())
        .stdout(stdout_write)
        .stderr(errors);
    // SAFETY: the callback executes after fork with exclusively async-signal-safe
    // integer syscalls; descriptors remain alive until spawn has completed.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            for fd in [control_fd, lease_fd] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let recovery = workspace.recovery()?;
    let child = command.spawn()?;
    let mut owner = NativeOwner {
        child: Some(child),
        control: Some(control_write),
        recovery: Some(recovery),
    };
    drop(command); // Drop the parent's duplicate stdout writer before reading EOF.
    drop(control_read);
    let mut transcript = Transcript::new(package, family, requested, source);
    let mut error = None;
    let mut line = Vec::new();
    let mut buffer = [0; 32 * 1024];
    let mut eof = false;
    let mut exit = None;
    let mut signalled = false;
    while !eof || exit.is_none() {
        tokio::select! {
            signal = cancel.cancelled(), if !signalled => {
                signalled = true;
                if let Err(problem) = request_cancel(&mut owner.control, signal) { error.get_or_insert(problem); }
            },
            ready = stdout.readable(), if !eof => {
                match ready {
                    Err(problem) => { error.get_or_insert(problem.into()); eof = true; }
                    Ok(mut guard) => match guard.try_io(|ready| {
                        let mut file = ready.get_ref();
                        file.read(&mut buffer)
                    }) {
                        Err(_) => (),
                        Ok(Err(problem)) if problem.kind() == io::ErrorKind::Interrupted => (),
                        Ok(Err(problem)) => { error.get_or_insert(problem.into()); eof = true; },
                        Ok(Ok(0)) => { eof = true; },
                        Ok(Ok(count)) => {
                            if error.is_none() {
                                for byte in &buffer[..count] {
                                    if *byte == b'\n' {
                                        if let Err(problem) = transcript.accept(&line) { error = Some(problem); break; }
                                        line.clear();
                                    } else if line.len() >= MAX_LINE_BYTES {
                                        error = Some(failure("adapter campaign record exceeds eight MiB"));
                                        break;
                                    } else { line.push(*byte); }
                                }
                            }
                        },
                    },
                }
            },
            _ = tokio::time::sleep(Duration::from_millis(10)), if exit.is_none() => {
                match owner.child.as_mut().expect("live owner").try_wait() {
                    Ok(status) => exit = status,
                    Err(problem) => { error.get_or_insert(problem.into()); break; }
                }
            },
        }
        if error.is_some() && owner.control.is_some() {
            let _ = request_cancel(&mut owner.control, libc::SIGINT);
        }
    }
    if exit.is_some() {
        owner.child.take();
    } // A successful try_wait reaped it.
    owner.control.take();
    if error.is_none() && !line.is_empty() {
        error = Some(failure(
            "adapter campaign ended with an unterminated record",
        ));
    }
    if let Some(error) = error {
        return Err(with_diagnostics(error, &output, family));
    }
    let result = transcript.finish(exit.expect("loop waits for owner exit"), cancel.signum());
    result.map_err(|error| with_diagnostics(error, &output, family))
}

fn with_diagnostics(error: ProfileError, output: &Path, family: &str) -> ProfileError {
    if matches!(error, ProfileError::Cancelled(_)) {
        return error;
    }
    let printable = |text: &str| {
        text.chars()
            .map(|character| {
                if character.is_control() || matches!(character,
                    '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut message = format!(
        "applicability analysis failed for {family}: {}",
        printable(&error.to_string())
    );
    if let Ok(bytes) = inputs::read_regular_prefix(&output.join("adapter.stderr"), 4096) {
        let detail = printable(&String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]));
        if !detail.is_empty() {
            message.push_str("; adapter stderr: ");
            message.push_str(&detail);
        }
        if bytes.len() > 4096 {
            message.push_str(" [truncated]");
        }
    }
    failure(message)
}

struct Transcript<'a> {
    package: &'a Path,
    family: &'a str,
    requested: &'a [String],
    source: &'a str,
    phase: u8,
    metadata: BTreeMap<String, String>,
    mutants: BTreeMap<String, Value>,
    seen: BTreeSet<String>,
    valid: BTreeMap<String, usize>,
    cleanup: Option<Value>,
    discovered: bool,
}

impl<'a> Transcript<'a> {
    fn new(package: &'a Path, family: &'a str, requested: &'a [String], source: &'a str) -> Self {
        Self {
            package,
            family,
            requested,
            source,
            phase: 0,
            metadata: BTreeMap::new(),
            mutants: BTreeMap::new(),
            seen: BTreeSet::new(),
            valid: BTreeMap::new(),
            cleanup: None,
            discovered: false,
        }
    }

    fn accept(&mut self, bytes: &[u8]) -> Result<(), ProfileError> {
        let event = inputs::parse_json(bytes)?;
        let object = event
            .as_object()
            .ok_or_else(|| failure("invalid adapter event"))?;
        if object.len() != 2
            || !object.contains_key("step")
            || !object.contains_key("payload")
            || !event["payload"].is_object()
            || self.phase == 3
        {
            return Err(failure("malformed or nonterminal adapter cleanup record"));
        }
        let payload = &event["payload"];
        match (self.phase, event["step"].as_str()) {
            (_, Some("cleanup")) => {
                self.cleanup = Some(payload.clone());
                self.phase = 3;
            }
            (0, Some("handshake")) => {
                self.handshake(payload)?;
                self.phase = 1;
            }
            (1, Some("discover")) => {
                self.discovery(payload)?;
                self.phase = 2;
                self.discovered = true;
            }
            (2, Some("candidate")) => self.candidate(payload)?,
            _ => return Err(failure("adapter transcript steps out of order")),
        }
        Ok(())
    }

    fn handshake(&mut self, value: &Value) -> Result<(), ProfileError> {
        for operator in value["operators"]
            .as_array()
            .ok_or_else(|| failure("adapter handshake has no operators"))?
        {
            let id = operator["id"]
                .as_str()
                .ok_or_else(|| failure("malformed adapter operator"))?;
            let family = operator["family"]
                .as_str()
                .ok_or_else(|| failure("malformed adapter family"))?;
            if self.metadata.insert(id.into(), family.into()).is_some() {
                return Err(failure("duplicate adapter operator"));
            }
        }
        for operator in self.requested {
            if self.metadata.get(operator).map(String::as_str) != Some(self.family) {
                return Err(failure("operator does not belong to selected family"));
            }
        }
        Ok(())
    }

    fn discovery(&mut self, value: &Value) -> Result<(), ProfileError> {
        let mutants = value["mutants"]
            .as_array()
            .ok_or_else(|| failure("adapter discovery has no mutants"))?;
        if value["truncated"] != false
            || value["returnedCandidates"].as_u64() != Some(mutants.len() as u64)
            || value["totalCandidates"].as_u64() != Some(mutants.len() as u64)
            || mutants.len() > MAX_DISCOVERY_CANDIDATES
        {
            return Err(failure(
                "adapter discovery is truncated, inconsistent or exceeds candidate limit",
            ));
        }
        for value in mutants {
            let mutant: Mutant = serde_json::from_value(value.clone())
                .map_err(|_| failure("malformed discovered mutant"))?;
            mutant
                .validate_model()
                .map_err(|_| failure("invalid discovered mutant identity"))?;
            if mutant.file != self.source
                || mutant.family.as_str() != self.family
                || !self.requested.contains(&mutant.operator_id)
                || !mutant.components.is_empty()
            {
                return Err(failure(
                    "adapter returned a mutant outside requested file or operator selection",
                ));
            }
            if mutant.validation != ValidationState::Unvalidated
                || self.mutants.insert(mutant.id, value.clone()).is_some()
            {
                return Err(failure("duplicate or already validated discovered mutant"));
            }
        }
        Ok(())
    }

    fn candidate(&mut self, value: &Value) -> Result<(), ProfileError> {
        let mut mutant: Mutant = serde_json::from_value(value["mutant"].clone())
            .map_err(|_| failure("malformed validated mutant"))?;
        if self.mutants.get(&mutant.id) != Some(&value["mutant"])
            || self.seen.contains(&mutant.id)
            || value["index"].as_u64() != Some(self.seen.len() as u64)
        {
            return Err(failure("missing, changed or duplicate validated candidate"));
        }
        let validation = &value["validation"];
        let patch = &value["safePatch"];
        if validation["mutantId"].as_str() != Some(&mutant.id)
            || patch["mutantId"].as_str() != Some(&mutant.id)
            || patch["file"].as_str() != Some(&mutant.file)
            || patch["writesPerformed"] != false
        {
            return Err(failure(
                "static validation or patch proof identifies a different mutant",
            ));
        }
        let valid = match (
            validation["valid"].as_bool(),
            validation["validation"].as_str(),
        ) {
            (Some(true), Some("valid")) => true,
            (Some(false), Some("invalid")) => false,
            _ => return Err(failure("contradictory static validation evidence")),
        };
        if valid {
            mutant.validation = ValidationState::Valid;
            let expected = read_and_apply_mutant(self.package, &mutant, 4 * 1024 * 1024)
                .map_err(|_| failure("static candidate byte patch is not applicable"))?;
            if patch["applied"] != true
                || patch["patchedBytes"].as_u64() != Some(expected.len() as u64)
                || patch["patchedUtf8"].as_str().map(str::as_bytes) != Some(expected.as_slice())
                || patch["patchedHash"].as_str() != Some(&sha256_hex(&expected))
            {
                return Err(failure("static candidate patch proof mismatch"));
            }
            *self.valid.entry(mutant.operator_id).or_default() += 1;
        } else {
            let bytes = inputs::read_regular(&self.package.join(&mutant.file), 4 * 1024 * 1024)?;
            if sha256_hex(&bytes) != mutant.source_hash || patch["applied"] != false {
                return Err(failure(
                    "invalid candidate changed source or contains a patch",
                ));
            }
        }
        self.seen.insert(mutant.id);
        Ok(())
    }

    fn finish(
        self,
        status: ExitStatus,
        signal: Option<i32>,
    ) -> Result<FamilyApplicability, ProfileError> {
        let cleanup = self
            .cleanup
            .ok_or_else(|| failure("adapter transcript has no cleanup receipt"))?;
        let valid: usize = self.valid.values().sum();
        if cleanup["complete"] != true
            || cleanup["runnerShutdown"] != true
            || cleanup["adapterCloseSucceeded"] != true
            || cleanup["writesPerformed"] != false
            || cleanup["activeRuns"].as_u64() != Some(0)
            || cleanup["validatedCandidates"].as_u64() != Some(self.seen.len() as u64)
            || cleanup["staticallyValidCandidates"].as_u64() != Some(valid as u64)
        {
            return Err(failure("adapter cleanup is incomplete or inconsistent"));
        }
        if cleanup["campaignSucceeded"] == false {
            if let Some(signal) = signal {
                let reason = if signal == libc::SIGTERM {
                    "SIGTERM"
                } else {
                    "SIGINT"
                };
                if status.code() == Some(1)
                    && cleanup["cancelled"] == true
                    && cleanup["cancellationReason"] == reason
                    && cleanup
                        .get("exitCode")
                        .is_some_and(|code| code.is_null() || code.as_u64().is_some())
                {
                    return Err(ProfileError::Cancelled(signal));
                }
            }
            return Err(failure(
                "adapter discovery failed without successful cleanup evidence",
            ));
        }
        if cleanup["campaignSucceeded"] != true
            || !status.success()
            || cleanup["exitCode"].as_u64() != Some(0)
            || cleanup["cancelled"] != false
            || !self.discovered
            || self.seen.len() != self.mutants.len()
        {
            return Err(failure(
                "incomplete adapter transcript or inconsistent owner exit",
            ));
        }
        if let Some(signal) = signal {
            return Err(ProfileError::Cancelled(signal));
        }
        let active_operators = failforge_config::catalogue::operators(self.family)
            .unwrap_or_default()
            .iter()
            .filter(|operator| self.valid.get(**operator).copied().unwrap_or(0) > 0)
            .map(|operator| (*operator).to_owned())
            .collect();
        Ok(FamilyApplicability {
            family: self.family.into(),
            active_operators,
            discovered_candidates: self.mutants.len(),
            statically_valid_candidates: valid,
            valid_counts: self
                .requested
                .iter()
                .map(|operator| {
                    (
                        operator.clone(),
                        self.valid.get(operator).copied().unwrap_or(0),
                    )
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "failforge-profile-transcript-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn handshake() -> Value {
        json!({"step":"handshake","payload":{"operators":[{"id":"RELATIONAL_BOUNDARY","family":"classic"}]}})
    }
    fn discovery(mutants: Vec<Value>) -> Value {
        json!({"step":"discover","payload":{"truncated":false,"returnedCandidates":mutants.len(),"totalCandidates":mutants.len(),"mutants":mutants}})
    }
    fn cleanup() -> Value {
        json!({"step":"cleanup","payload":{"complete":true,"runnerShutdown":true,"adapterCloseSucceeded":true,"writesPerformed":false,"activeRuns":0,"validatedCandidates":0,"staticallyValidCandidates":0,"campaignSucceeded":true,"exitCode":0,"cancelled":false}})
    }
    fn accept(transcript: &mut Transcript<'_>, event: &Value) -> Result<(), ProfileError> {
        transcript.accept(&serde_json::to_vec(event).unwrap())
    }

    #[test]
    fn complete_empty_discovery_is_valid_and_truncation_is_not() {
        let requested = vec!["RELATIONAL_BOUNDARY".to_owned()];
        let mut transcript = Transcript::new(Path::new("/"), "classic", &requested, "lib/a.dart");
        accept(&mut transcript, &handshake()).unwrap();
        accept(&mut transcript, &discovery(vec![])).unwrap();
        accept(&mut transcript, &cleanup()).unwrap();
        let result = transcript.finish(ExitStatus::from_raw(0), None).unwrap();
        assert_eq!(result.valid_counts, vec![("RELATIONAL_BOUNDARY".into(), 0)]);
        for changed in [json!(true), Value::Null] {
            let mut transcript =
                Transcript::new(Path::new("/"), "classic", &requested, "lib/a.dart");
            accept(&mut transcript, &handshake()).unwrap();
            let mut event = discovery(vec![]);
            event["payload"]["truncated"] = changed;
            assert!(accept(&mut transcript, &event).is_err());
        }
    }

    #[test]
    fn malformed_duplicate_and_nonterminal_transcript_are_rejected() {
        let mut transcript = Transcript::new(Path::new("/"), "classic", &[], "lib/a.dart");
        assert!(
            transcript
                .accept(br#"{"step":"handshake","step":"cleanup","payload":{}}"#)
                .is_err()
        );
        accept(&mut transcript, &handshake()).unwrap();
        assert!(accept(&mut transcript, &handshake()).is_err());
        accept(&mut transcript, &discovery(vec![])).unwrap();
        accept(&mut transcript, &cleanup()).unwrap();
        assert!(accept(&mut transcript, &cleanup()).is_err());
    }

    #[test]
    fn cancellation_requires_matching_terminal_cleanup() {
        let mut transcript = Transcript::new(Path::new("/"), "classic", &[], "lib/a.dart");
        let mut event = cleanup();
        event["payload"]["campaignSucceeded"] = false.into();
        event["payload"]["cancelled"] = true.into();
        event["payload"]["cancellationReason"] = "SIGTERM".into();
        event["payload"]["exitCode"] = Value::Null;
        accept(&mut transcript, &event).unwrap();
        assert!(matches!(
            transcript.finish(ExitStatus::from_raw(256), Some(libc::SIGTERM)),
            Err(ProfileError::Cancelled(libc::SIGTERM))
        ));
        event["payload"]["complete"] = false.into();
        let mut transcript = Transcript::new(Path::new("/"), "classic", &[], "lib/a.dart");
        accept(&mut transcript, &event).unwrap();
        assert!(matches!(
            transcript.finish(ExitStatus::from_raw(256), Some(libc::SIGTERM)),
            Err(ProfileError::Analysis(_))
        ));
    }

    #[test]
    fn selection_and_candidate_limits_are_checked_before_validation() {
        use failforge_dart::{ByteEdit, MutationFamily, Symbol};
        let requested = vec!["RELATIONAL_BOUNDARY".to_owned()];
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: requested[0].clone(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/other.dart".into(),
            source_hash: sha256_hex(b"a"),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 0,
                length_bytes: 1,
                expected: "a".into(),
                replacement: "b".into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        let mut transcript = Transcript::new(Path::new("/"), "classic", &requested, "lib/a.dart");
        accept(&mut transcript, &handshake()).unwrap();
        assert!(accept(&mut transcript, &discovery(vec![json!(mutant)])).is_err());
        mutant.file = "lib/a.dart".into();
        mutant.operator_id = "BOOLEAN_LITERAL_FLIP".into();
        mutant.id = mutant.stable_id();
        assert!(accept(&mut transcript, &discovery(vec![json!(mutant)])).is_err());
        assert!(
            accept(
                &mut transcript,
                &discovery(vec![Value::Null; MAX_DISCOVERY_CANDIDATES + 1])
            )
            .is_err()
        );
    }

    #[test]
    fn real_byte_proofs_are_checked_and_validated_results_do_not_write_sources() {
        use failforge_dart::{ByteEdit, MutationFamily, Symbol};
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("lib")).unwrap();
        let source = b"bool f(int x) => x < 1;\n";
        fs::write(fixture.0.join("lib/a.dart"), source).unwrap();
        let mut mutant = Mutant {
            id: String::new(),
            operator_id: "RELATIONAL_BOUNDARY".into(),
            operator_version: 1,
            family: MutationFamily::Classic,
            file: "lib/a.dart".into(),
            source_hash: sha256_hex(source),
            components: vec![],
            edit: ByteEdit {
                offset_bytes: 19,
                length_bytes: 1,
                expected: "<".into(),
                replacement: "<=".into(),
            },
            symbol: Symbol::default(),
            validation: ValidationState::Unvalidated,
        };
        mutant.id = mutant.stable_id();
        let patched = b"bool f(int x) => x <= 1;\n";
        let candidate = json!({"step":"candidate","payload":{"index":0,"mutant":mutant,
            "validation":{"mutantId":mutant.id,"valid":true,"validation":"valid"},
            "safePatch":{"applied":true,"mutantId":mutant.id,"file":mutant.file,"patchedHash":sha256_hex(patched),"patchedBytes":patched.len(),"patchedUtf8":std::str::from_utf8(patched).unwrap(),"writesPerformed":false}}});
        let requested = vec!["RELATIONAL_BOUNDARY".into()];
        let build = || {
            let mut transcript = Transcript::new(&fixture.0, "classic", &requested, "lib/a.dart");
            accept(&mut transcript, &handshake()).unwrap();
            accept(&mut transcript, &discovery(vec![json!(mutant)])).unwrap();
            transcript
        };
        let mut transcript = build();
        accept(&mut transcript, &candidate).unwrap();
        let mut terminal = cleanup();
        terminal["payload"]["validatedCandidates"] = 1.into();
        terminal["payload"]["staticallyValidCandidates"] = 1.into();
        accept(&mut transcript, &terminal).unwrap();
        let result = transcript.finish(ExitStatus::from_raw(0), None).unwrap();
        assert_eq!(result.active_operators, requested);
        assert_eq!(fs::read(fixture.0.join("lib/a.dart")).unwrap(), source);
        for field in [
            "patchedHash",
            "patchedBytes",
            "patchedUtf8",
            "mutantId",
            "file",
            "writesPerformed",
        ] {
            let mut event = candidate.clone();
            event["payload"]["safePatch"][field] = Value::Null;
            assert!(accept(&mut build(), &event).is_err(), "{field}");
        }
        let mut event = candidate.clone();
        event["payload"]["validation"]["valid"] = false.into();
        assert!(accept(&mut build(), &event).is_err());
        let mut transcript = build();
        accept(&mut transcript, &candidate).unwrap();
        assert!(accept(&mut transcript, &candidate).is_err());
        fs::write(fixture.0.join("lib/a.dart"), b"changed").unwrap();
        assert!(accept(&mut build(), &candidate).is_err());
        event["payload"]["validation"]["validation"] = "invalid".into();
        event["payload"]["safePatch"]["applied"] = false.into();
        assert!(accept(&mut build(), &event).is_err());
    }

    #[test]
    fn diagnostics_are_bounded_and_never_follow_aliases_or_block_on_fifos() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let errors = fixture.0.join("adapter.stderr");
        fs::write(&errors, vec![b'x'; 5000]).unwrap();
        let message = with_diagnostics(failure("failed"), &fixture.0, "classic").to_string();
        assert!(message.ends_with("[truncated]"));
        assert!(message.len() < 4300);
        fs::write(
            &errors,
            "adapter failed \u{202e}txt.exe\u{202c}\u{001b}[31m",
        )
        .unwrap();
        let message = with_diagnostics(failure("failed"), &fixture.0, "classic").to_string();
        assert!(!message.contains(['\u{202e}', '\u{202c}', '\u{001b}']));
        fs::remove_file(&errors).unwrap();
        let secret = fixture.0.join("secret");
        fs::write(&secret, "do not follow").unwrap();
        symlink(&secret, &errors).unwrap();
        assert!(
            !with_diagnostics(failure("failed"), &fixture.0, "classic")
                .to_string()
                .contains("do not follow")
        );
        fs::remove_file(&errors).unwrap();
        let path = std::ffi::CString::new(errors.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: path is a live terminated string naming a fresh test fixture.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(
            !with_diagnostics(failure("failed"), &fixture.0, "classic")
                .to_string()
                .contains("stderr:")
        );
    }

    #[tokio::test]
    async fn dropping_discovery_reaps_native_owner_and_recovers_real_workspace() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let helper = fixture.0.join("helper");
        let ready = fixture.0.join("ready");
        // A deterministic fake owner blocks on exactly the inherited private
        // cancellation descriptor. It exits only when dropping the future
        // closes the controller writer; no sleeps or author code are involved.
        let ready_text = ready.to_str().unwrap().replace('\'', "'\"'\"'");
        fs::write(&helper,format!("#!/bin/sh\nprintf '%s\\n' \"$$\" > '{ready_text}'\neval \"cat <&$FAILFORGE_ADAPTER_CANCEL_FD\" >/dev/null\n")).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = Runtime {
            helper,
            dart_sdk: fixture.0.clone(),
            flutter_sdk: None,
            adapter: fixture.0.clone(),
            installation: fixture.0.clone(),
        };
        let workspace = Workspace::new(std::slice::from_ref(&fixture.0)).unwrap();
        let campaign = workspace.path.parent().unwrap().to_owned();
        let cancel = Cancellation::default();
        let payload = json!({"rules":{"includePaths":["lib/a.dart"]}});
        let mut operation = Box::pin(discover(
            &runtime,
            &fixture.0,
            &workspace,
            &payload,
            "classic",
            &[],
            &cancel,
        ));
        tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                tokio::select! {
                    result = &mut operation => panic!("fake owner ended before EOF: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(5)) => if ready.exists() { break; },
                }
            }
        }).await.unwrap();
        let pid: libc::pid_t = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
        drop(operation);
        drop(workspace);
        tokio::time::timeout(Duration::from_secs(5), async {
            while campaign.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("detached cleanup must remove its leased campaign");
        let mut status = 0;
        // SAFETY: pid is the captured test child, status is writable. ECHILD
        // proves the guard waited it; returning pid would expose a zombie.
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
