//! Developer-only console-event fixture. Never part of a runtime package.
#[cfg(not(windows))]
fn main() {
    eprintln!("windows_console_host requires native Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(error) = native::run() {
        eprintln!("windows console fixture: {error}");
        std::process::exit(2);
    }
}

#[cfg(windows)]
mod native {
    use serde_json::json;
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc::{self, Receiver};
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Console::*;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
    static EVENTS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn observe(event: u32) -> windows_sys::core::BOOL {
        if event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT {
            EVENTS.fetch_or(1 << event, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    fn require(condition: bool, message: &str) -> Result<()> {
        if condition {
            Ok(())
        } else {
            Err(message.into())
        }
    }

    fn console_processes() -> Result<Vec<u32>> {
        let mut processes = vec![0; 8];
        loop {
            let count =
                unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) }
                    as usize;
            if count == 0 {
                return Err(io::Error::last_os_error().into());
            }
            require(count <= 128, "unexpectedly large fixture console")?;
            if count > processes.len() {
                processes.resize(count, 0);
            } else {
                processes.truncate(count);
                return Ok(processes);
            }
        }
    }

    fn controls() -> Receiver<io::Result<u8>> {
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut input = io::stdin().lock();
            loop {
                let mut byte = [0];
                let read = input.read_exact(&mut byte).map(|()| byte[0]);
                let failed = read.is_err();
                if send.send(read).is_err() || failed {
                    break;
                }
            }
        });
        receive
    }

    fn receive(
        control: &Receiver<io::Result<u8>>,
        mut child: Option<&mut Child>,
        expected: u8,
        budget: Duration,
    ) -> Result<()> {
        let start = Instant::now();
        loop {
            if let Some(child) = child.as_mut() {
                require(
                    child.try_wait()?.is_none(),
                    "target exited before console event",
                )?;
            }
            require(
                start.elapsed() < budget,
                "console control deadline exceeded",
            )?;
            match control.recv_timeout(Duration::from_millis(25)) {
                Ok(byte) => return require(byte? == expected, "unexpected console control byte"),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn write_json(path: &Path, value: &serde_json::Value) -> Result<()> {
        let pending = path.with_extension("pending");
        fs::write(&pending, serde_json::to_vec_pretty(value)?)?;
        fs::rename(pending, path)?;
        Ok(())
    }

    pub(super) fn run() -> Result<()> {
        let mut args = std::env::args_os().skip(1);
        let mode = args.next().ok_or("missing console mode")?;
        let mode = mode.to_str().ok_or("invalid console mode")?;
        let (event, ignored) = match mode {
            "ctrl-c" => (CTRL_C_EVENT, false),
            "ctrl-break" => (CTRL_BREAK_EVENT, false),
            "ctrl-c-ignored" => (CTRL_C_EVENT, true),
            _ => return Err("unknown console mode".into()),
        };
        let logs = PathBuf::from(args.next().ok_or("missing fixture logs")?);
        let parent: u32 = args
            .next()
            .ok_or("missing controller ID")?
            .to_str()
            .ok_or("invalid controller ID")?
            .parse()?;
        let program = args.next().ok_or("missing target executable")?;
        let initial = console_processes()?;
        require(
            initial == [std::process::id()] && !initial.contains(&parent),
            "host is not alone in a newly isolated console",
        )?;
        // A non-null handler is process-local. Unlike the NULL ignore flag, it
        // cannot disable the child's handlers through console inheritance.
        if unsafe { SetConsoleCtrlHandler(None, 0) } == 0
            || unsafe { SetConsoleCtrlHandler(Some(observe), 1) } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let control = controls();
        receive(&control, None, b'R', Duration::from_secs(10))?;
        if ignored && unsafe { SetConsoleCtrlHandler(None, 1) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let poisoned_pipe = std::env::var_os("FAILFORGE_CLI_CANCEL_PIPE").is_some();
        let spawned = Command::new(program)
            .args(args)
            .env_remove("FAILFORGE_CLI_CANCEL_PIPE")
            // No CREATE_NEW_PROCESS_GROUP: it would disable Ctrl+C delivery.
            .stdin(Stdio::null())
            .stdout(File::create(logs.join("stdout.log"))?)
            .stderr(File::create(logs.join("stderr.log"))?)
            .spawn();
        // Only the negative control's target inherits ignore. Restore the host
        // immediately so its event receipt still proves real console delivery.
        if ignored && unsafe { SetConsoleCtrlHandler(None, 0) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut target = spawned?;
        write_json(
            &logs.join("host-ready.json"),
            &json!({"isolatedConsole":true,"hostPid":std::process::id(),
                "targetPid":target.id(),"initialConsoleProcesses":initial,
                "poisonedPipeInherited":poisoned_pipe,"targetPipeRemoved":true}),
        )?;
        receive(&control, Some(&mut target), b'E', Duration::from_secs(150))?;
        let members = console_processes()?;
        require(
            members.contains(&target.id()) && !members.contains(&parent),
            "target does not share the isolated host console",
        )?;
        // Ctrl+C cannot be targeted at a nonzero process-group ID. Broadcast
        // only after proving this is the separately created fixture console.
        if unsafe { GenerateConsoleCtrlEvent(event, 0) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let start = Instant::now();
        let target_status = loop {
            let status = target.try_wait()?;
            let observed = EVENTS.load(Ordering::SeqCst) & (1 << event) != 0;
            if ignored {
                require(status.is_none(), "ignored Ctrl+C still terminated target")?;
                if start.elapsed() >= Duration::from_secs(2) && observed {
                    break None;
                }
            } else if status.is_some() && observed {
                break status;
            }
            require(
                start.elapsed() < Duration::from_secs(30),
                "target console cancellation deadline exceeded",
            )?;
            std::thread::sleep(Duration::from_millis(10));
        };
        write_json(
            &logs.join("host-result.json"),
            &json!({"mode":mode,"api":"GenerateConsoleCtrlEvent","event":event,
                "group":0,"hostObservedEvent":true,"consoleProcesses":members,
                "targetExitCode":target_status.and_then(|status| status.code()),
                "targetSurvivedIgnoredCtrlC":ignored}),
        )?;
        // The parent retains the kill-on-close job until it has checked this
        // receipt. It drains the intentionally live negative-control target.
        Ok(())
    }
}
