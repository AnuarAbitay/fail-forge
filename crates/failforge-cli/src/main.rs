//! Unix CLI. Signal ownership lives here, never inside the runner library.
#[cfg(not(unix))]
compile_error!("FailForge's first stage supports Linux and macOS only");

mod args;
mod dart_namespace;
mod diagnostics;
mod display;
mod mutate;
mod namespace_planner;
mod private_request;
mod private_response;
mod profile;
mod readiness;
mod test_context;
mod test_evidence;
mod workspace_handoff;
mod workspace_inputs;
mod workspace_lifecycle;

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use failforge_runner::{
    CancelReason, CaptureIssue, CommandResult, CompletionCause, Runner, WorkspaceLease,
};
use tokio::signal::unix::{SignalKind, signal};

use args::{Cli, Commands, ProfileCommands};
use diagnostics::Diagnostics;

const DIAGNOSTIC_BUDGET: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code() as u8;
            if let Some(message) = parse_error_diagnostic(&error) {
                let _ = write!(std::io::stderr(), "{message}");
            } else {
                let _ = error.print();
            }
            return ExitCode::from(code);
        }
    };
    let invocation_cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: cannot determine the invocation directory"
            );
            return ExitCode::from(125);
        }
    };
    let arguments = match cli.command {
        Commands::Run(arguments) => arguments,
        Commands::Mutate(arguments) => return mutate::execute(*arguments, &invocation_cwd),
        Commands::WorkspaceInputs => return workspace_inputs::execute(),
        Commands::WorkspaceLifecycle => return workspace_lifecycle::execute(),
        Commands::DartNamespace => return dart_namespace::execute(),
        Commands::Profile(arguments) => match arguments.command {
            ProfileCommands::Generate(arguments) => {
                return profile::execute(*arguments, &invocation_cwd);
            }
        },
    };
    // Retain the optional inherited capability until the runtime and Runner
    // have finished cleanup. Child exec must not inherit either descriptor.
    let workspace_lease = match WorkspaceLease::from_env() {
        Ok(lease) => lease,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: invalid workspace lease capability"
            );
            return ExitCode::from(125);
        }
    };
    let readiness = match readiness::Readiness::from_env() {
        Ok(readiness) => readiness,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: invalid native readiness capability"
            );
            return ExitCode::from(125);
        }
    };
    let evidence = match test_evidence::EvidenceHandoff::from_env(arguments.test_reporter) {
        Ok(evidence) => evidence,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: invalid native evidence capability"
            );
            return ExitCode::from(125);
        }
    };
    let mut spec = match arguments.into_spec(&invocation_cwd) {
        Ok(spec) => spec,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(2);
        }
    };
    if let Some(lease) = &workspace_lease {
        lease.protect_child(&mut spec);
    }
    if let Some(readiness) = &readiness {
        readiness.protect_child(&mut spec);
    }
    if let Some(evidence) = &evidence {
        evidence.protect_child(&mut spec);
    }
    let diagnostics = match Diagnostics::new() {
        Ok(diagnostics) => diagnostics,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr(),
                "failforge: cannot start diagnostic writer"
            );
            return ExitCode::from(125);
        }
    };
    let tracing_writer = diagnostics.clone();
    let _ = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || tracing_writer.clone())
        .try_init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            diagnostics.report("failforge: cannot initialize runtime\n", DIAGNOSTIC_BUDGET);
            return ExitCode::from(125);
        }
    };
    ExitCode::from(runtime.block_on(run(spec, &diagnostics, readiness, evidence)))
}

fn parse_error_diagnostic(error: &clap::Error) -> Option<String> {
    use clap::error::ErrorKind;

    if matches!(
        error.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        return None;
    }
    // Clap's formatted error includes the supplied value or argument. Only its
    // static category is safe here; neither context nor suggestions are printed.
    let category = error
        .kind()
        .as_str()
        .unwrap_or("invalid command-line input");
    Some(format!(
        "failforge: {category}\nFor more information, try '--help'.\n"
    ))
}

async fn run(
    spec: failforge_runner::CommandSpec,
    diagnostics: &Diagnostics,
    readiness: Option<readiness::Readiness>,
    evidence: Option<test_evidence::EvidenceHandoff>,
) -> u8 {
    let cwd = spec.cwd.clone();
    // Install both listeners before spawning: no child is owned if installation
    // fails, and the first signal cannot arrive between spawn and subscription.
    let (mut interrupt, mut terminate) = match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        _ => {
            diagnostics.report(
                "failforge: cannot install signal listeners\n",
                DIAGNOSTIC_BUDGET,
            );
            return 125;
        }
    };
    if readiness.is_some_and(|ready| ready.acknowledge().is_err()) {
        diagnostics.report(
            "failforge: cannot acknowledge native readiness\n",
            DIAGNOSTIC_BUDGET,
        );
        return 125;
    }
    let runner = Runner::new();
    let mut handle = match runner.start(spec) {
        Ok(handle) => handle,
        Err(error) => {
            diagnostics.report(&format!("failforge: {error}\n"), DIAGNOSTIC_BUDGET);
            let _ = runner.shutdown().await;
            return 125;
        }
    };
    let display_failed = handle
        .take_stream()
        .is_some_and(|receiver| display::spawn(receiver).is_err());
    if display_failed {
        handle.force_cancel(CancelReason::User);
        diagnostics.report("failforge: cannot start stream writer\n", DIAGNOSTIC_BUDGET);
    }
    let mut interrupts = 0u8;
    let mut interrupt_open = true;
    let mut terminate_open = true;
    let outcome = loop {
        tokio::select! {
            // A ready result is consumed before another CLI signal. The runner
            // independently owns and freezes the original stop decision.
            biased;
            result = handle.wait() => break result,
            event = interrupt.recv(), if interrupt_open => {
                if event.is_some() {
                    interrupts = interrupts.saturating_add(1);
                    if interrupts == 1 {
                        if handle.cancel(CancelReason::CtrlC) {
                            diagnostics.report("failforge: cancellation requested (Ctrl+C)\n", DIAGNOSTIC_BUDGET);
                        }
                    } else {
                        handle.force_cancel(CancelReason::CtrlC);
                    }
                } else {
                    interrupt_open = false;
                }
            }
            event = terminate.recv(), if terminate_open => {
                if event.is_some() {
                    if handle.cancel(CancelReason::SigTerm) {
                        diagnostics.report("failforge: cancellation requested (SIGTERM)\n", DIAGNOSTIC_BUDGET);
                    }
                } else {
                    terminate_open = false;
                }
            }
        }
    };
    // Never terminate the runtime while the runner still owns cleanup work.
    let shutdown = runner.shutdown().await;
    let native_ok = outcome.is_ok() && shutdown.is_ok() && !display_failed;
    let mut code = match &outcome {
        Ok(result) => {
            diagnostics.report(&summary(result), DIAGNOSTIC_BUDGET);
            result_exit_code(result)
        }
        Err(error) => {
            diagnostics.report(&format!("failforge: {error}\n"), DIAGNOSTIC_BUDGET);
            if let Some(result) = error.partial_result() {
                diagnostics.report(&summary(result), DIAGNOSTIC_BUDGET);
            }
            125
        }
    };
    if let Err(error) = shutdown {
        diagnostics.report(&format!("failforge: {error}\n"), DIAGNOSTIC_BUDGET);
        code = 125;
    }
    if display_failed {
        code = 125;
    }
    if let Some(evidence) = evidence {
        let result = match &outcome {
            Ok(result) => Some(result.as_ref()),
            Err(error) => error.partial_result(),
        };
        if result.is_none_or(|result| evidence.publish(result, code, native_ok, &cwd).is_err()) {
            diagnostics.report(
                "failforge: cannot publish native test evidence\n",
                DIAGNOSTIC_BUDGET,
            );
            code = 125;
        }
    }
    code
}

fn result_exit_code(result: &CommandResult) -> u8 {
    if !result.cleanup.is_complete() {
        return 125;
    }
    match result.cause {
        CompletionCause::TimedOut => 124,
        CompletionCause::Cancelled(CancelReason::CtrlC) => 130,
        CompletionCause::Cancelled(CancelReason::SigTerm) => 143,
        CompletionCause::Cancelled(_) | CompletionCause::RunnerFailure => 125,
        CompletionCause::ProcessCompleted => result
            .status
            .and_then(|status| {
                status
                    .code()
                    .or_else(|| status.signal().map(|signal| 128 + signal))
            })
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(125),
    }
}

fn summary(result: &CommandResult) -> String {
    let status = match result.status {
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("exit:{code}"),
            (_, Some(signal)) => format!("signal:{signal}"),
            _ => "unknown".to_owned(),
        },
        None => "unavailable".to_owned(),
    };
    let mut report = format!(
        "failforge: cause={:?} status={} execution_ms={} stdout_bytes={} stderr_bytes={} cleanup={} display_dropped_bytes={} display_unconfirmed_bytes={} stdout_issue={} stderr_issue={} stdout_tail_truncated={} stderr_tail_truncated={}\n",
        result.cause,
        status,
        result.timings.execution.as_millis(),
        result.stdout.bytes_read,
        result.stderr.bytes_read,
        if result.cleanup.is_complete() {
            "complete"
        } else {
            "incomplete"
        },
        result
            .stdout
            .stream_dropped_bytes
            .saturating_add(result.stderr.stream_dropped_bytes),
        result
            .stdout
            .stream_unconfirmed_bytes
            .saturating_add(result.stderr.stream_unconfirmed_bytes),
        capture_issue_label(result.stdout.issue.as_ref()),
        capture_issue_label(result.stderr.issue.as_ref()),
        result.stdout.tail_truncated,
        result.stderr.tail_truncated,
    );
    if !result.cleanup.is_complete() {
        use failforge_runner::{GroupState, SignalOutcome};
        let group = match &result.cleanup.group {
            GroupState::NoLiveMembers { .. } => "no_live",
            GroupState::LiveMembers { .. } => "live",
            GroupState::Unknown => "unknown",
        };
        // CleanupReport is a public value: never emit arbitrary error strings.
        // Only fixed stages and native integer signal errors leave this boundary.
        let first_error = match result.cleanup.errors.first().map(String::as_str) {
            Some(error) if error.starts_with("group observation: ") => "group-observation",
            Some(error) if error.starts_with("observe during cleanup: ") => "cleanup-observation",
            Some(error) if error.starts_with("observe: ") => "observation",
            Some(error) if error.starts_with("signal: ") => "signal",
            Some(error) if error.starts_with("reap: ") => "reap",
            Some(error) if error.starts_with("output setup: ") => "output-setup",
            Some("forced cleanup deadline expired") => "deadline",
            Some(_) => "other",
            None => "none",
        };
        let failed_signal = result
            .cleanup
            .signals
            .iter()
            .find_map(|signal| {
                if let SignalOutcome::Failed { errno } = signal.outcome {
                    Some(errno.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "none".into());
        // A distinct prefix preserves the strict machine-summary contract.
        report.push_str(&format!(
            "failforge-cleanup: child_reaped={} output_finished={} group={} errors={} first_error={} failed_signal_errno={} stdout_eof={} stderr_eof={} stdout_writer_finished={} stderr_writer_finished={}\n",
            result.cleanup.child_reaped, result.cleanup.output_finished, group,
            result.cleanup.errors.len(), first_error, failed_signal,
            result.stdout.eof, result.stderr.eof,
            result.stdout.writer_finished, result.stderr.writer_finished,
        ));
    }
    report
}

fn capture_issue_label(issue: Option<&CaptureIssue>) -> &'static str {
    match issue {
        None => "none",
        Some(CaptureIssue::ReadError(_)) => "read-error",
        Some(CaptureIssue::WriteError(_)) => "write-error",
        Some(CaptureIssue::FileQuotaExceeded) => "file-quota-exceeded",
        Some(CaptureIssue::DrainExpired) => "drain-expired",
        Some(CaptureIssue::StreamError(_)) => "stream-error",
        Some(CaptureIssue::CounterOverflow) => "counter-overflow",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use failforge_runner::{CleanupReport, GroupState};
    use std::process::ExitStatus;

    fn result(cause: CompletionCause, raw_status: Option<i32>) -> CommandResult {
        CommandResult {
            cause,
            status: raw_status.map(ExitStatus::from_raw),
            timings: Default::default(),
            stdout: Default::default(),
            stderr: Default::default(),
            cleanup: CleanupReport {
                child_reaped: true,
                output_finished: true,
                group: GroupState::NoLiveMembers { zombies: 0 },
                ..Default::default()
            },
        }
    }

    #[test]
    fn forwards_every_child_exit_code_including_reserved_codes() {
        for code in 0..=255 {
            let result = result(CompletionCause::ProcessCompleted, Some(code << 8));
            assert_eq!(result_exit_code(&result), code as u8);
        }
    }

    #[test]
    fn signals_use_the_documented_unix_mapping() {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGKILL, libc::SIGABRT] {
            assert_eq!(
                result_exit_code(&result(CompletionCause::ProcessCompleted, Some(signal))),
                (128 + signal) as u8
            );
        }
    }

    #[test]
    fn stop_cause_has_priority_over_actual_status() {
        for raw in [None, Some(0), Some(37 << 8), Some(libc::SIGKILL)] {
            for (cause, code) in [
                (CompletionCause::TimedOut, 124),
                (CompletionCause::Cancelled(CancelReason::CtrlC), 130),
                (CompletionCause::Cancelled(CancelReason::SigTerm), 143),
                (CompletionCause::RunnerFailure, 125),
                (CompletionCause::Cancelled(CancelReason::User), 125),
                (CompletionCause::Cancelled(CancelReason::HandleDropped), 125),
                (CompletionCause::Cancelled(CancelReason::Shutdown), 125),
            ] {
                assert_eq!(result_exit_code(&result(cause, raw)), code);
            }
        }
    }

    #[test]
    fn incomplete_cleanup_has_priority_over_every_cause() {
        for cause in [
            CompletionCause::TimedOut,
            CompletionCause::ProcessCompleted,
            CompletionCause::Cancelled(CancelReason::CtrlC),
            CompletionCause::Cancelled(CancelReason::SigTerm),
        ] {
            let mut result = result(cause, Some(0));
            result.cleanup.child_reaped = false;
            assert_eq!(result_exit_code(&result), 125);
        }
        assert_eq!(
            result_exit_code(&result(CompletionCause::ProcessCompleted, None)),
            125
        );
    }

    #[test]
    fn summary_separates_cause_and_actual_status_without_raw_output() {
        let mut result = result(CompletionCause::TimedOut, Some(0));
        result.stdout.tail = b"SECRET_STDOUT".to_vec();
        result.stderr.tail = b"SECRET_STDERR".to_vec();
        result.stdout.bytes_read = 13;
        result.stderr.bytes_read = 14;
        result.stdout.stream_dropped_bytes = 5;
        result.stderr.stream_unconfirmed_bytes = 7;
        let summary = summary(&result);
        assert!(summary.contains("cause=TimedOut status=exit:0"));
        assert!(summary.contains("stdout_bytes=13 stderr_bytes=14"));
        assert!(summary.contains("display_dropped_bytes=5 display_unconfirmed_bytes=7"));
        assert!(!summary.contains("SECRET"));
    }

    #[test]
    fn summary_reports_signal_missing_status_and_incomplete_cleanup() {
        let mut signalled = result(CompletionCause::ProcessCompleted, Some(libc::SIGTERM));
        assert!(summary(&signalled).contains(&format!("status=signal:{}", libc::SIGTERM)));
        signalled.status = None;
        signalled.cleanup.group = GroupState::Unknown;
        assert!(summary(&signalled).contains("status=unavailable"));
        assert!(summary(&signalled).contains("cleanup=incomplete"));
    }

    #[test]
    fn incomplete_cleanup_diagnostic_identifies_failed_evidence_and_signal_errno() {
        use failforge_runner::{SignalOutcome, SignalRecord};
        let mut partial = result(CompletionCause::ProcessCompleted, Some(0));
        partial
            .cleanup
            .errors
            .push("signal: PermissionDenied".into());
        partial.cleanup.signals.push(SignalRecord {
            signal: libc::SIGTERM,
            after_spawn: Duration::ZERO,
            outcome: SignalOutcome::Failed { errno: libc::EPERM },
        });
        let diagnostic = summary(&partial);
        assert!(diagnostic.contains("failforge-cleanup: child_reaped=true output_finished=true group=no_live errors=1 first_error=signal"));
        assert!(diagnostic.contains(&format!("failed_signal_errno={}", libc::EPERM)));
        for (group, label) in [
            (GroupState::Unknown, "unknown"),
            (GroupState::LiveMembers { count: 2 }, "live"),
        ] {
            partial.cleanup.group = group;
            partial.cleanup.child_reaped = false;
            partial.cleanup.output_finished = false;
            let diagnostic = summary(&partial);
            assert!(diagnostic.contains(&format!(
                "child_reaped=false output_finished=false group={label}"
            )));
        }
        assert!(
            !summary(&result(CompletionCause::ProcessCompleted, Some(0)))
                .contains("failforge-cleanup:")
        );
    }

    #[test]
    fn incomplete_cleanup_diagnostic_never_prints_arbitrary_error_messages() {
        let mut partial = result(CompletionCause::ProcessCompleted, Some(0));
        partial.cleanup.errors.push("SECRET_CLEANUP_MESSAGE".into());
        partial.stdout.tail = b"SECRET_STDOUT".to_vec();
        let diagnostic = summary(&partial);
        assert!(diagnostic.contains("errors=1 first_error=other"));
        assert!(!diagnostic.contains("SECRET"));
    }

    #[test]
    fn capture_issue_labels_are_specific_and_redacted() {
        assert_eq!(capture_issue_label(None), "none");
        for (issue, label) in [
            (CaptureIssue::ReadError("SECRET".into()), "read-error"),
            (CaptureIssue::WriteError("SECRET".into()), "write-error"),
            (CaptureIssue::FileQuotaExceeded, "file-quota-exceeded"),
            (CaptureIssue::DrainExpired, "drain-expired"),
            (CaptureIssue::StreamError("SECRET".into()), "stream-error"),
            (CaptureIssue::CounterOverflow, "counter-overflow"),
        ] {
            assert_eq!(capture_issue_label(Some(&issue)), label);
            let mut result = result(CompletionCause::RunnerFailure, None);
            result.stdout.issue = Some(issue);
            result.stdout.tail_truncated = true;
            let summary = summary(&result);
            assert!(summary.contains(&format!("stdout_issue={label}")));
            assert!(summary.contains("stdout_tail_truncated=true"));
            assert!(!summary.contains("SECRET"));
        }
    }
}
