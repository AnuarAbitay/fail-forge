use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::process::{Command, Stdio};

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let Some(mode) = args.first().and_then(|value| value.to_str()) else {
        return usage_error();
    };
    if mode == "exit" {
        return match parse_args(
            args.first().cloned(),
            args.get(1).cloned(),
            args.get(2).cloned(),
        ) {
            Some(number) => ExitCode::from(number),
            None => usage_error(),
        };
    }
    match run_mode(mode, &args[1..]) {
        Ok(code) => ExitCode::from(code),
        Err(_) => ExitCode::from(3),
    }
}

fn run_mode(mode: &str, args: &[OsString]) -> io::Result<u8> {
    match mode {
        "bytes" if args.len() == 2 => {
            let stdout = parse_count(&args[0])?;
            let stderr = parse_count(&args[1])?;
            install_bytes_watchdog()?;
            let thread = std::thread::spawn(move || write_pattern(io::stderr(), stderr));
            let result = write_pattern(io::stdout(), stdout);
            let other = thread
                .join()
                .map_err(|_| io::Error::other("writer panic"))?;
            result?;
            other?;
            Ok(0)
        }
        "raw" if args.len() == 2 => {
            // Validate both native arguments before opening the control socket
            // or producing output. Hex is ASCII; no UTF-8 conversion is needed.
            let stdout = decode_hex(&args[0])?;
            let stderr = decode_hex(&args[1])?;
            install_bytes_watchdog()?;
            let thread = std::thread::spawn(move || write_raw(io::stderr(), &stderr));
            let result = write_raw(io::stdout(), &stdout);
            let other = thread
                .join()
                .map_err(|_| io::Error::other("writer panic"))?;
            result?;
            other?;
            Ok(0)
        }
        "argv" => {
            for arg in args {
                writeln!(io::stdout(), "{}", hex(arg.as_encoded_bytes()))?;
            }
            Ok(0)
        }
        "env" if args.len() == 1 => {
            match std::env::var_os(&args[0]) {
                Some(value) => writeln!(io::stdout(), "{}", hex(value.as_encoded_bytes()))?,
                None => writeln!(io::stdout(), "<unset>")?,
            }
            Ok(0)
        }
        "cwd" if args.is_empty() => {
            writeln!(
                io::stdout(),
                "{}",
                hex(std::env::current_dir()?.as_os_str().as_encoded_bytes())
            )?;
            Ok(0)
        }
        "stdin-eof" if args.is_empty() => {
            let mut byte = [0];
            if io::stdin().read(&mut byte)? == 0 {
                Ok(0)
            } else {
                Ok(4)
            }
        }
        "wait" | "ignore" | "escape" | "graceful" | "move-group" if args.is_empty() => {
            if mode == "ignore" {
                ignore_signals();
            }
            if mode == "graceful" {
                install_graceful();
            }
            if mode == "escape" {
                // SAFETY: setsid has no pointer arguments and changes only this fixture process.
                if unsafe { libc::setsid() } == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if mode == "move-group" {
                // SAFETY: these integer-only calls move this fixture into its
                // parent's existing group; the controller never signals that
                // group and owns this fixture's separate READY connection.
                let group = unsafe { libc::getpgid(libc::getppid()) };
                if group == -1 || unsafe { libc::setpgid(0, group) } == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            controlled_wait(if mode == "escape" {
                Some("ESCAPED")
            } else {
                None
            })
        }
        "tree-pipe" | "tree-closed" | "tree-wait" | "tree-escape" if args.is_empty() => {
            let mut child = Command::new(std::env::current_exe()?);
            child.arg(if mode == "tree-escape" {
                "escape"
            } else {
                "ignore"
            });
            if mode == "tree-closed" {
                child
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
            }
            let _descendant = child.spawn()?;
            // Both processes report READY through separate control connections.
            // The parent leaves only after the test has observed the descendant.
            controlled_wait(if mode == "tree-escape" {
                Some("PARENT")
            } else {
                None
            })
        }
        _ => {
            let _ = usage_error();
            Ok(2)
        }
    }
}

fn ignore_signals() {
    // SAFETY: SIG_IGN is a defined signal disposition; no Rust callback is
    // installed. These changes are confined to the helper process.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
}

fn install_bytes_watchdog() -> io::Result<()> {
    let Some(path) = std::env::var_os("FAILFORGE_CONTROL") else {
        // Finite standalone/benchmark output needs no control endpoint.
        return Ok(());
    };
    let mut stream = UnixStream::connect(std::path::Path::new(&path))?;
    // SAFETY: getpgrp has no arguments and queries only this fixture's group.
    let pgid = unsafe { libc::getpgrp() };
    // The endpoint exists and READY is sent before either output writer begins.
    writeln!(stream, "READY {} {pgid}", std::process::id())?;
    std::thread::Builder::new()
        .name("fixture-byte-watchdog".into())
        .spawn(move || {
            let mut reader = io::BufReader::new(stream);
            let mut command = Vec::with_capacity(64);
            loop {
                command.clear();
                let result = Read::by_ref(&mut reader)
                    .take(64)
                    .read_until(b'\n', &mut command);
                let stop = match result {
                    Ok(0) => true,
                    Ok(_) => watchdog_requests_exit(&command) || command.last() != Some(&b'\n'),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => false,
                    Err(_) => true,
                };
                if stop {
                    // SAFETY: this is the test fixture's independent emergency
                    // exit, never production runner cleanup. _exit terminates
                    // its own process without acquiring Rust stdout locks or
                    // flushing writers that may be blocked in a full pipe.
                    unsafe { libc::_exit(0) };
                }
            }
        })?;
    // Do not join the watcher: finite writers must also finish naturally while
    // a benchmark listener keeps an unaccepted connection open.
    Ok(())
}

fn watchdog_requests_exit(command: &[u8]) -> bool {
    let mut words = command
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty());
    words.next() == Some(b"exit".as_slice())
        && words.next() == Some(b"0".as_slice())
        && words.next().is_none()
}

fn controlled_wait(role: Option<&str>) -> io::Result<u8> {
    let path = std::env::var_os("FAILFORGE_CONTROL")
        .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
    let mut stream = UnixStream::connect(std::path::Path::new(&path))?;
    stream.set_read_timeout(Some(std::time::Duration::from_millis(20)))?;
    // SAFETY: getpgrp has no arguments and returns this process's group ID.
    let pgid = unsafe { libc::getpgrp() };
    match role {
        Some(role) => writeln!(stream, "READY {} {pgid} {role}", std::process::id())?,
        None => writeln!(stream, "READY {} {pgid}", std::process::id())?,
    }
    let mut reader = io::BufReader::new(stream);
    let mut line = String::new();
    loop {
        if GRACEFUL.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(0);
        }
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(0),
            Ok(_) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    if line.trim() == "release" || line.trim() == "leave" {
        return Ok(0);
    }
    if let Some(code) = line.trim().strip_prefix("exit ") {
        return code
            .parse()
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput));
    }
    Err(io::Error::from(io::ErrorKind::InvalidInput))
}

static GRACEFUL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
extern "C" fn graceful_signal(_: libc::c_int) {
    GRACEFUL.store(true, std::sync::atomic::Ordering::Relaxed);
}
fn install_graceful() {
    // SAFETY: the handler uses only a lock-free atomic store, performs no
    // allocation or I/O, and its function address has static lifetime.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            graceful_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            graceful_signal as *const () as libc::sighandler_t,
        );
    }
}

fn parse_count(value: &OsStr) -> io::Result<usize> {
    value
        .to_str()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
}

fn write_pattern(mut writer: impl Write, mut count: usize) -> io::Result<()> {
    let pattern = [0, 255, b'a', 254, b'\n'];
    let block: Vec<_> = (0..16_385)
        .map(|index| pattern[index % pattern.len()])
        .collect();
    while count > 0 {
        let size = count.min(block.len());
        writer.write_all(&block[..size])?;
        count -= size;
    }
    writer.flush()
}

fn decode_hex(value: &OsStr) -> io::Result<Vec<u8>> {
    let bytes = value.as_encoded_bytes();
    if bytes.len() % 2 != 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let mut byte = 0;
        for &digit in pair {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                b'A'..=b'F' => digit - b'A' + 10,
                _ => return Err(io::ErrorKind::InvalidInput.into()),
            };
            byte = (byte << 4) | nibble;
        }
        decoded.push(byte);
    }
    Ok(decoded)
}

fn write_raw(mut writer: impl Write, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

fn parse_args(
    mode: Option<OsString>,
    code: Option<OsString>,
    extra: Option<OsString>,
) -> Option<u8> {
    match (mode, code, extra) {
        (Some(mode_value), Some(code_value), None) => {
            if mode_value.as_os_str() != OsStr::new("exit") {
                return None;
            }

            let text = code_value.to_str()?;
            text.parse::<u8>().ok()
        }
        _ => None,
    }
}

fn usage_error() -> ExitCode {
    eprintln!(
        "usage: failforge-process-fixture exit CODE | bytes STDOUT_BYTES STDERR_BYTES | raw STDOUT_HEX STDERR_HEX | argv ... | env KEY | cwd | stdin-eof | wait | ignore | escape | graceful | move-group | tree-pipe | tree-closed | tree-wait | tree-escape"
    );
    ExitCode::from(2)
}

#[cfg(test)]
#[path = "process_fixture_tests.rs"]
mod tests;
