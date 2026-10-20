use crate::common::{self, Result, Temporary, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FIELDS: &str = "entrypoint,scenario,iteration,total_ns,cancel_cleanup_ns,preparation_ns,spawn_ns,execution_ns,cleanup_ns,output_drain_ns,stdout_bytes,stderr_bytes";
const SCENARIOS: [&str; 3] = ["exit", "bytes", "cancel"];
#[derive(Args)]
pub struct Options {
    #[arg(long, default_value_t=20, value_parser=positive)]
    iterations: usize,
    #[arg(long, default_value_t = 3, value_parser = count_usize)]
    warmup: usize,
    #[arg(long, default_value_t = 1048576, value_parser = count)]
    bytes: u64,
    #[arg(long,default_value_t=10.0,value_parser=seconds)]
    deadline: f64,
    #[arg(long)]
    target_dir: Option<PathBuf>,
    #[arg(long)]
    output: PathBuf,
}
fn positive(value: &str) -> std::result::Result<usize, String> {
    count_usize(value)
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "expected a positive integer".to_owned())
}
fn count(value: &str) -> std::result::Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected an unsigned decimal integer".to_owned());
    }
    value
        .parse()
        .map_err(|_| "integer is out of range".to_owned())
}
fn count_usize(value: &str) -> std::result::Result<usize, String> {
    usize::try_from(count(value)?).map_err(|_| "integer is out of range".to_owned())
}
fn seconds(value: &str) -> std::result::Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 86400.0)
        .ok_or_else(|| "expected finite seconds in (0,86400]".to_owned())
}
fn statistics(samples: &[u128]) -> Result<Value> {
    ensure(!samples.is_empty(), "statistics require samples")?;
    let mut values = samples.to_vec();
    values.sort_unstable();
    let n = values.len();
    let median = if n % 2 == 1 {
        values[n / 2] as f64
    } else {
        values[n / 2 - 1] as f64 / 2.0 + values[n / 2] as f64 / 2.0
    };
    Ok(
        json!({"count":n,"median_ns":median,"p95_ns":values[(95*n).div_ceil(100)-1],"min_ns":values[0],"max_ns":values[n-1]}),
    )
}
fn integer(value: &str, max: u128) -> Result<u128> {
    ensure(
        !value.is_empty()
            && (value == "0" || !value.starts_with('0'))
            && value.bytes().all(|b| b.is_ascii_digit()),
        "noncanonical summary integer",
    )?;
    let n = value.parse::<u128>()?;
    ensure(n <= max, "summary integer overflow")?;
    Ok(n)
}
pub fn summary(raw: &[u8]) -> Result<BTreeMap<String, String>> {
    Ok(failforge_dart::parse_cli_summary(raw)?)
}
fn validate(scenario: &str, amount: u64, code: i32, stdout: &[u8], stderr: &[u8]) -> Result<()> {
    ensure(SCENARIOS.contains(&scenario), "unknown scenario")?;
    ensure(
        code == if scenario == "cancel" { 143 } else { 0 },
        "unexpected benchmark exit",
    )?;
    ensure(stdout.is_empty(), "capture-only CLI displayed stdout")?;
    let values = summary(stderr)?;
    for (key, expected) in [
        (
            "cause",
            if scenario == "cancel" {
                "Cancelled(SigTerm)"
            } else {
                "ProcessCompleted"
            },
        ),
        (
            "status",
            if scenario == "cancel" {
                "signal:15"
            } else {
                "exit:0"
            },
        ),
        ("cleanup", "complete"),
    ] {
        ensure(
            values.get(key).map(String::as_str) == Some(expected),
            format!("unexpected {key}"),
        )?;
    }
    let bytes = if scenario == "bytes" { amount } else { 0 }.to_string();
    for key in ["stdout_bytes", "stderr_bytes"] {
        ensure(values.get(key) == Some(&bytes), "incorrect captured count")?;
    }
    for key in ["stdout_issue", "stderr_issue"] {
        ensure(values.get(key).is_none_or(|v| v == "none"), "capture issue")?;
    }
    for key in ["display_dropped_bytes", "display_unconfirmed_bytes"] {
        ensure(
            values.get(key).is_none_or(|v| v == "0"),
            "unexpected display bytes",
        )?;
    }
    Ok(())
}
fn ready(line: &[u8]) -> Result<()> {
    ensure(
        line.len() <= 256 && line.ends_with(b"\n"),
        "invalid READY framing",
    )?;
    let text = std::str::from_utf8(line)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure(
        fields.len() == 3 && fields[0] == "READY",
        "invalid READY event",
    )?;
    let pid = integer(fields[1], i32::MAX as u128)?;
    let pgid = integer(fields[2], i32::MAX as u128)?;
    ensure(pid > 0 && pid == pgid, "fixture does not own its group")
}

pub(crate) fn receive_ready(stream: &mut UnixStream, deadline: Duration) -> Result<()> {
    stream.set_nonblocking(true)?;
    let start = Instant::now();
    let mut line = Vec::new();
    loop {
        common::check_interrupt()?;
        ensure(start.elapsed() < deadline, "fixture READY deadline")?;
        let mut byte = [0];
        match stream.read(&mut byte) {
            Ok(1) => {
                line.push(byte[0]);
                ensure(line.len() <= 256, "oversized READY")?;
                if byte[0] == b'\n' {
                    return ready(&line);
                }
            }
            Ok(_) => return Err("fixture disconnected before READY".into()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_micros(200))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn sample(
    cli: &Path,
    fixture: &Path,
    scenario: &str,
    amount: u64,
    deadline: Duration,
) -> Result<BTreeMap<String, String>> {
    common::check_interrupt()?;
    let temp = Temporary::new("ff-bench")?;
    let mut command = common::command(cli);
    command.arg("run").arg("--").arg(fixture);
    match scenario {
        "exit" => {
            command.args(["exit", "0"]);
        }
        "bytes" => {
            command
                .arg("bytes")
                .arg(amount.to_string())
                .arg(amount.to_string());
        }
        "cancel" => {
            command.arg("wait");
        }
        _ => return Err("unknown scenario".into()),
    };
    let socket = temp.0.join("control.sock");
    let listener = UnixListener::bind(&socket)?;
    listener.set_nonblocking(true)?;
    if scenario == "cancel" {
        command.env("FAILFORGE_CONTROL", &socket);
    }
    let stdout = temp.0.join("stdout");
    let stderr = temp.0.join("stderr");
    command
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?);
    let start = Instant::now();
    let mut child = common::OwnedChild::new(command.spawn()?);
    let mut connection = None;
    let mut cancel_start = None;
    if scenario == "cancel" {
        let mut stream = loop {
            common::check_interrupt()?;
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure(start.elapsed() < deadline, "fixture READY deadline")?;
                    ensure(child.0.try_wait()?.is_none(), "fixture exited before READY")?;
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(error) => return Err(error.into()),
            }
        };
        receive_ready(&mut stream, deadline.saturating_sub(start.elapsed()))?;
        connection = Some(stream);
        cancel_start = Some(Instant::now());
        child.signal(libc::SIGTERM)?;
    }
    let code = child.wait(deadline.saturating_sub(start.elapsed()))?;
    let total = start.elapsed().as_nanos();
    let cancelled = cancel_start
        .map(|s| s.elapsed().as_nanos().to_string())
        .unwrap_or_default();
    drop(connection);
    validate(
        scenario,
        amount,
        code,
        &fs::read(stdout)?,
        &fs::read(stderr)?,
    )?;
    let bytes = if scenario == "bytes" { amount } else { 0 }.to_string();
    Ok(BTreeMap::from([
        ("total_ns".into(), total.to_string()),
        ("cancel_cleanup_ns".into(), cancelled),
        ("stdout_bytes".into(), bytes.clone()),
        ("stderr_bytes".into(), bytes),
    ]))
}
fn library_rows(
    raw: &str,
    iterations: usize,
    amount: u64,
) -> Result<Vec<BTreeMap<String, String>>> {
    let mut lines = raw.lines();
    let fields: Vec<_> = lines
        .next()
        .ok_or("missing library header")?
        .split(',')
        .collect();
    ensure(
        fields == FIELDS.split(',').skip(1).collect::<Vec<_>>(),
        "library CSV header mismatch",
    )?;
    let mut rows = Vec::new();
    for line in lines {
        let values: Vec<_> = line.split(',').collect();
        ensure(values.len() == fields.len(), "invalid library CSV row")?;
        rows.push(
            fields
                .iter()
                .zip(values)
                .map(|(a, b)| (a.to_string(), b.to_owned()))
                .collect::<BTreeMap<_, _>>(),
        );
    }
    ensure(
        rows.len() == 3 * iterations,
        "library sample count mismatch",
    )?;
    for scenario in SCENARIOS {
        let selected: Vec<_> = rows.iter().filter(|r| r["scenario"] == scenario).collect();
        ensure(
            selected.len() == iterations,
            "library scenario count mismatch",
        )?;
        for (index, row) in selected.iter().enumerate() {
            ensure(
                row["iteration"] == index.to_string(),
                "library sequence mismatch",
            )?;
            ensure(
                integer(&row["total_ns"], u128::MAX)? > 0,
                "nonpositive library timing",
            )?;
            let expected = if scenario == "bytes" { amount } else { 0 };
            for key in ["stdout_bytes", "stderr_bytes"] {
                ensure(
                    integer(&row[key], u64::MAX.into())? == u128::from(expected),
                    "library byte count mismatch",
                )?;
            }
            if scenario == "cancel" {
                integer(&row["cancel_cleanup_ns"], u128::MAX)?;
            }
        }
    }
    Ok(rows)
}
pub fn run(options: Options) -> Result<()> {
    let root = common::root();
    let release = options
        .target_dir
        .unwrap_or_else(|| root.join("target"))
        .join("release");
    let cli = release.join("failforge");
    let fixture = release.join("failforge-process-fixture");
    let library = release.join("examples/microbench");
    for path in [&cli, &fixture, &library] {
        ensure(
            path.is_file(),
            format!("missing release executable {}", path.display()),
        )?;
    }
    common::create_output(&options.output)?;
    common::write_json(
        &options.output.join("environment.json"),
        &json!({"schema_version":1,"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"logical_cpus":std::thread::available_parallelism()?.get(),"iterations":options.iterations,"warmup_per_scenario":options.warmup,"bytes_per_stream":options.bytes,"cli_sample_deadline_seconds":options.deadline,"profile":"release","binaries":{"cli":{"path":cli,"sha256":common::hash(&cli)?},"fixture":{"path":fixture,"sha256":common::hash(&fixture)?},"library_example":{"path":library,"sha256":common::hash(&library)?}},"measurement":{"units":"nanoseconds","p95":"nearest rank ceil(0.95*n)","new_cli":"process spawn through reaped exit, polling at 200 microseconds","live_runner":"one runtime and production Runner"}}),
    )?;
    let mut command = common::command(&library);
    command
        .arg("--fixture")
        .arg(&fixture)
        .arg("--iterations")
        .arg(options.iterations.to_string())
        .arg("--warmup")
        .arg(options.warmup.to_string())
        .arg("--bytes")
        .arg(options.bytes.to_string());
    common::execute(
        &mut command,
        &options.output.join("library"),
        Duration::from_secs(
            ((options.iterations + options.warmup) as u64)
                .saturating_mul(60)
                .max(30),
        ),
        0,
    )?;
    let mut rows = library_rows(
        &fs::read_to_string(options.output.join("library/stdout.log"))?,
        options.iterations,
        options.bytes,
    )?;
    for row in &mut rows {
        row.insert("entrypoint".into(), "live_runner".into());
    }
    let mut raw = File::create(options.output.join("raw.csv"))?;
    writeln!(raw, "{FIELDS}")?;
    for row in &rows {
        writeln!(
            raw,
            "{}",
            FIELDS
                .split(',')
                .map(|f| row.get(f).map(String::as_str).unwrap_or(""))
                .collect::<Vec<_>>()
                .join(",")
        )?;
    }
    for scenario in SCENARIOS {
        for _ in 0..options.warmup {
            sample(
                &cli,
                &fixture,
                scenario,
                options.bytes,
                Duration::from_secs_f64(options.deadline),
            )?;
        }
        for iteration in 0..options.iterations {
            let mut row = sample(
                &cli,
                &fixture,
                scenario,
                options.bytes,
                Duration::from_secs_f64(options.deadline),
            )?;
            row.insert("entrypoint".into(), "new_cli".into());
            row.insert("scenario".into(), scenario.into());
            row.insert("iteration".into(), iteration.to_string());
            writeln!(
                raw,
                "{}",
                FIELDS
                    .split(',')
                    .map(|f| row.get(f).map(String::as_str).unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(",")
            )?;
            raw.flush()?;
            rows.push(row);
        }
    }
    let mut result = serde_json::Map::new();
    for entry in ["live_runner", "new_cli"] {
        for scenario in SCENARIOS {
            let selected: Vec<_> = rows
                .iter()
                .filter(|r| r["entrypoint"] == entry && r["scenario"] == scenario)
                .collect();
            let total = selected
                .iter()
                .map(|r| r["total_ns"].parse())
                .collect::<std::result::Result<Vec<u128>, _>>()?;
            let mut stats = json!({"total":statistics(&total)?});
            if scenario == "cancel" {
                let times = selected
                    .iter()
                    .map(|r| r["cancel_cleanup_ns"].parse())
                    .collect::<std::result::Result<Vec<u128>, _>>()?;
                stats["cancel_cleanup"] = statistics(&times)?;
            }
            result.insert(format!("{entry}/{scenario}"), stats);
        }
    }
    common::write_json(&options.output.join("summary.json"), &Value::Object(result))?;
    println!("{}", options.output.display());
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stats_nearest_rank_and_median() {
        assert_eq!(
            statistics(&(1..=20).collect::<Vec<_>>()).unwrap()["p95_ns"],
            19
        );
        assert_eq!(statistics(&[9, 1, 4, 2]).unwrap()["median_ns"], 3.0);
        assert!(statistics(&[]).is_err());
    }
    #[test]
    fn numeric_limits() {
        for text in ["", "-1", "01", "+2", "1x"] {
            assert!(integer(text, u64::MAX.into()).is_err());
        }
        assert!(integer("18446744073709551616", u64::MAX.into()).is_err());
        assert!(seconds("NaN").is_err());
        assert!(seconds("inf").is_err());
        assert!(positive("0").is_err());
        for text in ["+1", "-1", "١", "1.0", " 1"] {
            assert!(count(text).is_err());
            assert!(positive(text).is_err());
        }
        assert_eq!(count("0").unwrap(), 0);
        assert_eq!(positive("01").unwrap(), 1);
    }
    #[test]
    fn ready_requires_group_and_frame() {
        assert!(ready(b"READY 3 3\n").is_ok());
        for bad in [
            b"READY 0 0\n".as_slice(),
            b"READY 3 4\n",
            b"READY 3 3",
            b"READY 3 3 extra\n",
        ] {
            assert!(ready(bad).is_err());
        }
    }
    #[test]
    fn summary_rejects_ambiguity_and_forgery() {
        for bad in [
            "failforge: cause=ProcessCompleted cause=TimedOut\n",
            "failforge: cause=ProcessCompleted\nfailforge: status=exit:0\n",
            "failforge: status=exit:256\n",
            "failforge: stdout_bytes=0 stdout_tail_truncated=true\n",
            "failforge: cleanup=other\n",
            "failforge: cause=bad\n",
            "failforge: cause=ProcessCompleted bogus=0\n",
        ] {
            assert!(summary(bad.as_bytes()).is_err(), "{bad}");
        }
    }
    #[test]
    fn validate_rejects_wrong_capture_and_status() {
        let raw=b"failforge: cause=ProcessCompleted status=exit:0 cleanup=complete stdout_bytes=4 stderr_bytes=4\n";
        assert!(validate("bytes", 4, 0, b"", raw).is_ok());
        assert!(validate("bytes", 3, 0, b"", raw).is_err());
        assert!(validate("bytes", 4, 0, b"unexpected", raw).is_err());
        assert!(validate("cancel", 4, 0, b"", raw).is_err());
    }
    #[test]
    fn library_validates_sequence_and_bytes() {
        let header = FIELDS.split_once(',').unwrap().1;
        let raw = format!(
            "{header}\nexit,0,1,,0,0,0,0,0,0,0\nbytes,0,1,,0,0,0,0,0,4,4\ncancel,0,1,1,0,0,0,0,0,0,0\n"
        );
        assert_eq!(library_rows(&raw, 1, 4).unwrap().len(), 3);
        assert!(library_rows(&raw, 2, 4).is_err());
        assert!(library_rows(&raw, 1, 3).is_err());
        assert!(library_rows(&raw.replace("bytes,0", "bytes,1"), 1, 4).is_err());
    }
}
