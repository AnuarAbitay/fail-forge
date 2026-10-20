use std::fs::File;
use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;

use failforge_runner::{
    CancelReason, CommandResult, CommandSpec, OutputMode, OutputStream, RunHandle, Runner,
    RunnerError, StreamReceiver,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

use crate::{
    AdapterCommand, AdapterFailure, MAX_LINE_BYTES, PROTOCOL_VERSION, ProtocolError,
    RequestEnvelope, ResponseEnvelope, ResponseKind,
};

#[derive(Debug, Clone, Copy)]
pub struct ClientOptions {
    pub request_timeout: Duration,
    /// JSON bytes excluding LF. Cannot exceed the shared eight-MiB limit.
    pub max_line_bytes: usize,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(10),
            max_line_bytes: MAX_LINE_BYTES,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid adapter client limits")]
    InvalidLimits,
    #[error("adapter JSON line exceeds its byte limit")]
    LineTooLarge,
    #[error("adapter payload exceeds its supported nesting depth")]
    PayloadTooDeep,
    #[error("adapter disconnected before a complete response line")]
    UnexpectedEof,
    #[error("adapter emitted unsolicited response bytes")]
    UnsolicitedResponse,
    #[error("adapter stream lost output bytes")]
    OutputLost,
    #[error("adapter session cannot be reused after an interrupted request")]
    InterruptedRequest,
    #[error("adapter request exceeded its deadline")]
    TimedOut,
    #[error("adapter request identifier space exhausted")]
    IdentifierExhausted,
    #[error("adapter output stream could not be started")]
    MissingStream,
    #[error("adapter error {0:?}")]
    Remote(AdapterFailure),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("adapter JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("adapter JSON contains invalid or duplicate metadata")]
    InvalidMetadata,
    #[error("adapter transport I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("adapter spawn failed: {0}")]
    Spawn(#[from] RunnerError),
    #[error("adapter execution failed: {0}")]
    Execution(Arc<RunnerError>),
}

/// One sequential JSONL session. It reuses the supplied Runner and never calls
/// its global shutdown. Dropping a request future makes reuse fail closed;
/// dropping the client closes stdin and requests cleanup of only its own run.
pub struct AdapterClient {
    _runner: Arc<Runner>,
    handle: RunHandle,
    stream: StreamReceiver,
    writer: UnixStream,
    options: ClientOptions,
    close_budget: Duration,
    next_id: u64,
    in_flight: bool,
    poisoned: bool,
}

/// The process receipt survives post-execution protocol and capture failures.
/// An owner can authenticate both captures before classifying cancellation.
#[derive(Debug)]
pub struct ClientClose {
    pub outcome: Result<Arc<CommandResult>, ClientError>,
    pub result: Option<Arc<CommandResult>>,
}

impl AdapterClient {
    pub fn start(
        runner: Arc<Runner>,
        mut spec: CommandSpec,
        options: ClientOptions,
    ) -> Result<Self, ClientError> {
        if options.max_line_bytes == 0
            || options.max_line_bytes > MAX_LINE_BYTES
            || options.request_timeout.is_zero()
            || tokio::time::Instant::now()
                .checked_add(options.request_timeout)
                .is_none()
        {
            return Err(ClientError::InvalidLimits);
        }
        let close_budget = options
            .request_timeout
            .checked_add(spec.timeout_policy.grace_period)
            .and_then(|time| time.checked_add(spec.timeout_policy.forced_wait))
            .and_then(|time| time.checked_add(spec.timeout_policy.output_drain))
            .and_then(|time| time.checked_add(Duration::from_secs(1)))
            .ok_or(ClientError::InvalidLimits)?;
        if tokio::time::Instant::now()
            .checked_add(close_budget)
            .is_none()
        {
            return Err(ClientError::InvalidLimits);
        }
        // Protocol transport bounds are independent of total adapter output.
        // File capture can still be explicitly configured by the caller.
        spec.capture_policy.mode = OutputMode::StreamAndCapture;
        spec.capture_policy.read_chunk_bytes = 32 * 1024;
        spec.capture_policy.stream_queue_bytes =
            2 * (options.max_line_bytes + spec.capture_policy.read_chunk_bytes);
        spec.capture_policy.tail_bytes_per_stream = 64 * 1024;
        let (parent, child) = std::os::unix::net::UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let child = File::from(OwnedFd::from(child));
        let mut handle = runner.start_with_stdin(spec, child)?;
        let stream = handle.take_stream().ok_or(ClientError::MissingStream)?;
        let writer = UnixStream::from_std(parent)?;
        Ok(Self {
            _runner: runner,
            handle,
            stream,
            writer,
            options,
            close_budget,
            next_id: 1,
            in_flight: false,
            poisoned: false,
        })
    }

    pub async fn handshake(&mut self) -> Result<Value, ClientError> {
        self.request(AdapterCommand::Handshake, serde_json::json!({}))
            .await
    }
    pub async fn discover(&mut self, payload: Value) -> Result<Value, ClientError> {
        self.request(AdapterCommand::Discover, payload).await
    }
    pub async fn validate(&mut self, payload: Value) -> Result<Value, ClientError> {
        self.request(AdapterCommand::Validate, payload).await
    }

    pub async fn request(
        &mut self,
        command: AdapterCommand,
        payload: Value,
    ) -> Result<Value, ClientError> {
        if self.in_flight || self.poisoned {
            self.poison();
            return Err(ClientError::InterruptedRequest);
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(ClientError::IdentifierExhausted)?;
        let request = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: format!("rust-{id}"),
            command,
            payload,
        };
        let bytes = encode_request(&request, self.options.max_line_bytes)?;
        self.in_flight = true;
        let result = tokio::time::timeout(
            self.options.request_timeout,
            self.exchange(&bytes, &request.request_id, command),
        )
        .await;
        match result {
            Ok(Ok(response)) => {
                self.in_flight = false;
                if response.kind == ResponseKind::Error {
                    return Err(ClientError::Remote(
                        response.error.expect("validated structured error"),
                    ));
                }
                Ok(response.payload.expect("validated payload presence"))
            }
            Ok(Err(error)) => {
                self.poison();
                Err(error)
            }
            Err(_) => {
                self.poison();
                Err(ClientError::TimedOut)
            }
        }
    }

    fn poison(&mut self) {
        self.poisoned = true;
        self.handle.force_cancel(CancelReason::User);
    }

    async fn exchange(
        &mut self,
        request: &[u8],
        id: &str,
        command: AdapterCommand,
    ) -> Result<ResponseEnvelope, ClientError> {
        self.check_loss()?;
        self.writer.write_all(request).await?;
        self.writer.write_all(b"\n").await?;
        let mut line = Vec::new();
        loop {
            let mut chunk = self.stream.recv().await.ok_or(ClientError::UnexpectedEof)?;
            let bytes = chunk.bytes();
            let finished = if chunk.stream == OutputStream::Stdout {
                append_response_chunk(&mut line, bytes, self.options.max_line_bytes)?
            } else {
                false
            };
            let consumed = chunk.bytes().len();
            chunk.mark_displayed(consumed);
            self.check_loss()?;
            if finished {
                // Preserve the existing syntax-error category, then reject
                // duplicate nested keys before Value can silently collapse
                // contradictory adapter metadata.
                let value =
                    failforge_workspace::parse_json(&line).map_err(
                        |_| match serde_json::from_slice::<ResponseEnvelope>(&line) {
                            Err(error) => ClientError::Json(error),
                            Ok(_) => ClientError::InvalidMetadata,
                        },
                    )?;
                let response: ResponseEnvelope = serde_json::from_value(value)?;
                response.validate_for(id, command)?;
                return Ok(response);
            }
        }
    }

    fn check_loss(&self) -> Result<(), ClientError> {
        if self.stream.dropped_bytes() != (0, 0) {
            return Err(ClientError::OutputLost);
        }
        Ok(())
    }

    /// Closes adapter input and waits for this run only. The actual status is
    /// returned for the caller to classify; an adapter error cannot shut down
    /// unrelated commands using the shared Runner.
    pub async fn close(self) -> Result<Arc<CommandResult>, ClientError> {
        self.close_with_result().await.outcome
    }

    pub async fn close_with_result(self) -> ClientClose {
        let mut result = None;
        let outcome = self.close_recorded(&mut result).await;
        ClientClose { outcome, result }
    }

    async fn close_recorded(
        mut self,
        evidence: &mut Option<Arc<CommandResult>>,
    ) -> Result<Arc<CommandResult>, ClientError> {
        if self.poisoned {
            self.handle.force_cancel(CancelReason::User);
        } else if self.in_flight {
            // A dropped caller future requests ordinary owned cancellation.
            // Honor its configured grace period before escalating: an adapter
            // already exiting after SIGTERM must not receive an unnecessary
            // immediate SIGKILL. Transport/protocol poisoning remains forced.
            self.handle.cancel(CancelReason::User);
        }
        let input_error = self.writer.shutdown().await.err();
        let mut unsolicited = false;
        let outcome=tokio::time::timeout(self.close_budget,async {
            loop {
                tokio::select! {
                    result=self.handle.wait()=>break result,
                    chunk=self.stream.recv()=>if let Some(mut chunk)=chunk { let length=chunk.bytes().len();if chunk.stream==OutputStream::Stdout && length!=0 {unsolicited=true;}chunk.mark_displayed(length); }
                        else { break self.handle.wait().await; }
                }
            }
        }).await;
        let result = match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                *evidence = error
                    .partial_result()
                    .map(|result| Arc::new(result.clone()));
                return Err(ClientError::Execution(error));
            }
            Err(_) => {
                self.handle.force_cancel(CancelReason::User);
                // Keep the sole result consumer alive through owned cleanup.
                // Runner's forced-wait/output-drain policies bound this wait.
                *evidence = match self.handle.wait().await {
                    Ok(result) => Some(result),
                    Err(error) => error
                        .partial_result()
                        .map(|result| Arc::new(result.clone())),
                };
                return Err(ClientError::TimedOut);
            }
        };
        *evidence = Some(result.clone());
        self.check_loss()?;
        if result.stdout.stream_dropped_bytes != 0 || result.stderr.stream_dropped_bytes != 0 {
            return Err(ClientError::OutputLost);
        }
        if unsolicited {
            return Err(ClientError::UnsolicitedResponse);
        }
        if let Some(error) = input_error {
            return Err(ClientError::Io(error));
        }
        Ok(result)
    }
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    overflow: bool,
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflow = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "JSON line limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_request(request: &RequestEnvelope, limit: usize) -> Result<Vec<u8>, ClientError> {
    request.validate()?;
    let mut payload_budget = limit;
    check_payload(&request.payload, 0, &mut payload_budget)?;
    let mut buffer = LimitedBuffer {
        bytes: Vec::new(),
        limit,
        overflow: false,
    };
    if let Err(error) = serde_json::to_writer(&mut buffer, request) {
        return if buffer.overflow {
            Err(ClientError::LineTooLarge)
        } else {
            Err(ClientError::Json(error))
        };
    }
    Ok(buffer.bytes)
}

// Bound recursive serialization before it can consume an arbitrary caller-
// constructed Value stack. The budget is a lower bound on wire bytes; the
// LimitedBuffer still enforces the precise encoded byte limit afterward.
fn check_payload(value: &Value, depth: usize, budget: &mut usize) -> Result<(), ClientError> {
    if depth > 64 {
        return Err(ClientError::PayloadTooDeep);
    }
    *budget = budget.checked_sub(1).ok_or(ClientError::LineTooLarge)?;
    match value {
        Value::String(text) => {
            *budget = budget
                .checked_sub(text.len())
                .ok_or(ClientError::LineTooLarge)?;
        }
        Value::Array(values) => {
            for child in values {
                check_payload(child, depth + 1, budget)?;
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                *budget = budget
                    .checked_sub(key.len())
                    .ok_or(ClientError::LineTooLarge)?;
                check_payload(child, depth + 1, budget)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn append_response_chunk(
    line: &mut Vec<u8>,
    bytes: &[u8],
    limit: usize,
) -> Result<bool, ClientError> {
    let newline = bytes.iter().position(|byte| *byte == b'\n');
    let length = newline.unwrap_or(bytes.len());
    if length > limit.saturating_sub(line.len()) {
        return Err(ClientError::LineTooLarge);
    }
    if let Some(newline) = newline {
        if newline + 1 != bytes.len() {
            return Err(ClientError::UnsolicitedResponse);
        }
    }
    line.extend_from_slice(&bytes[..length]);
    Ok(newline.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serialization_is_bounded_before_any_transport_write() {
        let request = RequestEnvelope {
            protocol_version: 1,
            request_id: "1".into(),
            command: AdapterCommand::Handshake,
            payload: serde_json::json!({}),
        };
        let exact = serde_json::to_vec(&request).unwrap();
        assert_eq!(encode_request(&request, exact.len()).unwrap(), exact);
        assert!(matches!(
            encode_request(&request, exact.len() - 1),
            Err(ClientError::LineTooLarge)
        ));
    }
    #[test]
    fn response_framing_bounds_fragmentation_and_extra_output() {
        let mut line = Vec::new();
        assert!(!append_response_chunk(&mut line, b"{", 2).unwrap());
        assert!(append_response_chunk(&mut line, b"}\n", 2).unwrap());
        assert_eq!(line, b"{}");
        assert!(matches!(
            append_response_chunk(&mut Vec::new(), b"{}\n{}\n", 10),
            Err(ClientError::UnsolicitedResponse)
        ));
        assert!(matches!(
            append_response_chunk(&mut Vec::new(), b"abc", 2),
            Err(ClientError::LineTooLarge)
        ));
        assert!(matches!(
            append_response_chunk(&mut line, b"x", 2),
            Err(ClientError::LineTooLarge)
        ));
    }
    #[test]
    fn deeply_nested_or_large_payloads_fail_before_serialization() {
        let mut payload = Value::Null;
        for _ in 0..66 {
            payload = Value::Array(vec![payload]);
        }
        let request = RequestEnvelope {
            protocol_version: 1,
            request_id: "1".into(),
            command: AdapterCommand::Handshake,
            payload: serde_json::json!({"deep":payload}),
        };
        assert!(matches!(
            encode_request(&request, MAX_LINE_BYTES),
            Err(ClientError::PayloadTooDeep)
        ));
        let request = RequestEnvelope {
            payload: serde_json::json!({"large":"x".repeat(1000)}),
            ..request
        };
        assert!(matches!(
            encode_request(&request, 100),
            Err(ClientError::LineTooLarge)
        ));
    }
}
