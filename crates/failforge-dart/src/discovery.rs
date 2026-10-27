//! One owned discovery/validation session with source-bound patch evidence.
//!
//! The sink receives bounded LF-delimited records as candidates arrive. The
//! caller must retain and verify the prepared source snapshot and authenticate
//! capture files before publishing this session as campaign evidence.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use failforge_runner::{CommandResult, CommandSpec, EnvSpec, Runner};
use failforge_workspace::Cancellation;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{AdapterClient, ClientError, ClientOptions, MAX_LINE_BYTES, MutationFamily};

mod candidate_proof;
mod retained_transcript;
mod validation;
pub use candidate_proof::{VerifiedCandidate, verify_candidate};
pub use retained_transcript::{RetainedTranscript, load_retained_transcript};
use validation::{candidate, candidate_limit, discovered, operator_catalogue, package_root};

pub const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const CLEANUP_RESERVE: usize = 64 * 1024;
const PRIVATE_ENV: [&str; 11] = [
    "FAILFORGE_WORKSPACE_LEASE_FD",
    "FAILFORGE_ADAPTER_CANCEL_FD",
    "FAILFORGE_ADAPTER_CANCEL_PIPE",
    "FAILFORGE_CLI_CANCEL_PIPE",
    "FAILFORGE_RUNNER_READY_FD",
    "FAILFORGE_TEST_EVIDENCE_FD",
    "FAILFORGE_TEST_CONTEXT_FD",
    "FAILFORGE_WORKSPACE_REQUEST_FD",
    "FAILFORGE_WORKSPACE_HANDOFF_FD",
    "FAILFORGE_WORKSPACE_CAPS_FDS",
    "FAILFORGE_REPORT_FDS",
];

/// Validated inputs. Constructing this object never starts an adapter process.
pub struct DiscoveryConfig {
    dart: OsString,
    adapter: PathBuf,
    package: PathBuf,
    payload: Value,
    family: MutationFamily,
    candidate_limit: u64,
    capture_dir: Option<PathBuf>,
    options: ClientOptions,
}

impl DiscoveryConfig {
    pub fn new(
        dart: OsString,
        adapter: PathBuf,
        payload: Value,
        family: MutationFamily,
    ) -> Result<Self, DiscoveryError> {
        if !payload.is_object() {
            return Err(DiscoveryError::Invalid(
                "campaign payload must be an object",
            ));
        }
        // Caller-constructed Values can exceed parser nesting limits. Bound
        // their depth before asking serde to recursively serialize anything.
        let mut budget = MAX_LINE_BYTES;
        validate_value(&payload, 0, &mut budget)?;
        let mut encoded = LimitedBuffer::new(MAX_LINE_BYTES);
        serde_json::to_writer(&mut encoded, &payload)
            .map_err(|_| DiscoveryError::Invalid("campaign payload exceeds its byte limit"))?;
        let package = package_root(&payload)?;
        let candidate_limit = candidate_limit(&payload)?;
        let adapter = adapter.canonicalize().map_err(io_error)?;
        if !adapter.is_dir() {
            return Err(DiscoveryError::Invalid("adapter root must be a directory"));
        }
        let config = Self {
            dart,
            adapter,
            package,
            payload,
            family,
            candidate_limit,
            capture_dir: None,
            options: ClientOptions {
                request_timeout: Duration::from_secs(60),
                ..ClientOptions::default()
            },
        };
        config
            .spec()
            .validate()
            .map_err(|_| DiscoveryError::Invalid("invalid adapter command"))?;
        Ok(config)
    }

    pub fn with_capture_dir(mut self, directory: PathBuf) -> Result<Self, DiscoveryError> {
        if !directory.is_absolute() || directory.as_os_str().as_encoded_bytes().contains(&0) {
            return Err(DiscoveryError::Invalid(
                "adapter capture directory must be absolute",
            ));
        }
        self.capture_dir = Some(directory);
        Ok(self)
    }

    pub fn with_client_options(mut self, options: ClientOptions) -> Result<Self, DiscoveryError> {
        if options.max_line_bytes == 0
            || options.max_line_bytes > MAX_LINE_BYTES
            || options.request_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(options.request_timeout)
                .is_none()
        {
            return Err(DiscoveryError::Invalid("invalid adapter response limits"));
        }
        self.options = options;
        Ok(self)
    }

    pub fn package_root(&self) -> &Path {
        &self.package
    }

    fn spec(&self) -> CommandSpec {
        let mut env = EnvSpec::default();
        env.remove.extend(PRIVATE_ENV.into_iter().map(Into::into));
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
            env,
            timeout_policy: Default::default(),
            capture_policy: failforge_runner::CapturePolicy {
                capture_dir: self.capture_dir.clone(),
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("adapter campaign cancelled")]
    Cancelled(i32),
    #[error("discovery I/O failed ({0:?})")]
    Io(io::ErrorKind),
    #[error("adapter session failed ({0})")]
    Adapter(&'static str),
    #[error("candidate source or patch is invalid")]
    Patch,
    #[error("adapter runner shutdown was incomplete")]
    Shutdown,
}

fn io_error(error: io::Error) -> DiscoveryError {
    DiscoveryError::Io(error.kind())
}
fn adapter_error(error: &ClientError) -> DiscoveryError {
    DiscoveryError::Adapter(match error {
        ClientError::TimedOut => "timeout",
        ClientError::OutputLost => "output_lost",
        ClientError::UnsolicitedResponse => "unsolicited_output",
        ClientError::LineTooLarge => "response_byte_limit",
        ClientError::InvalidMetadata | ClientError::Json(_) => "invalid_json",
        ClientError::Protocol(_) => "protocol",
        ClientError::Execution(_) => "runner_failure",
        ClientError::Spawn(_) => "spawn",
        ClientError::Remote(_) => "remote",
        _ => "transport",
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateEvidence {
    pub index: usize,
    /// Original discovery JSON is retained exactly, including optional nulls.
    pub mutant: Value,
    pub validation: Value,
    pub safe_patch: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoverySteps {
    pub handshake: Value,
    pub discover: Value,
    pub cleanup: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveryResult {
    pub steps: DiscoverySteps,
    pub candidates: Vec<CandidateEvidence>,
}

/// Cleanup evidence is retained even when a protocol, sink or cancellation
/// failure prevents a complete candidate result.
#[derive(Debug)]
pub struct DiscoveryRun {
    pub outcome: Result<DiscoveryResult, DiscoveryError>,
    pub cleanup: Value,
    pub command_result: Option<Arc<CommandResult>>,
}

#[derive(Default)]
struct SessionEvidence {
    close_succeeded: bool,
    close_failure: Option<&'static str>,
    cancellation: Option<i32>,
    result: Option<Arc<CommandResult>>,
}

pub async fn discover(
    config: &DiscoveryConfig,
    output: &mut dyn Write,
    cancel: &Cancellation,
) -> DiscoveryRun {
    let runner = Arc::new(Runner::new());
    let mut sink = EventSink { output, emitted: 0 };
    let mut candidates = Vec::new();
    let mut evidence = SessionEvidence::default();
    let mut outcome = match cancel.signum() {
        Some(signal) => {
            evidence.cancellation = Some(signal);
            Err(DiscoveryError::Cancelled(signal))
        }
        None => {
            session(
                &runner,
                config,
                &mut sink,
                &mut candidates,
                &mut evidence,
                cancel,
            )
            .await
        }
    };
    // Explicit ownership completion precedes every return, including failed
    // spawn, malformed metadata, interrupted requests and a disconnected sink.
    let shutdown = runner.shutdown().await;
    let active_runs = runner.active_runs();
    if shutdown.is_err() || active_runs != 0 {
        outcome = Err(DiscoveryError::Shutdown);
    }
    if let Some(result) = &evidence.result {
        if !capture_complete(result, config.capture_dir.is_some()) {
            outcome = Err(DiscoveryError::Adapter("capture_incomplete"));
        }
    }
    if outcome.is_ok() {
        if let Some(signal) = cancel.signum() {
            evidence.cancellation = Some(signal);
            outcome = Err(DiscoveryError::Cancelled(signal));
        }
    }
    let mut cleanup = json!({
        "complete":shutdown.is_ok() && active_runs == 0,
        "adapterCloseSucceeded":evidence.close_succeeded,
        "adapterCloseFailure":evidence.close_failure,
        "adapterCloseFailureDetails":null,
        "exitCode":evidence.result.as_ref().and_then(|r| r.status.and_then(|s| s.code())),
        "runnerShutdown":shutdown.is_ok(), "activeRuns":active_runs,
        "campaignSucceeded":outcome.is_ok(),
        "cancelled":evidence.cancellation.is_some(),
        "cancellationReason":evidence.cancellation.map(|signal| if signal == libc::SIGTERM {"SIGTERM"} else {"SIGINT"}),
        "validatedCandidates":candidates.len(),
        "staticallyValidCandidates":candidates.iter().filter(|c| c.validation["valid"] == true).count(),
        "writesPerformed":false,
    });
    if let Err(error) = sink.emit("cleanup", &cleanup, true) {
        outcome = Err(error);
        cleanup["campaignSucceeded"] = Value::Bool(false);
    }
    let outcome = outcome.map(|(handshake, discovery)| DiscoveryResult {
        steps: DiscoverySteps {
            handshake,
            discover: discovery,
            cleanup: cleanup.clone(),
        },
        candidates,
    });
    DiscoveryRun {
        outcome,
        cleanup,
        command_result: evidence.result,
    }
}

fn capture_complete(result: &CommandResult, files: bool) -> bool {
    result.cleanup.is_complete()
        && [&result.stdout, &result.stderr].into_iter().all(|stream| {
            stream.eof
                && stream.writer_finished
                && stream.issue.is_none()
                && stream.stream_dropped_bytes == 0
                && stream.sha256.is_some()
                && (!files
                    || (stream.file_path.is_some()
                        && stream.file_bytes_written == stream.bytes_read))
        })
}

async fn session(
    runner: &Arc<Runner>,
    config: &DiscoveryConfig,
    sink: &mut EventSink<'_>,
    candidates: &mut Vec<CandidateEvidence>,
    evidence: &mut SessionEvidence,
    cancel: &Cancellation,
) -> Result<(Value, Value), DiscoveryError> {
    let mut client = AdapterClient::start(runner.clone(), config.spec(), config.options)
        .map_err(|error| adapter_error(&error))?;
    let mut outcome = tokio::select! {
        biased;
        result = steps(&mut client, config, sink, candidates) => result,
        signal = cancel.cancelled() => {
            evidence.cancellation = Some(signal);
            Err(DiscoveryError::Cancelled(signal))
        },
    };
    let close = client.close_with_result();
    tokio::pin!(close);
    let closed = if evidence.cancellation.is_some() {
        close.await
    } else {
        tokio::select! {
            biased;
            result = &mut close => result,
            signal = cancel.cancelled() => {
                if outcome.is_ok() {
                    evidence.cancellation = Some(signal);
                    outcome = Err(DiscoveryError::Cancelled(signal));
                }
                let (_, result) = tokio::join!(runner.shutdown(), &mut close);
                result
            },
        }
    };
    evidence.result = closed.result;
    match closed.outcome {
        Ok(result) => {
            evidence.close_succeeded = result.cleanup.is_complete();
            let success = result.status.is_some_and(|status| status.success());
            if outcome.is_ok() && !success {
                outcome = Err(DiscoveryError::Adapter("exit_status"));
            }
        }
        Err(error) => {
            let failure = adapter_error(&error);
            evidence.close_failure = match &failure {
                DiscoveryError::Adapter(kind) => Some(*kind),
                _ => None,
            };
            // A failed close cannot authenticate an otherwise clean stop.
            if outcome.is_ok() || matches!(outcome, Err(DiscoveryError::Cancelled(_))) {
                outcome = Err(failure);
            }
        }
    }
    outcome
}

async fn steps(
    client: &mut AdapterClient,
    config: &DiscoveryConfig,
    sink: &mut EventSink<'_>,
    candidates: &mut Vec<CandidateEvidence>,
) -> Result<(Value, Value), DiscoveryError> {
    let handshake = client
        .handshake()
        .await
        .map_err(|error| adapter_error(&error))?;
    let operators: BTreeMap<String, (MutationFamily, u64)> = operator_catalogue(&handshake)?;
    sink.emit("handshake", &handshake, false)?;
    let discovery = client
        .discover(config.payload.clone())
        .await
        .map_err(|error| adapter_error(&error))?;
    // Keep an authentic truncated response in the diagnostic transcript, while
    // never exposing it as a successful discovery result.
    sink.emit("discover", &discovery, false)?;
    let originals = discovered(
        &discovery,
        &operators,
        config.family,
        config.candidate_limit,
    )?;
    for (index, original) in originals.iter().enumerate() {
        let mut request = config.payload.clone();
        request["mutant"] = original.clone();
        let validation = client
            .validate(request)
            .await
            .map_err(|error| adapter_error(&error))?;
        let candidate = candidate(index, original, validation, &config.package)?;
        emit_candidate(sink, candidates, candidate)?;
    }
    Ok((handshake, discovery))
}

fn emit_candidate(
    sink: &mut EventSink<'_>,
    candidates: &mut Vec<CandidateEvidence>,
    candidate: CandidateEvidence,
) -> Result<(), DiscoveryError> {
    let before = sink.emitted;
    let emitted = sink.emit("candidate", &candidate, false);
    // A failed flush can follow a complete frame write. Count that frame, but
    // preserve the I/O failure so it can never become a successful campaign.
    if sink.emitted > before {
        candidates.push(candidate);
    }
    emitted
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}
impl LimitedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "discovery JSON byte limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct EventSink<'a> {
    output: &'a mut dyn Write,
    emitted: usize,
}
impl EventSink<'_> {
    fn emit(
        &mut self,
        step: &str,
        payload: &impl Serialize,
        cleanup: bool,
    ) -> Result<(), DiscoveryError> {
        #[derive(Serialize)]
        struct Event<'a, T> {
            step: &'a str,
            payload: &'a T,
        }
        let limit = if cleanup {
            CLEANUP_RESERVE / 2
        } else {
            MAX_LINE_BYTES
        };
        let mut buffer = LimitedBuffer::new(limit);
        serde_json::to_writer(&mut buffer, &Event { step, payload }).map_err(|_| {
            DiscoveryError::Invalid("discovery event exceeds the eight-MiB byte limit")
        })?;
        buffer.bytes.push(b'\n');
        let ceiling = MAX_TRANSCRIPT_BYTES
            - if cleanup {
                CLEANUP_RESERVE / 2
            } else {
                CLEANUP_RESERVE
            };
        if buffer.bytes.len() > ceiling.saturating_sub(self.emitted) {
            return Err(DiscoveryError::Invalid(
                "discovery transcript exceeds the 64-MiB aggregate limit",
            ));
        }
        self.output.write_all(&buffer.bytes).map_err(io_error)?;
        self.emitted += buffer.bytes.len();
        self.output.flush().map_err(io_error)?;
        Ok(())
    }
}

fn validate_value(value: &Value, depth: usize, budget: &mut usize) -> Result<(), DiscoveryError> {
    let failure = || DiscoveryError::Invalid("campaign payload exceeds byte or nesting limits");
    if depth > 64 {
        return Err(failure());
    }
    *budget = budget.checked_sub(1).ok_or_else(failure)?;
    match value {
        Value::String(text) => {
            *budget = budget.checked_sub(text.len()).ok_or_else(failure)?;
        }
        Value::Array(values) => {
            for child in values {
                validate_value(child, depth + 1, budget)?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                *budget = budget.checked_sub(key.len()).ok_or_else(failure)?;
                validate_value(child, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests;
