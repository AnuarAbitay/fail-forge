//! Conservative Dart/Flutter test verdicts bound to a complete native capture.
//!
//! Reporter diagnostics do not authenticate exception ownership. In particular,
//! loading failures stay unattributed until campaign-level causal replay.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::process::ExitStatusExt;

use failforge_runner::{CommandResult, CompletionCause};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::test_reporter::{Reporter, parse_event};

pub const MAX_CAPTURE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_EVENTS: usize = 100_000;
const MAX_TIMEOUT_AUTHORS: usize = 8;
const MAX_TIMEOUT_IDENTITY_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReporterMode {
    Dart,
    Flutter,
}

impl ReporterMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Dart => "dart",
            Self::Flutter => "flutter",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RunnerSummaryError(&'static str);

fn field_limit(key: &str) -> Option<u128> {
    match key {
        "execution_ms" => Some(u128::MAX),
        "stdout_bytes" | "stderr_bytes" | "display_dropped_bytes" | "display_unconfirmed_bytes" => {
            Some(u128::from(u64::MAX))
        }
        _ => None,
    }
}

fn known_field(key: &[u8]) -> bool {
    matches!(
        key,
        b"execution_ms"
            | b"stdout_bytes"
            | b"stderr_bytes"
            | b"display_dropped_bytes"
            | b"display_unconfirmed_bytes"
            | b"cause"
            | b"status"
            | b"cleanup"
            | b"stdout_issue"
            | b"stderr_issue"
            | b"stdout_tail_truncated"
            | b"stderr_tail_truncated"
    )
}

fn decimal(value: &str, limit: u128) -> Result<u128, RunnerSummaryError> {
    if value.is_empty()
        || value.len() > 39
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(RunnerSummaryError("invalid CLI summary integer"));
    }
    value
        .parse::<u128>()
        .ok()
        .filter(|number| *number <= limit)
        .ok_or(RunnerSummaryError("invalid CLI summary integer"))
}

/// Parse one unambiguous native summary. Diagnostic lines cannot provide missing
/// fields, and repeated, unknown or noncanonical fields are rejected.
///
/// This compatibility API is for persisted CLI receipts; native owners should
/// pass their [`CommandResult`] to [`classify_dart_result`] instead.
pub fn parse_cli_summary(stderr: &[u8]) -> Result<BTreeMap<String, String>, RunnerSummaryError> {
    fn whitespace(byte: u8) -> bool {
        matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
    }
    let mut candidate = None;
    for line in stderr.split(|byte| matches!(byte, b'\n' | b'\r')) {
        let Some(tail) = line.strip_prefix(b"failforge: ") else {
            continue;
        };
        let tokens: Vec<_> = tail
            .split(|byte| whitespace(*byte))
            .filter(|part| !part.is_empty())
            .collect();
        if !tokens
            .iter()
            .any(|token| known_field(token.split(|byte| *byte == b'=').next().unwrap_or_default()))
        {
            continue;
        }
        if candidate.replace(tokens).is_some() {
            return Err(RunnerSummaryError(
                "CLI must report exactly one unambiguous summary line",
            ));
        }
    }
    let tokens = candidate.ok_or(RunnerSummaryError(
        "CLI must report exactly one unambiguous summary line",
    ))?;
    let mut summary = BTreeMap::new();
    for token in tokens {
        let mut parts = token.split(|byte| *byte == b'=');
        let key = parts.next().unwrap_or_default();
        let value = parts
            .next()
            .ok_or(RunnerSummaryError("malformed CLI summary field"))?;
        if parts.next().is_some() || !known_field(key) || value.is_empty() {
            return Err(RunnerSummaryError("unknown or malformed CLI summary field"));
        }
        let key = std::str::from_utf8(key)
            .map_err(|_| RunnerSummaryError("invalid CLI summary encoding"))?;
        let value = std::str::from_utf8(value)
            .map_err(|_| RunnerSummaryError("invalid CLI summary encoding"))?;
        if summary.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(RunnerSummaryError("duplicate CLI summary field"));
        }
    }
    for (key, value) in &summary {
        if let Some(limit) = field_limit(key) {
            decimal(value, limit)?;
        }
        let valid = match key.as_str() {
            "cause" => matches!(
                value.as_str(),
                "ProcessCompleted"
                    | "TimedOut"
                    | "RunnerFailure"
                    | "Cancelled(User)"
                    | "Cancelled(CtrlC)"
                    | "Cancelled(SigTerm)"
                    | "Cancelled(HandleDropped)"
                    | "Cancelled(Shutdown)"
            ),
            "status" => {
                if matches!(value.as_str(), "unknown" | "unavailable") {
                    true
                } else if let Some(code) = value.strip_prefix("exit:") {
                    decimal(code, 255)?;
                    true
                } else if let Some(signal) = value.strip_prefix("signal:") {
                    decimal(signal, i32::MAX as u128)? != 0
                } else {
                    false
                }
            }
            "cleanup" => matches!(value.as_str(), "complete" | "incomplete"),
            "stdout_issue" | "stderr_issue" => matches!(
                value.as_str(),
                "none"
                    | "read-error"
                    | "write-error"
                    | "file-quota-exceeded"
                    | "drain-expired"
                    | "stream-error"
                    | "counter-overflow"
            ),
            "stdout_tail_truncated" | "stderr_tail_truncated" => {
                matches!(value.as_str(), "true" | "false")
            }
            _ => true,
        };
        if !valid {
            return Err(RunnerSummaryError("invalid CLI summary value"));
        }
    }
    for (count, tail) in [
        ("stdout_bytes", "stdout_tail_truncated"),
        ("stderr_bytes", "stderr_tail_truncated"),
    ] {
        if summary.get(count).is_some_and(|value| value == "0")
            && summary.get(tail).is_some_and(|value| value == "true")
        {
            return Err(RunnerSummaryError(
                "zero-byte CLI stream reported a truncated tail",
            ));
        }
    }
    Ok(summary)
}

#[derive(Debug)]
struct RunnerReceipt {
    cause: String,
    status: String,
    cleanup: bool,
    capture_issue: bool,
    stdout_bytes: u64,
    digest: CaptureDigest,
}

#[derive(Debug, Clone, Copy)]
enum CaptureDigest {
    LegacyReceipt,
    Native(Option<[u8; 32]>),
}

impl RunnerReceipt {
    fn projection(&self) -> Value {
        json!({"cause": self.cause, "status": self.status,
            "cleanup": if self.cleanup { "complete" } else { "incomplete" }})
    }
}

fn answer(reporter: &Reporter, runner: &Value, outcome: &str, reason: &str) -> Value {
    json!({"outcome": outcome, "reason": reason, "tests": reporter.counts,
        "reporterComplete": reporter.finished, "runner": runner,
        "loadFailures": reporter.load_failures})
}

/// Classify captured reporter bytes against a strict legacy CLI receipt.
/// `cli_exit` is the owner's exit status, not a status inferred from reporter text.
/// Legacy receipts bind byte counts, not capture digests. This compatibility API
/// is for trusted archived captures; live execution should use
/// [`classify_dart_result`] with the native owner's digest.
pub fn classify_dart_capture(
    stdout: &[u8],
    cli_exit: i32,
    cli_stderr: &[u8],
    mode: ReporterMode,
) -> Value {
    classify_capture(stdout, cli_exit, cli_stderr, mode, Limits::default())
}

fn classify_capture(
    stdout: &[u8],
    cli_exit: i32,
    cli_stderr: &[u8],
    mode: ReporterMode,
    limits: Limits,
) -> Value {
    let reporter = Reporter::new(mode.as_str());
    let Ok(fields) = parse_cli_summary(cli_stderr) else {
        return answer(
            &reporter,
            &json!({}),
            "INFRA_ERROR",
            "invalid_runner_summary",
        );
    };
    if [
        "cause",
        "status",
        "cleanup",
        "stdout_bytes",
        "stdout_issue",
        "stderr_issue",
    ]
    .iter()
    .any(|field| !fields.contains_key(*field))
    {
        return answer(
            &reporter,
            &json!({}),
            "INFRA_ERROR",
            "incomplete_runner_summary",
        );
    }
    let receipt = RunnerReceipt {
        cause: fields["cause"].clone(),
        status: fields["status"].clone(),
        cleanup: fields["cleanup"] == "complete",
        capture_issue: fields["stdout_issue"] != "none" || fields["stderr_issue"] != "none",
        stdout_bytes: fields["stdout_bytes"]
            .parse()
            .expect("summary counter checked"),
        digest: CaptureDigest::LegacyReceipt,
    };
    classify(stdout, cli_exit, &receipt, reporter, limits)
}

/// Classify the full stdout capture using the native owner's typed result.
/// No shell process or textual summary is used on this path. Capture integrity,
/// cleanup and termination always take precedence over reporter assertions. The
/// bytes must match the digest computed by Runner while reading the child pipe;
/// a missing digest or a modified capture cannot establish a test verdict.
///
/// This is a primary attempt verdict. Campaigns must still verify the pristine
/// baseline, selected author-test catalogue and unchanged execution inputs
/// before accepting it as a mutation result.
pub fn classify_dart_result(stdout: &[u8], result: &CommandResult, mode: ReporterMode) -> Value {
    let status = match result.status {
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("exit:{code}"),
            (_, Some(signal)) => format!("signal:{signal}"),
            _ => "unknown".to_owned(),
        },
        None => "unavailable".to_owned(),
    };
    let receipt = RunnerReceipt {
        cause: format!("{:?}", result.cause),
        status,
        cleanup: result.cleanup.is_complete(),
        capture_issue: result.stdout.issue.is_some() || result.stderr.issue.is_some(),
        stdout_bytes: result.stdout.bytes_read,
        digest: CaptureDigest::Native(result.stdout.sha256),
    };
    // Other completion causes are rejected independently of this code.
    let exit = if result.cause == CompletionCause::TimedOut {
        124
    } else {
        result
            .status
            .and_then(|status| status.code())
            .unwrap_or(125)
    };
    classify(
        stdout,
        exit,
        &receipt,
        Reporter::new(mode.as_str()),
        Limits::default(),
    )
}

#[derive(Clone, Copy)]
struct Limits {
    capture: usize,
    event: usize,
    events: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            capture: MAX_CAPTURE_BYTES,
            event: MAX_EVENT_BYTES,
            events: MAX_EVENTS,
        }
    }
}

fn classify(
    raw: &[u8],
    exit: i32,
    receipt: &RunnerReceipt,
    mut reporter: Reporter,
    limits: Limits,
) -> Value {
    let runner = receipt.projection();
    let fail = |reporter: &Reporter, reason| answer(reporter, &runner, "INFRA_ERROR", reason);
    if !receipt.cleanup {
        return fail(&reporter, "incomplete_runner_cleanup");
    }
    if receipt.capture_issue {
        return fail(&reporter, "runner_capture_issue");
    }
    let timed_out = receipt.cause == "TimedOut";
    if timed_out {
        if exit != 124 {
            return fail(&reporter, "timeout_exit_code_mismatch");
        }
    } else {
        if receipt.cause != "ProcessCompleted" {
            return fail(&reporter, "runner_not_completed");
        }
        let Some(code) = receipt.status.strip_prefix("exit:") else {
            return fail(&reporter, "no_normal_child_exit");
        };
        if code.parse::<i32>().ok() != Some(exit) {
            return fail(&reporter, "child_exit_code_mismatch");
        }
    }
    if raw.len() > limits.capture {
        return fail(&reporter, "capture_limit_exceeded");
    }
    if u64::try_from(raw.len()).ok() != Some(receipt.stdout_bytes) {
        return fail(&reporter, "incomplete_stdout_capture");
    }
    if let CaptureDigest::Native(expected) = receipt.digest {
        let Some(expected) = expected else {
            return fail(&reporter, "missing_capture_digest");
        };
        let observed: [u8; 32] = Sha256::digest(raw).into();
        if observed != expected {
            return fail(&reporter, "capture_digest_mismatch");
        }
    }
    if timed_out {
        return timeout_prefix(raw, reporter, &runner, limits);
    }
    if raw.iter().filter(|byte| **byte == b'\n').count() >= limits.events {
        return fail(&reporter, "event_limit_exceeded");
    }
    for line in raw.split(|byte| *byte == b'\n') {
        if blank(line) {
            continue;
        }
        if line.len() > limits.event {
            return fail(&reporter, "event_limit_exceeded");
        }
        let event = match parse_event(line) {
            Ok(event) => event,
            Err(error) => return fail(&reporter, error.0),
        };
        if let Err(error) = reporter.consume(&event) {
            return fail(&reporter, error.0);
        }
    }
    if !reporter.finished {
        return fail(&reporter, "missing_done_event");
    }
    if reporter.compile_load_failures > 0 || reporter.other_load_failures > 0 {
        let mut result = if exit == 0 {
            fail(&reporter, "failed_load_exit_code_mismatch")
        } else if reporter.compile_load_failures > 0 {
            answer(&reporter, &runner, "INVALID", "compiler_load_failure")
        } else {
            answer(
                &reporter,
                &runner,
                "LOAD_FAILURE",
                "suite_load_failure_unattributed",
            )
        };
        result["phase"] = json!("suite_loading");
        return result;
    }
    if reporter.counts.failed > 0 {
        if exit == 0 {
            return fail(&reporter, "failed_test_exit_code_mismatch");
        }
        let diagnostics = timeout_diagnostics(
            "test_timeout",
            reporter
                .tests
                .iter()
                .filter(|(_, test)| !test.loading)
                .map(|(id, test)| {
                    (
                        *id,
                        test.suite_path.as_str(),
                        test.test_name.as_str(),
                        false,
                        test.result.as_deref() == Some("error") && test.timeout_diagnostic_observed,
                    )
                }),
        );
        if diagnostics["errorObserved"] == true {
            let mut result = answer(&reporter, &runner, "KILLED", "test_timeout");
            result["timeoutDiagnostics"] = diagnostics;
            return result;
        }
        return answer(&reporter, &runner, "KILLED", "user_test_failure");
    }
    if reporter.counts.passed > 0 {
        return if exit == 0 {
            answer(&reporter, &runner, "SURVIVED", "user_tests_passed")
        } else {
            fail(&reporter, "successful_tests_exit_code_mismatch")
        };
    }
    if matches!(exit, 0 | 79) {
        answer(&reporter, &runner, "NO_TESTS", "no_executed_user_tests")
    } else {
        fail(&reporter, "no_tests_exit_code_mismatch")
    }
}

fn blank(line: &[u8]) -> bool {
    line.iter().all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
}

fn timeout_diagnostics<'a>(
    kind: &str,
    authors: impl Iterator<Item = (u64, &'a str, &'a str, bool, bool)>,
) -> Value {
    let (mut observed, mut pending) = (Vec::new(), Vec::new());
    let (mut observed_total, mut pending_total) = (0usize, 0usize);
    for (id, suite, name, is_pending, timeout_observed) in authors {
        if !is_pending && !timeout_observed {
            continue;
        }
        let identity = json!({"testId": id,
            "suitePath": suite.chars().take(MAX_TIMEOUT_IDENTITY_CHARS).collect::<String>(),
            "testName": name.chars().take(MAX_TIMEOUT_IDENTITY_CHARS).collect::<String>(),
            "identityTruncated": suite.chars().count() > MAX_TIMEOUT_IDENTITY_CHARS || name.chars().count() > MAX_TIMEOUT_IDENTITY_CHARS});
        if timeout_observed {
            observed_total += 1;
            if observed.len() < MAX_TIMEOUT_AUTHORS {
                observed.push(identity.clone());
            }
        }
        if is_pending {
            pending_total += 1;
            if pending.len() < MAX_TIMEOUT_AUTHORS {
                pending.push(identity);
            }
        }
    }
    json!({"kind": kind, "scope": "current_capture", "cause": "not_established",
        "evidenceSource": if kind == "runner_timeout" { "native_runner_summary" } else { "package_test_json_diagnostic" },
        "errorObserved": observed_total > 0,
        "observedInnerTimeouts": {"total": observed_total, "truncated": observed_total > observed.len(), "authorTests": observed},
        "pendingAuthorTests": {"total": pending_total, "truncated": pending_total > pending.len(), "authorTests": pending}})
}

struct AuthorProgress {
    id: u64,
    suite: String,
    name: String,
    pending: bool,
    timeout_observed: bool,
}

fn timeout_prefix(raw: &[u8], mut reporter: Reporter, runner: &Value, limits: Limits) -> Value {
    let mut counts = reporter.counts.clone();
    let (mut finished, mut failure_count) = (false, 0usize);
    let mut progress = json!({"eventsParsed":0,"pendingTests":0,"pendingLoadingTests":0,
        "stopReason":"capture_ended","lastEventTimeMs":null,"unparsedBytes":0,"captureBytesVerified":true});
    let (mut offset, mut consumed, mut lines) = (0usize, 0usize, 0usize);
    let mut authors = Vec::<AuthorProgress>::new();
    let mut author_indices = HashMap::new();
    let (mut pending, mut pending_loading, mut parsed) = (0usize, 0usize, 0usize);
    while offset < raw.len() {
        let newline = raw[offset..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| offset + index);
        let end = newline.unwrap_or(raw.len());
        let next_offset = end + usize::from(newline.is_some());
        if lines >= limits.events || end - offset > limits.event {
            progress["stopReason"] = json!("event_limit_exceeded");
            progress["diagnostic"] = json!("event_limit_exceeded");
            break;
        }
        lines += 1;
        let line = &raw[offset..end];
        if blank(line) {
            consumed = next_offset;
            offset = next_offset;
            continue;
        }
        let event = match parse_event(line) {
            Ok(event) => event,
            Err(error) => {
                progress["stopReason"] =
                    json!(if error.is_interrupted_candidate() && newline.is_none() {
                        "interrupted_final_event"
                    } else {
                        "invalid_reporter_event"
                    });
                progress["diagnostic"] = json!(error.0);
                break;
            }
        };
        if let Err(error) = reporter.consume(&event) {
            progress["stopReason"] = json!("invalid_reporter_event");
            progress["diagnostic"] = json!(error.0);
            break;
        }
        // Commit only after a whole event is accepted: consume may mutate state
        // before rejecting an event. Never copy the growing reporter graph.
        counts = reporter.counts.clone();
        finished = reporter.finished;
        failure_count = reporter.load_failures.len();
        parsed += 1;
        progress["lastEventTimeMs"] = json!(reporter.last_time);
        match event["type"].as_str() {
            Some("testStart") => {
                let id = event["test"]["id"].as_u64().expect("accepted test ID");
                let test = reporter.test(id).expect("accepted test");
                if test.loading {
                    pending_loading += 1;
                } else {
                    pending += 1;
                    author_indices.insert(id, authors.len());
                    authors.push(AuthorProgress {
                        id,
                        suite: test.suite_path.clone(),
                        name: test.test_name.clone(),
                        pending: true,
                        timeout_observed: false,
                    });
                }
            }
            Some("error") => {
                let id = event["testID"].as_u64().expect("accepted error ID");
                if let Some(index) = author_indices.get(&id) {
                    authors[*index].timeout_observed = reporter
                        .test(id)
                        .expect("accepted test")
                        .timeout_diagnostic_observed;
                }
            }
            Some("testDone") => {
                let id = event["testID"].as_u64().expect("accepted completion ID");
                if reporter.test(id).expect("accepted test").loading {
                    pending_loading -= 1;
                } else {
                    pending -= 1;
                    authors[*author_indices.get(&id).expect("accepted author")].pending = false;
                }
            }
            _ => {}
        }
        consumed = next_offset;
        offset = next_offset;
    }
    if offset == raw.len() {
        progress["stopReason"] = json!(if finished {
            "reporter_done"
        } else {
            "capture_ended"
        });
    }
    progress["eventsParsed"] = json!(parsed);
    progress["pendingTests"] = json!(pending);
    progress["pendingLoadingTests"] = json!(pending_loading);
    progress["unparsedBytes"] = json!(raw.len() - consumed);
    let mut result = answer(&reporter, runner, "TIMEOUT", "runner_timeout");
    result["tests"] = json!(counts);
    result["reporterComplete"] = json!(finished);
    result["loadFailures"] = json!(&reporter.load_failures[..failure_count]);
    result["testsPartial"] = json!(true);
    result["timeoutProgress"] = progress;
    result["timeoutDiagnostics"] = timeout_diagnostics(
        "runner_timeout",
        authors.iter().map(|author| {
            (
                author.id,
                author.suite.as_str(),
                author.name.as_str(),
                author.pending,
                author.timeout_observed,
            )
        }),
    );
    result
}

#[cfg(test)]
#[path = "test_outcome_tests.rs"]
mod tests;
