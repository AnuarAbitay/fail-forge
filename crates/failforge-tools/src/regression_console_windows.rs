//! Real console events stay inside a separately owned console and Job Object.
use super::*;
use std::io::{self, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

const CLEANUP_BUDGET: Duration = Duration::from_secs(30);

fn ignored_control_summary(summary: &Value) -> Result<()> {
    ensure(
        summary["status"] == "running"
            && summary["complete"] == false
            && summary["completedMutants"] == 0
            && summary["records"].as_array().is_some_and(Vec::is_empty)
            && ["mutationScore", "failureModeScore", "languageSpecificScore"]
                .iter()
                .all(|key| summary.get(key) == Some(&Value::Null)),
        "ignored Ctrl+C control lacks an unfinished campaign without results or scores",
    )
}

struct ConsoleHost {
    child: Child,
    job: OwnedHandle,
    assigned: bool,
    cleaned: bool,
    cleanup_attempted: bool,
}

impl ConsoleHost {
    fn start(command: &mut Command) -> Result<Self> {
        // The host waits for our start byte before spawning any target. Assign
        // its retained process handle to the job before releasing that gate.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        ensure(!raw.is_null(), "cannot create console fixture job")?;
        // SAFETY: CreateJobObjectW transferred this unique owned handle.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let child = command
            .creation_flags(CREATE_NEW_CONSOLE)
            .stdin(Stdio::piped())
            .spawn()?;
        let mut owner = Self {
            child,
            job,
            assigned: false,
            cleaned: false,
            cleanup_attempted: false,
        };
        if unsafe {
            AssignProcessToJobObject(owner.job.as_raw_handle(), owner.child.as_raw_handle())
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        owner.assigned = true;
        owner.send(b'R')?;
        Ok(owner)
    }

    fn send(&mut self, byte: u8) -> Result<()> {
        self.child
            .stdin
            .as_mut()
            .ok_or("console host control input is absent")?
            .write_all(&[byte])?;
        Ok(())
    }

    fn wait(&mut self, budget: Duration) -> Result<ExitStatus> {
        let start = Instant::now();
        loop {
            common::check_interrupt()?;
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            ensure(
                start.elapsed() < budget,
                "console host exit deadline exceeded",
            )?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn job_empty(&self) -> Result<bool> {
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.job.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&raw mut accounting).cast(),
                std::mem::size_of_val(&accounting) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(accounting.ActiveProcesses == 0)
    }

    fn require_drained(&mut self) -> Result<()> {
        let start = Instant::now();
        loop {
            common::check_interrupt()?;
            if self.child.try_wait()?.is_some() && self.job_empty()? {
                self.cleaned = true;
                return Ok(());
            }
            ensure(
                start.elapsed() < Duration::from_secs(10),
                "console target left live descendants before fixture cleanup",
            )?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleanup_attempted = true;
        if self.assigned {
            // This job and every process handle were created for this case.
            // No PID lookup or external process is used as kill authority.
            if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error().into());
            }
        } else if self.child.try_wait()?.is_none() {
            self.child.kill()?;
        }
        let start = Instant::now();
        loop {
            let exited = self.child.try_wait()?.is_some();
            let empty = if self.assigned {
                self.job_empty()?
            } else {
                exited
            };
            if exited && empty {
                self.cleaned = true;
                return Ok(());
            }
            ensure(
                start.elapsed() < CLEANUP_BUDGET,
                "console fixture job did not drain before its cleanup deadline",
            )?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ConsoleHost {
    fn drop(&mut self) {
        if !self.cleanup_attempted {
            let _ = self.cleanup();
        }
    }
}

impl Harness {
    pub(super) fn console_cancellation(&mut self) -> Result<()> {
        let project = fixture(
            &self.root,
            &self.output.join("console-cancel-subject"),
            "bool keepGoing() => false;\n",
            LOOP_TEST,
        )?;
        for (mode, name, expected) in [
            ("ctrl-c", "windows-console-ctrl-c", Some(130)),
            ("ctrl-break", "windows-console-ctrl-break", Some(143)),
            ("ctrl-c-ignored", "windows-console-ctrl-c-ignored", None),
        ] {
            self.console_case(&project, mode, name, expected)?;
        }
        Ok(())
    }

    fn console_case(
        &mut self,
        project: &Path,
        mode: &str,
        name: &str,
        expected: Option<i32>,
    ) -> Result<()> {
        let before = common::tree(project)?;
        let temporary = common::Temporary::new("ff-console")?;
        let destination = self.output.join(name);
        let logs = self.output.join(format!("{name}-command"));
        fs::create_dir(&logs)?;
        let mut target = self.command();
        target
            .args(["mutate", "--installation"])
            .arg(&self.root)
            .arg("--project")
            .arg(project)
            .arg("--dart-sdk")
            .arg(&self.sdk)
            .args([
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--test-timeout",
                "30s",
                "--command-timeout",
                "60s",
            ])
            .arg("--output")
            .arg(&destination)
            .env("TMPDIR", &temporary.0)
            .env("TMP", &temporary.0)
            .env("TEMP", &temporary.0);
        let example = self
            .binary_dir
            .join("examples")
            .join(failforge_config::platform::binary_name("windows_console_host").as_ref());
        ensure(
            example.is_file(),
            "build the windows_console_host developer example",
        )?;
        let mut host_command = common::command(&example);
        host_command
            .arg(mode)
            .arg(&logs)
            .arg(std::process::id().to_string())
            .arg(target.get_program())
            .args(target.get_args())
            .stdout(File::create(logs.join("host-stdout.log"))?)
            .stderr(File::create(logs.join("host-stderr.log"))?);
        if let Some(cwd) = target.get_current_dir() {
            host_command.current_dir(cwd);
        }
        for (key, value) in target.get_envs() {
            if let Some(value) = value {
                host_command.env(key, value);
            } else {
                host_command.env_remove(key);
            }
        }
        // A target that accidentally inherits this nonexistent capability fails
        // admission. No private pipe exists through which cancellation can pass.
        host_command.env(
            "FAILFORGE_CLI_CANCEL_PIPE",
            format!(
                r"\\.\pipe\failforge-console-poison-{}",
                failforge_workspace::windows_fs::random_token()?
            ),
        );
        println!("regression {name}");
        let start = Instant::now();
        let mut host = ConsoleHost::start(&mut host_command)?;
        let result = (|| -> Result<()> {
            while !logs.join("host-ready.json").exists() || !author_started(&destination, "loop")? {
                common::check_interrupt()?;
                ensure(
                    host.child.try_wait()?.is_none() && start.elapsed() < Duration::from_secs(120),
                    format!(
                        "{name}: target did not start the mutant author test; see {}",
                        logs.display()
                    ),
                )?;
                std::thread::sleep(Duration::from_millis(25));
            }
            let ready = common::read_json(&logs.join("host-ready.json"))?;
            ensure(
                ready["isolatedConsole"] == true
                    && ready["poisonedPipeInherited"] == true
                    && ready["targetPipeRemoved"] == true,
                "console target isolation or cancellation ingress is unproven",
            )?;
            host.send(b'E')?;
            let status = host.wait(Duration::from_secs(45))?;
            ensure(
                status.success(),
                "console host failed; inspect host-stderr.log",
            )?;
            let receipt = common::read_json(&logs.join("host-result.json"))?;
            ensure(
                receipt["api"] == "GenerateConsoleCtrlEvent"
                    && receipt["group"] == 0
                    && receipt["hostObservedEvent"] == true
                    && receipt["mode"] == mode,
                "real console event was not observed inside its isolated host",
            )?;
            ensure(
                common::tree(project)? == before,
                "console cancellation changed source inputs",
            )?;
            if let Some(expected) = expected {
                // A passing report cannot hide leaked descendants that the
                // fixture's fallback TerminateJobObject would otherwise kill.
                host.require_drained()?;
                ensure(
                    receipt["targetExitCode"] == expected,
                    "console cancellation exit mismatch",
                )?;
                let report = self.report(name, &destination, &logs.join("stdout.log"), receipt)?;
                expect_counts(&report, json!({"CANCELLED":1}))?;
                ensure(
                    report["status"] == "cancelled"
                        && report["complete"] == false
                        && report["records"][0]["runner"]["cleanup"] == "complete",
                    "console cancellation lost terminal or cleanup evidence",
                )?;
            } else {
                ensure(
                    receipt["targetSurvivedIgnoredCtrlC"] == true,
                    "ignored Ctrl+C negative control unexpectedly cancelled",
                )?;
                // Campaigns publish their initial running summary before the
                // author-start barrier. Its presence is expected; a terminal
                // report or a mutation result would contradict this control.
                ignored_control_summary(&common::read_json(&destination.join("summary.json"))?)?;
                self.reports.push(
                    json!({"name":name,"receipt":receipt,"ignoredConsoleEventDidNotCancel":true}),
                );
            }
            Ok(())
        })();
        // Always drain before the private temporary tree's RAII removal, even
        // for a failed barrier or the intentionally non-cancelling control.
        let cleaned = host.cleanup();
        if cleaned.is_err() {
            eprintln!(
                "console job cleanup unconfirmed; retaining {}",
                temporary.0.display()
            );
            std::mem::forget(temporary);
        }
        match (result, cleaned) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => {
                Err(format!("{error}; console cleanup: {cleanup}").into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_control_accepts_initial_report_but_rejects_terminal_results_and_scores() {
        let running = json!({"status":"running","complete":false,
            "completedMutants":0,"records":[],"mutationScore":null,
            "failureModeScore":null,"languageSpecificScore":null});
        assert!(ignored_control_summary(&running).is_ok());
        for (key, value) in [
            ("status", json!("cancelled")),
            ("status", json!("completed")),
            ("complete", json!(true)),
            ("completedMutants", json!(1)),
            ("records", json!([{"outcome":"CANCELLED"}])),
            ("mutationScore", json!({"percent":100})),
            ("failureModeScore", json!({"percent":100})),
            ("languageSpecificScore", json!({"percent":100})),
        ] {
            let mut changed = running.clone();
            changed[key] = value;
            assert!(ignored_control_summary(&changed).is_err(), "{key}");
        }
        for key in ["mutationScore", "failureModeScore", "languageSpecificScore"] {
            let mut missing = running.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(ignored_control_summary(&missing).is_err(), "{key}");
        }
    }
}
