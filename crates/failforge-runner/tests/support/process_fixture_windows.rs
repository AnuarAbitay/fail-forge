//! Native Windows fixture: no command shell and no production process owner.
use std::io::{self, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

pub(crate) fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(_) => ExitCode::from(3),
    }
}
fn run() -> io::Result<u8> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mode = args.first().and_then(|arg| arg.to_str()).unwrap_or("");
    match mode {
        "exit" => Ok(args
            .get(1)
            .and_then(|arg| arg.to_str())
            .and_then(|arg| arg.parse().ok())
            .unwrap_or(2)),
        "exit-dword" => {
            let code = args
                .get(1)
                .and_then(|arg| arg.to_str())
                .and_then(|arg| arg.parse::<u32>().ok())
                .ok_or(io::ErrorKind::InvalidInput)?;
            // SAFETY: this fixture exits only its own process with the exact
            // requested native status; no console event or cancellation occurs.
            unsafe { windows_sys::Win32::System::Threading::ExitProcess(code) }
        }
        "console-state" => {
            let (error, members) = console_members()?;
            let code_page = unsafe { windows_sys::Win32::System::Console::GetConsoleCP() };
            writeln!(io::stdout(), "console-pid {}", std::process::id())?;
            writeln!(io::stdout(), "console-code-page {code_page}")?;
            writeln!(io::stdout(), "console-query-error {error}")?;
            write_members("console-members", &members)?;
            Ok(0)
        }
        "console-owner" => {
            let (error, members) = console_members()?;
            if error != 0 || !members.contains(&std::process::id()) {
                return Err(io::Error::other(
                    "console-owner must observe itself in its attached console",
                ));
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let runner = failforge_runner::Runner::new();
                let spec = failforge_runner::CommandSpec {
                    program: std::env::current_exe()?.into(),
                    args: vec!["console-state".into()],
                    cwd: std::env::current_dir()?,
                    env: Default::default(),
                    timeout_policy: failforge_runner::TimeoutPolicy {
                        timeout: Some(Duration::from_secs(5)),
                        forced_wait: Duration::from_secs(5),
                        output_drain: Duration::from_secs(5),
                        ..Default::default()
                    },
                    capture_policy: Default::default(),
                };
                let outcome = runner.start(spec).map_err(io::Error::other)?.wait().await;
                let shutdown = runner.shutdown().await;
                let result = outcome.map_err(io::Error::other)?;
                shutdown.map_err(io::Error::other)?;
                if result.cause != failforge_runner::CompletionCause::ProcessCompleted
                    || !result.status.is_some_and(|status| status.success())
                    || !result.cleanup.is_complete()
                {
                    return Err(io::Error::other(
                        "console-state child did not finish cleanly",
                    ));
                }
                writeln!(io::stdout(), "owner-pid {}", std::process::id())?;
                write_members("owner-members", &members)?;
                io::stdout().write_all(&result.stdout.tail)?;
                Ok(0)
            })
        }
        "argv" => {
            for arg in &args[1..] {
                writeln!(io::stdout(), "{}", hex(arg.as_encoded_bytes()))?;
            }
            Ok(0)
        }
        "module-path" => {
            let module = std::env::current_exe()?;
            let argv0 = std::env::args_os().next().unwrap();
            writeln!(
                io::stdout(),
                "{}",
                hex(module.as_os_str().as_encoded_bytes())
            )?;
            writeln!(io::stdout(), "{}", hex(argv0.as_encoded_bytes()))?;
            Ok(0)
        }
        "env" if args.len() == 2 => {
            match std::env::var_os(&args[1]) {
                Some(value) => writeln!(io::stdout(), "{}", hex(value.as_encoded_bytes()))?,
                None => writeln!(io::stdout(), "<unset>")?,
            }
            Ok(0)
        }
        "env-override" => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let runner = failforge_runner::Runner::new();
                let mut env = failforge_runner::EnvSpec::default();
                env.set
                    .insert("FLUTTER_ROOT".into(), "controlled Flutter SDK".into());
                let spec = failforge_runner::CommandSpec {
                    program: std::env::current_exe()?.into(),
                    args: vec!["env".into(), "FLUTTER_ROOT".into()],
                    cwd: std::env::current_dir()?,
                    env,
                    timeout_policy: Default::default(),
                    capture_policy: Default::default(),
                };
                let result = runner
                    .start(spec)
                    .map_err(io::Error::other)?
                    .wait()
                    .await
                    .map_err(io::Error::other)?;
                runner.shutdown().await.map_err(io::Error::other)?;
                io::stdout().write_all(&result.stdout.tail)?;
                Ok(result.status.and_then(|status| status.code()).unwrap_or(3) as u8)
            })
        }
        "cwd" => {
            writeln!(
                io::stdout(),
                "{}",
                hex(std::env::current_dir()?.as_os_str().as_encoded_bytes())
            )?;
            Ok(0)
        }
        "cwd-paths" => {
            let cwd = std::env::current_dir()?;
            writeln!(io::stdout(), "{}", hex(cwd.as_os_str().as_encoded_bytes()))?;
            writeln!(
                io::stdout(),
                "{}",
                hex(cwd.canonicalize()?.as_os_str().as_encoded_bytes())
            )?;
            Ok(0)
        }
        "stdin-eof" => {
            let mut bytes = Vec::new();
            io::stdin().read_to_end(&mut bytes)?;
            io::stdout().write_all(&bytes)?;
            Ok(0)
        }
        "bytes" if args.len() == 3 => {
            let parse = |index: usize| {
                args[index]
                    .to_str()
                    .and_then(|text| text.parse::<usize>().ok())
                    .ok_or(io::ErrorKind::InvalidInput)
            };
            let stdout = parse(1)?;
            let stderr = parse(2)?;
            let other = std::thread::spawn(move || write_pattern(io::stderr(), stderr));
            write_pattern(io::stdout(), stdout)?;
            other
                .join()
                .map_err(|_| io::Error::other("writer panic"))??;
            Ok(0)
        }
        "wait" | "ignore" | "tree-wait" | "tree-exit" => {
            if mode.starts_with("tree-") {
                let child = Command::new(std::env::current_exe()?)
                    .arg("ignore")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?;
                writeln!(io::stdout(), "CHILD {}", child.id())?;
            }
            writeln!(io::stdout(), "READY {}", std::process::id())?;
            io::stdout().flush()?;
            if mode == "tree-exit" {
                return Ok(0);
            }
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        "lease-hold" => {
            let _lease =
                failforge_runner::WorkspaceLease::from_env()?.ok_or(io::ErrorKind::InvalidInput)?;
            writeln!(io::stdout(), "READY {}", std::process::id())?;
            io::stdout().flush()?;
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        _ => Ok(2),
    }
}

fn console_members() -> io::Result<(u32, Vec<u32>)> {
    use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, GetLastError};
    use windows_sys::Win32::System::Console::GetConsoleProcessList;

    let mut members = vec![0; 8];
    loop {
        let count = unsafe { GetConsoleProcessList(members.as_mut_ptr(), members.len() as u32) };
        if count == 0 {
            let error = unsafe { GetLastError() };
            return if error == ERROR_INVALID_HANDLE {
                Ok((error, Vec::new()))
            } else {
                Err(io::Error::from_raw_os_error(error as i32))
            };
        }
        if count > 128 {
            return Err(io::Error::other("unexpectedly large fixture console"));
        }
        if count as usize > members.len() {
            members.resize(count as usize, 0);
        } else {
            members.truncate(count as usize);
            return Ok((0, members));
        }
    }
}

fn write_members(label: &str, members: &[u32]) -> io::Result<()> {
    write!(io::stdout(), "{label}")?;
    for member in members {
        write!(io::stdout(), " {member}")?;
    }
    writeln!(io::stdout())
}

fn write_pattern(mut writer: impl Write, mut count: usize) -> io::Result<()> {
    let block = [b'x'; 8192];
    while count > 0 {
        let size = count.min(block.len());
        writer.write_all(&block[..size])?;
        count -= size;
    }
    writer.flush()
}
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::new();
    for byte in bytes {
        write!(result, "{byte:02x}").unwrap();
    }
    result
}
