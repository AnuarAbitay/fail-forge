//! Outcomes preserve the stop decision separately from the operating system status.
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    User,
    CtrlC,
    SigTerm,
    HandleDropped,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionCause {
    ProcessCompleted,
    TimedOut,
    Cancelled(CancelReason),
    RunnerFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureIssue {
    ReadError(String),
    WriteError(String),
    FileQuotaExceeded,
    DrainExpired,
    StreamError(String),
    CounterOverflow,
}

#[derive(Debug, Clone, Default)]
pub struct CaptureSummary {
    pub bytes_read: u64,
    pub tail: Vec<u8>,
    pub tail_truncated: bool,
    pub file_path: Option<PathBuf>,
    pub file_bytes_written: u64,
    pub stream_dropped_bytes: u64,
    pub stream_unconfirmed_bytes: u64,
    pub eof: bool,
    pub writer_finished: bool,
    pub issue: Option<CaptureIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupState {
    NoLiveMembers { zombies: usize },
    LiveMembers { count: usize },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalOutcome {
    Sent,
    AlreadyGone,
    Failed { errno: i32 },
}

#[derive(Debug, Clone)]
pub struct SignalRecord {
    pub signal: i32,
    pub after_spawn: Duration,
    pub outcome: SignalOutcome,
}

#[derive(Debug, Clone)]
pub struct CleanupReport {
    pub child_reaped: bool,
    pub signals: Vec<SignalRecord>,
    pub errors: Vec<String>,
    pub group: GroupState,
    pub output_finished: bool,
}

impl Default for CleanupReport {
    fn default() -> Self {
        Self {
            child_reaped: false,
            signals: Vec::new(),
            errors: Vec::new(),
            group: GroupState::Unknown,
            output_finished: false,
        }
    }
}

impl CleanupReport {
    /// True only when the direct child was reaped, no live group members were
    /// observed before retiring the PGID, and capture completed without errors.
    pub fn is_complete(&self) -> bool {
        self.child_reaped
            && self.output_finished
            && self.errors.is_empty()
            && matches!(self.group, GroupState::NoLiveMembers { .. })
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunTimings {
    pub preparation: Duration,
    pub spawn: Duration,
    pub execution: Duration,
    pub cleanup: Duration,
    pub output_drain: Duration,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub cause: CompletionCause,
    pub status: Option<ExitStatus>,
    pub timings: RunTimings,
    pub stdout: CaptureSummary,
    pub stderr: CaptureSummary,
    pub cleanup: CleanupReport,
}

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("invalid command: {0}")]
    InvalidSpec(crate::SpecError),
    #[error("{operation} failed ({kind:?}, OS error {os_error:?})")]
    Preparation {
        operation: &'static str,
        kind: io::ErrorKind,
        os_error: Option<i32>,
    },
    #[error("runner is shutting down")]
    ShuttingDown,
    #[error("a Tokio runtime with I/O and time enabled is required")]
    NoRuntime,
    #[error("time budget cannot be represented by the system clock")]
    TimeBudgetOutOfRange,
    #[error("run failed during {operation}; partial result is available")]
    Partial {
        operation: &'static str,
        partial: Box<CommandResult>,
    },
    #[error("shutdown incomplete: {active} supervised runs remain")]
    ShutdownIncomplete { active: usize },
    #[error("shutdown encountered {runs} unacknowledged or incomplete run failures")]
    ShutdownFailed { runs: usize },
    #[error("runner accounting limit reached: {active} active or retained runs")]
    RunLimitReached { active: usize },
}

impl RunnerError {
    pub(crate) fn preparation(operation: &'static str, error: io::Error) -> Self {
        Self::Preparation {
            operation,
            kind: error.kind(),
            os_error: error.raw_os_error(),
        }
    }

    pub fn partial_result(&self) -> Option<&CommandResult> {
        match self {
            Self::Partial { partial, .. } => Some(partial),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn complete_cleanup_requires_all_evidence() {
        let good = CleanupReport {
            child_reaped: true,
            output_finished: true,
            group: GroupState::NoLiveMembers { zombies: 2 },
            ..Default::default()
        };
        assert!(good.is_complete());
        let mut cases = vec![CleanupReport::default()];
        let mut report = good.clone();
        report.child_reaped = false;
        cases.push(report);
        let mut report = good.clone();
        report.output_finished = false;
        cases.push(report);
        let mut report = good.clone();
        report.group = GroupState::Unknown;
        cases.push(report);
        let mut report = good.clone();
        report.group = GroupState::LiveMembers { count: 1 };
        cases.push(report);
        let mut report = good;
        report.errors.push("wait failed".into());
        cases.push(report);
        for report in cases {
            assert!(!report.is_complete());
        }
    }
    #[test]
    fn preparation_errors_preserve_kind_without_secret_text() {
        let error = RunnerError::preparation(
            "spawn",
            io::Error::new(io::ErrorKind::NotFound, "SECRET_ARG"),
        );
        assert!(matches!(
            error,
            RunnerError::Preparation {
                kind: io::ErrorKind::NotFound,
                ..
            }
        ));
        assert!(!error.to_string().contains("SECRET_ARG"));
        assert!(error.partial_result().is_none());
    }
    #[test]
    fn partial_result_preserves_original_cause_and_capture() {
        let result = CommandResult {
            cause: CompletionCause::TimedOut,
            status: None,
            timings: Default::default(),
            stdout: CaptureSummary {
                tail: vec![0, 255],
                bytes_read: 2,
                ..Default::default()
            },
            stderr: Default::default(),
            cleanup: Default::default(),
        };
        let error = RunnerError::Partial {
            operation: "output",
            partial: Box::new(result),
        };
        let result = error.partial_result().unwrap();
        assert_eq!(result.cause, CompletionCause::TimedOut);
        assert_eq!(result.stdout.tail, [0, 255]);
    }
}
