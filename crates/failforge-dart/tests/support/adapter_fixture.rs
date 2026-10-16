//! Native protocol fixture. It has no subprocesses and executes no JSON command.
use failforge_dart::{AdapterFailure, RequestEnvelope, ResponseEnvelope, ResponseKind};
use serde_json::json;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).ok_or("fixture mode required")?;
    if mode == "hang-ignore-term" {
        // SAFETY: this dedicated fixture process changes only its own SIGTERM
        // disposition before notifying the test that it is ready.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    let mut control = if let Some(path) = std::env::var_os("FAILFORGE_ADAPTER_CONTROL") {
        let mut stream = UnixStream::connect(path)?;
        // SAFETY: getpgrp reads this fixture's own process group without mutation.
        writeln!(stream, "READY {} {}", std::process::id(), unsafe {
            libc::getpgrp()
        })?;
        let mut watchdog = stream.try_clone()?;
        std::thread::spawn(move || {
            let mut byte = [0];
            loop {
                match watchdog.read(&mut byte) {
                    Ok(0) => std::process::exit(0),
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => std::process::exit(0),
                }
            }
        });
        Some(stream)
    } else {
        None
    };
    let mut reader = io::BufReader::new(io::stdin().lock());
    let mut stdout = io::stdout().lock();
    let mut sequence = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            if mode == "extra-on-close" {
                stdout.write_all(b"{}\n")?;
                stdout.flush()?;
            }
            return Ok(());
        }
        let request: RequestEnvelope = serde_json::from_str(&line)?;
        request.validate()?;
        if let Some(control) = &mut control {
            writeln!(control, "REQUEST {}", request.request_id)?;
        }
        sequence += 1;
        if matches!(
            mode.as_str(),
            "hang" | "hang-until-signal" | "hang-ignore-term"
        ) {
            let mut ignored = String::new();
            reader.read_line(&mut ignored)?;
            if mode != "hang" {
                // EOF alone is not completion in these cancellation fixtures.
                // A finite fallback keeps a broken RED run self-terminating.
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            return Ok(());
        }
        if mode == "eof" {
            return Ok(());
        }
        if mode == "malformed" {
            stdout.write_all(b"not-json\n")?;
            stdout.flush()?;
            continue;
        }
        if mode == "oversize" {
            stdout.write_all(&vec![b'x'; 1025])?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
            continue;
        }
        let kind = match request.command {
            failforge_dart::AdapterCommand::Handshake => ResponseKind::Handshake,
            failforge_dart::AdapterCommand::Discover => ResponseKind::Discover,
            failforge_dart::AdapterCommand::Validate => ResponseKind::Validate,
        };
        let mut response = ResponseEnvelope {
            protocol_version: if mode == "wrong-version" { 2 } else { 1 },
            request_id: if mode == "wrong-id" {
                "wrong".into()
            } else {
                request.request_id
            },
            kind,
            payload: Some(json!({"sequence":sequence,"command":request.command.as_str()})),
            error: None,
        };
        if request.payload.get("error") == Some(&json!(true)) {
            response.kind = ResponseKind::Error;
            response.payload = None;
            response.error = Some(AdapterFailure {
                code: "test_error".into(),
                message: "expected structured error".into(),
                details: None,
            });
        }
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
}
