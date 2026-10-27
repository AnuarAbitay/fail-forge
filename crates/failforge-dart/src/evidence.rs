use failforge_runner::{CommandResult, CompletionCause};
use serde::{Deserialize, Serialize};

/// A test-output consumer must supply this evidence. No generic nonzero exit
/// code is interpreted as an assertion failure. Counts refer to executed tests,
/// excluding synthetic loader tests in Flutter's machine event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestEvidence {
    pub baseline_succeeded: bool,
    pub machine_session_complete: bool,
    pub completed_tests: u64,
    pub assertion_failures: u64,
    pub compile_or_load_failed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestRunClassification {
    Passed,
    TestFailure,
    CompileOrLoadFailure,
    NoTests,
    TimedOut,
    Cancelled,
    InfrastructureError,
    BaselineUnavailable,
    Unclassified,
}

/// This small extension point classifies a process plus externally validated
/// machine evidence. It does not assign KILLED/SURVIVED or combine failure-mode
/// mutations with classic mutation scores.
pub fn classify_test_run(result: &CommandResult, evidence: &TestEvidence) -> TestRunClassification {
    use TestRunClassification as Class;
    if !result.cleanup.is_complete()
        || result.status.is_none()
        || result.cause == CompletionCause::RunnerFailure
    {
        return Class::InfrastructureError;
    }
    match result.cause {
        CompletionCause::TimedOut => return Class::TimedOut,
        CompletionCause::Cancelled(_) => return Class::Cancelled,
        _ => {}
    }
    let Some(code) = result.status.and_then(|status| status.code()) else {
        return Class::InfrastructureError;
    };
    #[cfg(windows)]
    if code < 0 {
        // Native exception statuses are not ordinary Dart test failures.
        return Class::InfrastructureError;
    }
    if !evidence.baseline_succeeded {
        return Class::BaselineUnavailable;
    }
    if !evidence.machine_session_complete {
        return Class::Unclassified;
    }
    if evidence.compile_or_load_failed {
        return Class::CompileOrLoadFailure;
    }
    if evidence.assertion_failures > evidence.completed_tests {
        return Class::InfrastructureError;
    }
    if evidence.completed_tests == 0 {
        return Class::NoTests;
    }
    if evidence.assertion_failures > 0 {
        return if code != 0 {
            Class::TestFailure
        } else {
            Class::InfrastructureError
        };
    }
    if code == 0 {
        Class::Passed
    } else {
        Class::Unclassified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use failforge_runner::{CancelReason, CaptureSummary, CleanupReport, GroupState};
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    fn process(code: i32) -> CommandResult {
        CommandResult {
            cause: CompletionCause::ProcessCompleted,
            status: Some(native_status(code)),
            timings: Default::default(),
            stdout: CaptureSummary::default(),
            stderr: CaptureSummary::default(),
            cleanup: CleanupReport {
                child_reaped: true,
                output_finished: true,
                group: GroupState::NoLiveMembers { zombies: 0 },
                ..Default::default()
            },
        }
    }
    fn native_status(code: i32) -> std::process::ExitStatus {
        #[cfg(unix)]
        {
            std::process::ExitStatus::from_raw(code << 8)
        }
        #[cfg(windows)]
        {
            std::process::ExitStatus::from_raw(code as u32)
        }
    }
    #[test]
    fn exit_status_without_machine_evidence_never_proves_test_failure() {
        let mut evidence = TestEvidence {
            baseline_succeeded: true,
            ..Default::default()
        };
        assert_eq!(
            classify_test_run(&process(1), &evidence),
            TestRunClassification::Unclassified
        );
        evidence.machine_session_complete = true;
        assert_eq!(
            classify_test_run(&process(0), &evidence),
            TestRunClassification::NoTests
        );
        evidence.completed_tests = 1;
        assert_eq!(
            classify_test_run(&process(1), &evidence),
            TestRunClassification::Unclassified
        );
        assert_eq!(
            classify_test_run(&process(0), &evidence),
            TestRunClassification::Passed
        );
        evidence.assertion_failures = 1;
        assert_eq!(
            classify_test_run(&process(1), &evidence),
            TestRunClassification::TestFailure
        );
        assert_eq!(
            classify_test_run(&process(0), &evidence),
            TestRunClassification::InfrastructureError
        );
        evidence.compile_or_load_failed = true;
        assert_eq!(
            classify_test_run(&process(1), &evidence),
            TestRunClassification::CompileOrLoadFailure
        );
    }
    #[test]
    fn baseline_infrastructure_and_termination_are_separate() {
        let evidence = TestEvidence {
            machine_session_complete: true,
            completed_tests: 1,
            assertion_failures: 1,
            ..Default::default()
        };
        assert_eq!(
            classify_test_run(&process(1), &evidence),
            TestRunClassification::BaselineUnavailable
        );
        let mut result = process(0);
        result.cause = CompletionCause::TimedOut;
        assert_eq!(
            classify_test_run(&result, &evidence),
            TestRunClassification::TimedOut
        );
        result.cause = CompletionCause::Cancelled(CancelReason::User);
        assert_eq!(
            classify_test_run(&result, &evidence),
            TestRunClassification::Cancelled
        );
        result.cleanup.child_reaped = false;
        assert_eq!(
            classify_test_run(&result, &evidence),
            TestRunClassification::InfrastructureError
        );
        result = process(1);
        #[cfg(unix)]
        {
            result.status = Some(std::process::ExitStatus::from_raw(9));
        }
        #[cfg(windows)]
        {
            result.status = Some(std::process::ExitStatus::from_raw(0xc0000005));
        }
        assert_eq!(
            classify_test_run(&result, &evidence),
            TestRunClassification::InfrastructureError
        );
        let inconsistent = TestEvidence {
            baseline_succeeded: true,
            machine_session_complete: true,
            completed_tests: 1,
            assertion_failures: 2,
            compile_or_load_failed: false,
        };
        assert_eq!(
            classify_test_run(&process(1), &inconsistent),
            TestRunClassification::InfrastructureError
        );
    }
}
