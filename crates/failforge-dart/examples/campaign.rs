//! Stream validation and read-only patch evidence for every discovered mutant.
//! Test execution and isolated copies belong to the caller, not this example.
use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::future::Future;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use failforge_dart::{
    AdapterClient, ClientError, ClientOptions, MAX_LINE_BYTES, Mutant, ValidationState,
    read_and_apply_mutant, sha256_hex, validate_relative_target,
};
use failforge_runner::{CommandSpec, EnvSpec, Runner, WORKSPACE_LEASE_ENV, WorkspaceLease};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{Interest, unix::AsyncFd};
use tokio::signal::unix::{Signal, SignalKind, signal};

type Failure = Box<dyn Error>;
const MAX_MUTANTS: u64 = 10_000;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const CANCEL_ENV: &str = "FAILFORGE_ADAPTER_CANCEL_FD";

struct Configuration {
    dart: OsString,
    adapter: PathBuf,
    package: PathBuf,
    payload: Value,
    candidate_limit: u64,
}

impl Configuration {
    fn from_args(args: Vec<OsString>) -> Result<Self, Failure> {
        if args.len() != 3 {
            return Err("usage: campaign DART_EXECUTABLE ADAPTER_ROOT PAYLOAD_JSON_FILE".into());
        }
        let payload = read_payload(File::open(&args[2])?)?;
        let candidate_limit = candidate_limit(&payload)?;
        let package = package_root(&payload)?;
        let adapter = PathBuf::from(&args[1]).canonicalize()?;
        Ok(Self {
            dart: args[0].clone(),
            adapter,
            package,
            payload,
            candidate_limit,
        })
    }

    fn spec(&self) -> CommandSpec {
        let mut environment = EnvSpec::default();
        environment.remove.insert(WORKSPACE_LEASE_ENV.into());
        environment.remove.insert(CANCEL_ENV.into());
        CommandSpec {
            program: self.dart.clone(),
            args: vec![
                format!(
                    "--packages={}",
                    self.adapter
                        .join(".dart_tool/package_config.json")
                        .display()
                )
                .into(),
                self.adapter
                    .join("bin/failforge_dart_adapter.dart")
                    .into_os_string(),
            ],
            cwd: self.adapter.clone(),
            env: environment,
            timeout_policy: Default::default(),
            capture_policy: Default::default(),
        }
    }
}

fn read_payload(input: impl Read) -> Result<Value, Failure> {
    let mut bytes = Vec::new();
    input
        .take(u64::try_from(MAX_LINE_BYTES)? + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LINE_BYTES {
        return Err("normalized payload file exceeds the eight-MiB byte limit".into());
    }
    let payload: Value = serde_json::from_slice(&bytes)?;
    if !payload.is_object() {
        return Err("normalized campaign payload must be a JSON object".into());
    }
    Ok(payload)
}

fn candidate_limit(payload: &Value) -> Result<u64, Failure> {
    let Some(limits) = payload.get("limits") else {
        return Ok(1000); // The adapter's existing default, not a new setting.
    };
    let limits = limits
        .as_object()
        .ok_or("payload limits must be a JSON object")?;
    if limits.keys().any(|key| key != "maxMutants") {
        return Err("unknown campaign limits field".into());
    }
    let Some(limit) = limits.get("maxMutants") else {
        return Ok(1000);
    };
    limit
        .as_u64()
        .filter(|count| *count <= MAX_MUTANTS)
        .ok_or_else(|| "maxMutants must be an integer from zero through 10000".into())
}

fn package_root(payload: &Value) -> Result<PathBuf, Failure> {
    let workspace = payload
        .get("workspaceRoot")
        .and_then(Value::as_str)
        .ok_or("payload workspaceRoot must be a string")?;
    let workspace = Path::new(workspace);
    if !workspace.is_absolute() {
        return Err("payload workspaceRoot must be absolute".into());
    }
    let workspace = workspace.canonicalize()?;
    if !workspace.is_dir() {
        return Err("payload workspaceRoot must be a directory".into());
    }
    let relative = payload
        .get("packageRoot")
        .and_then(Value::as_str)
        .ok_or("payload packageRoot must be a canonical relative string")?;
    if relative != "." {
        validate_relative_target(relative)?;
    }
    let package = workspace.join(relative).canonicalize()?;
    if !package.is_dir() || !package.starts_with(&workspace) {
        return Err("payload packageRoot must remain inside workspaceRoot".into());
    }
    Ok(package)
}

#[derive(Default)]
struct Statistics {
    validated: u64,
    valid: u64,
}

#[derive(Default)]
struct SessionEvidence {
    close_succeeded: bool,
    exit_code: Option<i32>,
    cancellation: Option<&'static str>,
    close_failure: Option<String>,
    close_failure_details: Option<Value>,
}

/// A pipe buffers a controller request even before this process installs its
/// signal handlers. The private capability is never inherited by the adapter.
struct Cancellation {
    interrupt: Signal,
    terminate: Signal,
    control: Option<AsyncFd<OwnedFd>>,
}

impl Cancellation {
    fn new() -> io::Result<Self> {
        // Subscribe before any managed child exists. External callers can
        // signal this owner or use the race-free buffered control pipe.
        let interrupt = signal(SignalKind::interrupt())?;
        let terminate = signal(SignalKind::terminate())?;
        let control = std::env::var_os(CANCEL_ENV)
            .as_deref()
            .map(cancellation_descriptor)
            .transpose()?;
        Ok(Self {
            interrupt,
            terminate,
            control,
        })
    }

    async fn request(&mut self) -> io::Result<&'static str> {
        tokio::select! {
            event = self.interrupt.recv() => event.map(|()| "SIGINT")
                .ok_or_else(|| io::Error::other("interrupt listener closed")),
            event = self.terminate.recv() => event.map(|()| "SIGTERM")
                .ok_or_else(|| io::Error::other("termination listener closed")),
            result = control_request(&self.control) => result,
        }
    }
}

fn cancellation_descriptor(value: &std::ffi::OsStr) -> io::Result<AsyncFd<OwnedFd>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid campaign cancellation capability",
        )
    };
    let value = value.to_str().ok_or_else(invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let source: RawFd = value.parse().map_err(|_| invalid())?;
    if source < 3 {
        return Err(invalid());
    }
    // SAFETY: fcntl borrows the source and returns a fresh CLOEXEC descriptor;
    // no existing Rust-owned descriptor is consumed or closed.
    let copied = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful duplication created a uniquely owned live descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(copied) };
    let file = File::from(descriptor);
    use std::os::unix::fs::FileTypeExt;
    if !file.metadata()?.file_type().is_fifo() {
        return Err(invalid());
    }
    // SAFETY: both source and duplicate remain live; these fcntl operations use
    // only descriptor/integer arguments. Their open description is read-only.
    let (status, flags) = unsafe {
        (
            libc::fcntl(copied, libc::F_GETFL),
            libc::fcntl(source, libc::F_GETFD),
        )
    };
    if status < 0 || flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if status & libc::O_ACCMODE != libc::O_RDONLY {
        return Err(invalid());
    }
    // SAFETY: source is borrowed, while file owns copied. CLOEXEC keeps both
    // capabilities out of exec; nonblocking is required by AsyncFd readiness.
    if unsafe { libc::fcntl(source, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        || unsafe { libc::fcntl(copied, libc::F_SETFL, status | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    AsyncFd::with_interest(file.into(), Interest::READABLE)
}

async fn control_request(control: &Option<AsyncFd<OwnedFd>>) -> io::Result<&'static str> {
    let Some(control) = control else {
        return std::future::pending().await;
    };
    loop {
        let mut byte = [0_u8];
        let read = control
            .async_io(Interest::READABLE, |descriptor| {
                // SAFETY: AsyncFd owns a live readable pipe and byte is writable
                // for one byte. read retains no pointer after returning.
                let count =
                    unsafe { libc::read(descriptor.as_raw_fd(), byte.as_mut_ptr().cast(), 1) };
                if count < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(count)
                }
            })
            .await;
        match read {
            Ok(0) => return Ok("controller_eof"),
            Ok(_) => {
                return match byte[0] {
                    b'I' => Ok("SIGINT"),
                    b'T' => Ok("SIGTERM"),
                    _ => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid campaign cancellation request",
                    )),
                };
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[tokio::main]
pub async fn main() -> Result<(), Failure> {
    // This owner outlives its controller on channel closure; do not unlock its shared
    // open description or pass the lease capability into the Dart adapter.
    let _workspace_lease = WorkspaceLease::from_env()?;
    let configuration = Configuration::from_args(std::env::args_os().skip(1).collect())?;
    let mut stdout = io::stdout().lock();
    execute(configuration, &mut stdout).await
}

async fn execute(configuration: Configuration, output: &mut dyn Write) -> Result<(), Failure> {
    let mut cancellation = Cancellation::new()?;
    execute_cancellable(configuration, output, cancellation.request()).await
}

async fn execute_cancellable(
    configuration: Configuration,
    output: &mut dyn Write,
    cancellation: impl Future<Output = io::Result<&'static str>>,
) -> Result<(), Failure> {
    let runner = Arc::new(Runner::new());
    let mut statistics = Statistics::default();
    let mut evidence = SessionEvidence::default();
    let outcome = session(
        runner.clone(),
        &configuration,
        output,
        &mut statistics,
        &mut evidence,
        cancellation,
    )
    .await;
    // This executes on every session result, including transport, model,
    // validation, patch and consumer I/O failures. Only this Runner is owned.
    let shutdown = runner.shutdown().await;
    let active_runs = runner.active_runs();
    let cleanup = emit(
        output,
        "cleanup",
        &json!({
            "complete":shutdown.is_ok() && active_runs == 0,
            "adapterCloseSucceeded":evidence.close_succeeded,
            "adapterCloseFailure":evidence.close_failure,
            "adapterCloseFailureDetails":evidence.close_failure_details,
            "exitCode":evidence.exit_code,
            "runnerShutdown":shutdown.is_ok(),
            "activeRuns":active_runs,
            "campaignSucceeded":outcome.is_ok(),
            "cancelled":evidence.cancellation.is_some(),
            "cancellationReason":evidence.cancellation,
            "validatedCandidates":statistics.validated,
            "staticallyValidCandidates":statistics.valid,
            "writesPerformed":false,
        }),
    );
    shutdown?;
    cleanup?;
    outcome
}

async fn session(
    runner: Arc<Runner>,
    configuration: &Configuration,
    output: &mut dyn Write,
    statistics: &mut Statistics,
    evidence: &mut SessionEvidence,
    cancellation: impl Future<Output = io::Result<&'static str>>,
) -> Result<(), Failure> {
    let mut client = AdapterClient::start(
        runner.clone(),
        configuration.spec(),
        ClientOptions {
            request_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    )?;
    tokio::pin!(cancellation);
    let (outcome, request_received) = tokio::select! {
        // An already available protocol/model error must never be relabelled
        // as user cancellation merely because a stop request raced with it.
        biased;
        result = steps(&mut client, configuration, output, statistics) => (result, false),
        request = &mut cancellation => (match request {
            Ok(reason) => { evidence.cancellation = Some(reason); Err("adapter campaign cancelled".into()) },
            Err(error) => Err(error.into()),
        }, true),
    };
    let closed = client.close();
    tokio::pin!(closed);
    let closed = if request_received {
        closed.await
    } else {
        tokio::select! {
            biased;
            result = &mut closed => result,
            request = &mut cancellation => {
                if outcome.is_ok() {
                    evidence.cancellation = Some(request?);
                }
                // This Runner owns only the adapter. Keep polling close so
                // its actual cleanup evidence survives cancellation during
                // EOF/exit, rather than dropping the sole result consumer.
                let (_, result) = tokio::join!(runner.shutdown(), &mut closed);
                result
            },
        }
    };
    if let Ok(result) = &closed {
        evidence.close_succeeded = result.cleanup.is_complete();
        evidence.exit_code = result.status.and_then(|status| status.code());
    } else if let Err(error) = &closed {
        if let ClientError::Execution(error) = error {
            if let Some(result) = error.partial_result() {
                evidence.close_failure_details = Some(json!({
                    "cause":format!("{:?}",result.cause),
                    "cleanupComplete":result.cleanup.is_complete(),
                    "errors":result.cleanup.errors,
                    "group":format!("{:?}",result.cleanup.group),
                    "stdoutEof":result.stdout.eof,"stderrEof":result.stderr.eof,
                    "stdoutIssue":format!("{:?}",result.stdout.issue),
                    "stderrIssue":format!("{:?}",result.stderr.issue),
                }));
            }
        }
        // Only a fixed category and native I/O kind leave this boundary, never
        // protocol bytes or arbitrary adapter diagnostics.
        evidence.close_failure = Some(match error {
            ClientError::Io(error) => format!("io_{:?}", error.kind()),
            ClientError::Execution(_) => "runner_failure".into(),
            ClientError::OutputLost => "output_lost".into(),
            ClientError::UnsolicitedResponse => "unsolicited_output".into(),
            ClientError::TimedOut => "timeout".into(),
            _ => "transport".into(),
        });
    }
    outcome?;
    let result = closed?;
    if evidence.cancellation.is_some() {
        return Err("adapter campaign cancelled".into());
    }
    if !result.cleanup.is_complete() || !result.status.is_some_and(|status| status.success()) {
        return Err("adapter campaign did not finish successfully".into());
    }
    Ok(())
}

async fn steps(
    client: &mut AdapterClient,
    configuration: &Configuration,
    output: &mut dyn Write,
    statistics: &mut Statistics,
) -> Result<(), Failure> {
    emit(output, "handshake", &client.handshake().await?)?;
    let discovery = client.discover(configuration.payload.clone()).await?;
    emit(output, "discover", &discovery)?;
    let truncated = discovery
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or("discovery response has no truncation flag")?;
    if truncated {
        return Err("discovery was truncated; narrow the campaign scope or raise maxMutants within its bound".into());
    }
    let mutants = discovery
        .get("mutants")
        .and_then(Value::as_array)
        .ok_or("discovery response has no mutants array")?;
    if u64::try_from(mutants.len())? > configuration.candidate_limit {
        return Err("discovery response exceeds the requested candidate limit".into());
    }
    for (index, original) in mutants.iter().enumerate() {
        let mut mutant: Mutant = serde_json::from_value(original.clone())?;
        mutant.validate_model()?;
        let mut validation_request = configuration.payload.clone();
        validation_request["mutant"] = original.clone();
        let validation = client.validate(validation_request).await?;
        let valid = match (
            validation.get("valid").and_then(Value::as_bool),
            validation.get("validation").and_then(Value::as_str),
        ) {
            (Some(true), Some("valid")) => true,
            (Some(false), Some("invalid")) => false,
            _ => return Err("adapter returned inconsistent validation metadata".into()),
        };
        statistics.validated += 1;
        let safe_patch = if valid {
            // Only this local DTO is changed. Original discovery JSON remains
            // unvalidated; static evidence and byte applicability are separate.
            mutant.validation = ValidationState::Valid;
            let bytes = read_and_apply_mutant(&configuration.package, &mutant, MAX_SOURCE_BYTES)?;
            let patched_hash = sha256_hex(&bytes);
            let patched_bytes = bytes.len();
            let patched_utf8 = String::from_utf8(bytes)?;
            statistics.valid += 1;
            json!({
                "applied":true,"mutantId":mutant.id,"file":mutant.file,
                "patchedHash":patched_hash,"patchedBytes":patched_bytes,
                "patchedUtf8":patched_utf8,"writesPerformed":false,
            })
        } else {
            json!({
                "applied":false,"mutantId":mutant.id,"file":mutant.file,
                "reason":"static_invalid","writesPerformed":false,
            })
        };
        emit(
            output,
            "candidate",
            &json!({"index":index,"mutant":original,"validation":validation,"safePatch":safe_patch}),
        )?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Event<'a> {
    step: &'a str,
    payload: &'a Value,
}

fn emit(output: &mut dyn Write, step: &str, payload: &Value) -> Result<(), Failure> {
    serde_json::to_writer(&mut *output, &Event { step, payload })?;
    output.write_all(b"\n")?;
    // The caller can process candidates as they arrive, without waiting for the
    // campaign to finish or buffering the aggregate output in this example.
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn adapter_spec_removes_private_workspace_control_environment() {
        let configuration = Configuration {
            dart: "/dart/bin/dart".into(),
            adapter: "/adapter".into(),
            package: "/subject".into(),
            payload: json!({}),
            candidate_limit: 0,
        };
        let spec = configuration.spec();
        assert!(spec.env.remove.contains(OsStr::new(WORKSPACE_LEASE_ENV)));
        assert!(spec.env.remove.contains(OsStr::new(CANCEL_ENV)));
        assert!(!spec.env.set.contains_key(OsStr::new(WORKSPACE_LEASE_ENV)));
        assert!(spec.validate().is_ok());
    }

    fn control_pipe() -> (OwnedFd, File) {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors is writable storage for the two returned fds.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: a successful pipe returned two fresh, uniquely owned fds.
        unsafe {
            (
                OwnedFd::from_raw_fd(descriptors[0]),
                File::from_raw_fd(descriptors[1]),
            )
        }
    }

    #[tokio::test]
    async fn private_control_pipe_buffers_signals_and_controller_eof() {
        for (byte, reason) in [(b'I', "SIGINT"), (b'T', "SIGTERM")] {
            let (read, mut write) = control_pipe();
            write.write_all(&[byte]).unwrap();
            let control =
                Some(cancellation_descriptor(OsStr::new(&read.as_raw_fd().to_string())).unwrap());
            // SAFETY: source remains borrowed from the live read owner.
            assert_ne!(
                unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            let duplicate = control.as_ref().unwrap().get_ref().as_raw_fd();
            // SAFETY: AsyncFd owns the live duplicate inspected by fcntl.
            assert_ne!(
                unsafe { libc::fcntl(duplicate, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            assert_eq!(control_request(&control).await.unwrap(), reason);
        }
        let (read, write) = control_pipe();
        let control =
            Some(cancellation_descriptor(OsStr::new(&read.as_raw_fd().to_string())).unwrap());
        drop(write);
        assert_eq!(control_request(&control).await.unwrap(), "controller_eof");
    }

    #[tokio::test]
    async fn private_control_rejects_invalid_capabilities_and_request_bytes() {
        for text in [
            "",
            "0",
            "1",
            "2",
            "-1",
            "3.0",
            "not-a-descriptor",
            "999999999999999999999999",
        ] {
            assert!(cancellation_descriptor(OsStr::new(text)).is_err());
        }
        let fixture = Fixture::new("void main() {}\n");
        for path in [fixture.0.clone(), fixture.0.join("lib/source.dart")] {
            let file = File::open(path).unwrap();
            assert!(cancellation_descriptor(OsStr::new(&file.as_raw_fd().to_string())).is_err());
            assert!(file.metadata().is_ok()); // A rejected source was not closed.
        }
        let (read, mut write) = control_pipe();
        assert!(cancellation_descriptor(OsStr::new(&write.as_raw_fd().to_string())).is_err());
        let control =
            Some(cancellation_descriptor(OsStr::new(&read.as_raw_fd().to_string())).unwrap());
        write.write_all(b"X").unwrap();
        assert_eq!(
            control_request(&control).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn payload_read_and_candidate_limits_are_bounded_and_strict() {
        assert!(read_payload(&b"[]"[..]).is_err());
        assert!(read_payload(&b"{"[..]).is_err());
        assert!(read_payload(io::repeat(b' ').take(MAX_LINE_BYTES as u64 + 1)).is_err());
        let exact = format!("{{}}{}", " ".repeat(MAX_LINE_BYTES - 2));
        assert_eq!(read_payload(exact.as_bytes()).unwrap(), json!({}));
        assert_eq!(candidate_limit(&json!({})).unwrap(), 1000);
        for limit in [0, 10_000] {
            assert_eq!(
                candidate_limit(&json!({"limits":{"maxMutants":limit}})).unwrap(),
                limit
            );
        }
        for limit in [json!(-1), json!(10001), json!(1.5), json!(true), json!("2")] {
            assert!(candidate_limit(&json!({"limits":{"maxMutants":limit}})).is_err());
        }
        assert!(candidate_limit(&json!({"limits":null})).is_err());
        assert!(candidate_limit(&json!({"limits":{"unknown":1}})).is_err());
        for count in [0, 1, 2, 4] {
            assert!(Configuration::from_args(vec![OsString::from("unused"); count]).is_err());
        }
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(source: &str) -> Self {
            let path = loop {
                let path = std::env::temp_dir().join(format!(
                    "ff campaign {} {}",
                    std::process::id(),
                    NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create owned campaign fixture: {error}"),
                }
            };
            let fixture = Self(path.canonicalize().unwrap());
            std::fs::create_dir_all(path.join("lib")).unwrap();
            std::fs::create_dir_all(path.join(".dart_tool")).unwrap();
            std::fs::write(path.join("lib/source.dart"), source).unwrap();
            std::fs::write(
                path.join("pubspec.yaml"),
                "name: campaign_fixture\nenvironment:\n  sdk: '>=3.13.0 <3.14.0'\n",
            )
            .unwrap();
            std::fs::write(path.join(".dart_tool/package_config.json"), json!({"configVersion":2,"packages":[{"name":"campaign_fixture","rootUri":"../","packageUri":"lib/","languageVersion":"3.13"}]}).to_string()).unwrap();
            fixture
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn configuration(fixture: &Fixture, operator: &str, max: u64) -> Configuration {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let sdk = root.join(".toolchain/flutter/bin/cache/dart-sdk");
        Configuration {
            dart: sdk.join("bin/dart").into_os_string(),
            adapter: root.join("adapters/dart"),
            package: fixture.0.clone(),
            payload: json!({"workspaceRoot":fixture.0,"packageRoot":".","dartSdkPath":sdk,"rules":{"operators":[operator]},"limits":{"maxMutants":max}}),
            candidate_limit: max,
        }
    }

    fn events(output: &[u8]) -> Vec<Value> {
        std::str::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    #[ignore = "requires the explicitly installed local Dart SDK and adapter package configuration"]
    async fn actual_campaign_streams_every_candidate_keeps_original_json_and_releases_processes() {
        let source = "bool first() => true;\nbool second() => false;\n";
        let fixture = Fixture::new(source);
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                configuration(&fixture, "BOOLEAN_LITERAL_FLIP", 10),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let events = events(&output);
        assert_eq!(
            events
                .iter()
                .map(|event| event["step"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["handshake", "discover", "candidate", "candidate", "cleanup"]
        );
        for event in &events[2..4] {
            let payload = &event["payload"];
            assert_eq!(payload["mutant"]["validation"], "unvalidated");
            assert_eq!(payload["validation"]["valid"], true);
            assert_eq!(payload["safePatch"]["applied"], true);
            assert_eq!(payload["safePatch"]["writesPerformed"], false);
            assert_eq!(payload["mutant"]["id"], payload["safePatch"]["mutantId"]);
            let patched = payload["safePatch"]["patchedUtf8"].as_str().unwrap();
            assert_ne!(patched, source);
            assert_eq!(
                payload["safePatch"]["patchedHash"],
                sha256_hex(patched.as_bytes())
            );
        }
        assert_eq!(events.last().unwrap()["payload"]["complete"], true);
        assert_eq!(events.last().unwrap()["payload"]["activeRuns"], 0);
        assert_eq!(events.last().unwrap()["payload"]["validatedCandidates"], 2);
        assert_eq!(
            std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
            source.as_bytes()
        );
    }

    #[tokio::test]
    #[ignore = "requires the explicitly installed local Dart SDK and adapter package configuration"]
    async fn invalid_and_truncated_discovery_are_explicit_and_still_cleanup() {
        let source =
            "int length(String? value) { if(value != null) return value.length; return 0; }\n";
        let fixture = Fixture::new(source);
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                configuration(&fixture, "CONDITIONAL_NEGATION", 10),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let invalid = events(&output);
        let candidate = invalid
            .iter()
            .find(|event| event["step"] == "candidate")
            .unwrap();
        assert_eq!(candidate["payload"]["validation"]["validation"], "invalid");
        assert_eq!(candidate["payload"]["safePatch"]["applied"], false);
        assert!(
            candidate["payload"]["safePatch"]
                .get("patchedUtf8")
                .is_none()
        );
        assert_eq!(invalid.last().unwrap()["payload"]["complete"], true);

        let mut output = Vec::new();
        let error = tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                configuration(&fixture, "CONDITIONAL_NEGATION", 0),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("truncated"));
        let truncated = events(&output);
        assert_eq!(
            truncated
                .iter()
                .map(|event| event["step"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["handshake", "discover", "cleanup"]
        );
        assert_eq!(truncated[1]["payload"]["truncated"], true);
        assert_eq!(truncated.last().unwrap()["payload"]["complete"], true);
        assert_eq!(
            truncated.last().unwrap()["payload"]["campaignSucceeded"],
            false
        );
        assert_eq!(
            std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
            source.as_bytes()
        );
    }

    struct DisconnectedConsumer;
    impl Write for DisconnectedConsumer {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "consumer closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "requires the explicitly installed local Dart SDK and adapter package configuration"]
    async fn consumer_io_failure_still_closes_the_adapter_and_releases_native_processes() {
        let fixture = Fixture::new("bool first() => true;\n");
        let configuration = configuration(&fixture, "BOOLEAN_LITERAL_FLIP", 10);
        let runner = Arc::new(Runner::new());
        let mut statistics = Statistics::default();
        let mut evidence = SessionEvidence::default();
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            session(
                runner.clone(),
                &configuration,
                &mut DisconnectedConsumer,
                &mut statistics,
                &mut evidence,
                std::future::pending(),
            ),
        )
        .await;
        // Cleanup precedes every assertion so even a failing watchdog or
        // assertion does not leave this fixture's native child unattended.
        let shutdown = tokio::time::timeout(Duration::from_secs(10), runner.shutdown()).await;
        shutdown.unwrap().unwrap();
        assert_eq!(runner.active_runs(), 0);
        assert!(
            outcome
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("consumer closed")
        );
        assert!(evidence.close_succeeded);
        assert_eq!(evidence.exit_code, Some(0));
        assert_eq!(statistics.validated, 0);
        assert_eq!(
            std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
            b"bool first() => true;\n"
        );
    }
}
