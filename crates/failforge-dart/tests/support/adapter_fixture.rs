//! Native protocol fixture. It has no subprocesses and executes no JSON command.
use failforge_dart::{AdapterFailure, RequestEnvelope, ResponseEnvelope, ResponseKind};
use serde_json::json;
#[cfg(windows)]
use std::fs::File as ControlStream;
#[cfg(unix)]
use std::io::Read;
use std::io::{self, BufRead, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream as ControlStream;

fn connect_control(path: &std::ffi::OsStr) -> io::Result<ControlStream> {
    #[cfg(unix)]
    {
        ControlStream::connect(path)
    }
    #[cfg(windows)]
    {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
    }
}

/// Finite protocol actor for the installed helper's cancellation boundary.
fn cancellation_fixture(marker: std::path::PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    let probe: i32 = std::env::var("FAILFORGE_CANCEL_PROBE_FD")?.parse()?;
    #[cfg(unix)]
    let expected: serde_json::Value =
        serde_json::from_str(&std::env::var("FAILFORGE_CANCEL_PROBE_INODE")?)?;
    // SAFETY: fstat initializes the supplied storage when it succeeds. An
    // intentionally closed probe descriptor returns EBADF without mutation.
    #[cfg(unix)]
    let inherited = unsafe {
        let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
        if libc::fstat(probe, info.as_mut_ptr()) == 0 {
            let info = info.assume_init();
            expected == json!([info.st_dev, info.st_ino])
        } else {
            false
        }
    };
    #[cfg(windows)]
    let inherited = false;
    let close = std::env::var("FAILFORGE_CANCEL_MODE").as_deref() == Ok("close");
    let report = |stage: &str| {
        std::fs::write(
            &marker,
            json!({"pid":std::process::id(),"inheritedControl":inherited,
                "privateEnvironmentPresent":std::env::var_os("FAILFORGE_ADAPTER_CANCEL_FD").is_some() || std::env::var_os("FAILFORGE_ADAPTER_CANCEL_PIPE").is_some(),
                "stage":stage})
            .to_string(),
        )
    };
    report("started")?;
    for line in io::stdin().lock().lines() {
        let request: serde_json::Value = serde_json::from_str(&line?)?;
        let command = request["command"].as_str().ok_or("missing command")?;
        let payload = if command == "handshake" {
            json!({"operators":[]})
        } else {
            report("discover")?;
            if !close {
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
            json!({"mutants":[],"truncated":false,"totalCandidates":0,"returnedCandidates":0})
        };
        println!(
            "{}",
            json!({"protocolVersion":1,"requestId":request["requestId"],
                "kind":command,"payload":payload})
        );
        io::stdout().flush()?;
    }
    if close {
        report("close")?;
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(marker) = std::env::var_os("FAILFORGE_CANCEL_MARKER") {
        return cancellation_fixture(marker.into());
    }
    let mode = std::env::args().nth(1).ok_or("fixture mode required")?;
    #[cfg(unix)]
    if mode == "hang-ignore-term" {
        // SAFETY: this dedicated fixture process changes only its own SIGTERM
        // disposition before notifying the test that it is ready.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    let mut control = if let Some(path) = std::env::var_os("FAILFORGE_ADAPTER_CONTROL") {
        let mut stream = connect_control(&path)?;
        // SAFETY: getpgrp reads this fixture's own process group without mutation.
        #[cfg(unix)]
        let group = unsafe { libc::getpgrp() };
        #[cfg(windows)]
        let group = std::process::id();
        writeln!(stream, "READY {} {}", std::process::id(), group)?;
        let watchdog = stream.try_clone()?;
        std::thread::spawn(move || {
            #[cfg(unix)]
            let mut watchdog = watchdog;
            #[cfg(unix)]
            let mut byte = [0];
            loop {
                #[cfg(unix)]
                match watchdog.read(&mut byte) {
                    Ok(0) => std::process::exit(0),
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => std::process::exit(0),
                }
                #[cfg(windows)]
                {
                    use std::os::windows::io::AsRawHandle;
                    // A blocking read on this synchronous pipe would serialize
                    // REQUEST writes on its cloned file object. Only probe EOF.
                    let connected = unsafe {
                        windows_sys::Win32::System::Pipes::PeekNamedPipe(
                            watchdog.as_raw_handle(),
                            std::ptr::null_mut(),
                            0,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    };
                    if connected == 0 {
                        std::process::exit(0);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
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
            if matches!(mode.as_str(), "hang-on-close" | "buffered-valid-response") {
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
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
        if mode == "buffered-valid-response" {
            let release = std::env::var_os("FAILFORGE_ADAPTER_RESPONSE_RELEASE")
                .ok_or("response release marker required")?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !std::path::Path::new(&release).is_file() {
                if std::time::Instant::now() >= deadline {
                    return Err("response release deadline".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
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
        if mode == "duplicate-nested" {
            writeln!(
                stdout,
                "{{\"protocolVersion\":1,\"requestId\":{},\"kind\":\"handshake\",\"payload\":{{\"valid\":true,\"valid\":false}}}}",
                serde_json::to_string(&request.request_id)?
            )?;
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
        if mode == "buffered-valid-response" {
            let case = std::env::var("FAILFORGE_ADAPTER_RESPONSE_CASE")
                .unwrap_or_else(|_| "valid".to_owned());
            match case.as_str() {
                "wrong-id" => response.request_id = "other".into(),
                "wrong-version" => response.protocol_version = 2,
                "wrong-kind" => response.kind = ResponseKind::Validate,
                "remote" => {
                    response.kind = ResponseKind::Error;
                    response.payload = None;
                    response.error = Some(AdapterFailure {
                        code: "test_error".into(),
                        message: "expected structured error".into(),
                        details: None,
                    });
                }
                _ => {}
            }
            let mut bytes = serde_json::to_vec(&response)?;
            match case.as_str() {
                "malformed" => bytes = b"not-json".to_vec(),
                "duplicate" => {
                    bytes = format!(
                        "{{\"protocolVersion\":1,\"requestId\":{},\"kind\":\"handshake\",\"payload\":{{\"valid\":true,\"valid\":false}}}}",
                        serde_json::to_string(&response.request_id)?
                    )
                    .into_bytes();
                }
                "oversize" => bytes = vec![b'x'; 513],
                "partial" => bytes.truncate(bytes.len() / 2),
                _ => {}
            }
            if case != "partial" {
                bytes.push(b'\n');
            }
            if case == "second-response" {
                bytes.extend(serde_json::to_vec(&response)?);
                bytes.push(b'\n');
            } else if case == "extra" {
                bytes.extend_from_slice(b"unexpected");
            }
            stdout.write_all(&bytes)?;
            stdout.flush()?;
            writeln!(
                control.as_mut().ok_or("response control required")?,
                "RESPONSE"
            )?;
            continue;
        }
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
}
