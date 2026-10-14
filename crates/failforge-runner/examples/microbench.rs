//! A small measurement client of the production Runner, not another backend.
//! The Python harness supplies an external process deadline and aggregates CSV.
use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use failforge_runner::{
    CancelReason, CapturePolicy, CommandSpec, CompletionCause, EnvSpec, Runner, TimeoutPolicy,
};
use tokio::net::{UnixListener, UnixStream};

const DEADLINE: Duration = Duration::from_secs(10);
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Config {
    fixture: PathBuf,
    iterations: usize,
    warmup: usize,
    bytes: usize,
}

impl Config {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, &'static str> {
        let mut fixture = None;
        let mut iterations = 20;
        let mut warmup = 3;
        let mut bytes = 1024 * 1024;
        let mut arguments = arguments.into_iter();
        while let Some(option) = arguments.next() {
            let value = arguments
                .next()
                .ok_or("a benchmark option requires a value")?;
            match option.to_str() {
                Some("--fixture") => fixture = Some(PathBuf::from(value)),
                Some("--iterations") => {
                    iterations = parse_count(&value)?;
                    if iterations == 0 {
                        return Err("iterations must be positive");
                    }
                }
                Some("--warmup") => warmup = parse_count(&value)?,
                Some("--bytes") => bytes = parse_count(&value)?,
                _ => return Err("unknown benchmark option"),
            }
        }
        Ok(Self {
            fixture: fixture.ok_or("--fixture is required")?,
            iterations,
            warmup,
            bytes,
        })
    }
}

fn parse_count(value: &OsString) -> Result<usize, &'static str> {
    let text = value.to_str().ok_or("counts must be decimal integers")?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("counts must be unsigned decimal integers");
    }
    text.parse().map_err(|_| "count is out of range")
}

fn main() -> ExitCode {
    let config = match Config::parse(std::env::args_os().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("microbench: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("microbench: runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(measure(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("microbench: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn measure(config: Config) -> io::Result<()> {
    let runner = Runner::new();
    println!(
        "scenario,iteration,total_ns,cancel_cleanup_ns,preparation_ns,spawn_ns,execution_ns,cleanup_ns,output_drain_ns,stdout_bytes,stderr_bytes"
    );
    let mut outcome = Ok(());
    'cases: for scenario in ["exit", "bytes", "cancel"] {
        for _ in 0..config.warmup {
            if let Err(error) = sample(&runner, &config, scenario).await {
                outcome = Err(error);
                break 'cases;
            }
        }
        for iteration in 0..config.iterations {
            match sample(&runner, &config, scenario).await {
                Ok(row) => println!("{scenario},{iteration},{row}"),
                Err(error) => {
                    outcome = Err(error);
                    break 'cases;
                }
            }
        }
    }
    // An error never bypasses explicit shutdown while the runtime still exists.
    let shutdown = runner
        .shutdown()
        .await
        .map_err(|error| io::Error::other(error.to_string()));
    outcome.and(shutdown)
}

async fn sample(runner: &Runner, config: &Config, scenario: &str) -> io::Result<String> {
    let mut control = if scenario == "cancel" {
        Some(Control::new()?)
    } else {
        None
    };
    let mut spec = CommandSpec {
        program: config.fixture.as_os_str().to_os_string(),
        args: match scenario {
            "exit" => ["exit", "0"].map(OsString::from).to_vec(),
            "bytes" => vec![
                "bytes".into(),
                config.bytes.to_string().into(),
                config.bytes.to_string().into(),
            ],
            "cancel" => vec!["wait".into()],
            _ => return Err(io::Error::from(io::ErrorKind::InvalidInput)),
        },
        cwd: std::env::current_dir()?,
        env: EnvSpec::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(DEADLINE),
            ..TimeoutPolicy::default()
        },
        capture_policy: CapturePolicy::default(),
    };
    if let Some(control) = &control {
        spec.env.set.insert(
            "FAILFORGE_CONTROL".into(),
            control.path.as_os_str().to_os_string(),
        );
    } else {
        spec.env.remove.insert("FAILFORGE_CONTROL".into());
    }
    let before = Instant::now();
    let mut handle = runner
        .start(spec)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let cancel_start = if let Some(control) = &mut control {
        if let Err(error) = tokio::time::timeout(DEADLINE, control.ready())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "helper READY deadline"))
            .and_then(|result| result)
        {
            handle.force_cancel(CancelReason::User);
            drop(control.connection.take());
            return Err(error);
        }
        let now = Instant::now();
        handle.cancel(CancelReason::User);
        Some(now)
    } else {
        None
    };
    let result = match tokio::time::timeout(DEADLINE, handle.wait()).await {
        Ok(outcome) => outcome.map_err(|error| io::Error::other(error.to_string()))?,
        Err(_) => {
            handle.force_cancel(CancelReason::User);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "run result deadline",
            ));
        }
    };
    let total = before.elapsed().as_nanos();
    let cancel_cleanup = cancel_start.map(|start| start.elapsed().as_nanos());
    if !result.cleanup.is_complete() {
        return Err(io::Error::other("benchmark cleanup incomplete"));
    }
    if scenario == "cancel" {
        if result.cause != CompletionCause::Cancelled(CancelReason::User) {
            return Err(io::Error::other("benchmark cancellation cause mismatch"));
        }
        if !result
            .status
            .is_some_and(|status| status.signal() == Some(libc::SIGTERM))
        {
            return Err(io::Error::other(
                "benchmark cancellation child status mismatch",
            ));
        }
    } else if result.cause != CompletionCause::ProcessCompleted
        || !result.status.is_some_and(|status| status.success())
    {
        return Err(io::Error::other("benchmark child exit mismatch"));
    }
    let expected = if scenario == "bytes" {
        config.bytes as u64
    } else {
        0
    };
    if result.stdout.bytes_read != expected || result.stderr.bytes_read != expected {
        return Err(io::Error::other("benchmark capture byte count mismatch"));
    }
    Ok(format!(
        "{total},{},{},{},{},{},{},{},{}",
        cancel_cleanup
            .map(|value| value.to_string())
            .unwrap_or_default(),
        result.timings.preparation.as_nanos(),
        result.timings.spawn.as_nanos(),
        result.timings.execution.as_nanos(),
        result.timings.cleanup.as_nanos(),
        result.timings.output_drain.as_nanos(),
        result.stdout.bytes_read,
        result.stderr.bytes_read
    ))
}

/// Holding the connection keeps the fixture waiting. Dropping it provides
/// independent EOF cleanup if the measurement assertion or runner fails.
struct Control {
    directory: PathBuf,
    path: PathBuf,
    listener: UnixListener,
    connection: Option<UnixStream>,
}

impl Control {
    fn new() -> io::Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "ff-live-bench-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory)?;
        let path = directory.join("control.sock");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        Ok(Self {
            directory,
            path,
            listener,
            connection: None,
        })
    }

    async fn ready(&mut self) -> io::Result<()> {
        let (stream, _) = self.listener.accept().await?;
        // Store ownership before any await/read: cancellation closes the socket.
        self.connection = Some(stream);
        let stream = self.connection.as_ref().expect("connection just stored");
        let mut line = Vec::new();
        loop {
            stream.readable().await?;
            let mut block = [0; 128];
            match stream.try_read(&mut block) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "helper closed before READY",
                    ));
                }
                Ok(count) => {
                    line.extend_from_slice(&block[..count]);
                    if line.len() > 256 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "oversized READY event",
                        ));
                    }
                    if line.contains(&b'\n') {
                        return validate_ready(&line);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        drop(self.connection.take());
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn validate_ready(bytes: &[u8]) -> io::Result<()> {
    let line =
        std::str::from_utf8(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let fields: Vec<_> = line.split_whitespace().collect();
    let valid = fields.len() == 3
        && fields[0] == "READY"
        && fields[1].parse::<u32>().is_ok_and(|pid| pid != 0)
        && fields[1] == fields[2];
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid READY event",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn defaults_and_overrides_are_parsed() {
        let default = Config::parse(["--fixture", "/helper"].map(OsString::from)).unwrap();
        assert_eq!(default.iterations, 20);
        assert_eq!(default.warmup, 3);
        assert_eq!(default.bytes, 1024 * 1024);
        let config = Config::parse(
            [
                "--fixture",
                "/helper",
                "--iterations",
                "5",
                "--warmup",
                "0",
                "--bytes",
                "0",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert_eq!((config.iterations, config.warmup, config.bytes), (5, 0, 0));
    }

    #[test]
    fn invalid_configuration_is_rejected() {
        for args in [
            vec![],
            vec!["--fixture"],
            vec!["--fixture", "/helper", "--iterations", "0"],
            vec!["--fixture", "/helper", "--unknown", "x"],
        ] {
            assert!(Config::parse(args.into_iter().map(OsString::from)).is_err());
        }
        for value in ["", "-1", "+1", "1.5", " 1", "184467440737095516160"] {
            assert!(parse_count(&value.into()).is_err());
        }
        assert!(parse_count(&OsString::from_vec(vec![255])).is_err());
    }

    #[test]
    fn native_fixture_path_is_preserved() {
        let fixture = OsString::from_vec(b"/helper-\xff".to_vec());
        let config = Config::parse([OsString::from("--fixture"), fixture.clone()]).unwrap();
        assert_eq!(config.fixture.into_os_string(), fixture);
    }

    #[test]
    fn ready_events_validate_identity_and_group_without_lossy_decoding() {
        assert!(validate_ready(b"READY 37 37\n").is_ok());
        for bytes in [
            b"".as_slice(),
            b"READY 0 0\n",
            b"READY 37 38\n",
            b"READY abc abc\n",
            b"READY 37 37 extra\n",
            b"READY \xff \xff\n",
        ] {
            assert!(validate_ready(bytes).is_err());
        }
    }
}
