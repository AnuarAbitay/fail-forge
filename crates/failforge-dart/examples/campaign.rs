//! Stream validation and read-only patch evidence for every discovered mutant.
//! Test execution and isolated copies belong to the caller, not this example.
use std::error::Error;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use failforge_dart::{
    AdapterClient, ClientOptions, MAX_LINE_BYTES, Mutant, ValidationState, read_and_apply_mutant,
    sha256_hex, validate_relative_target,
};
use failforge_runner::{CommandSpec, Runner};
use serde::Serialize;
use serde_json::{Value, json};

type Failure = Box<dyn Error>;
const MAX_MUTANTS: u64 = 10_000;
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

struct Configuration {
    dart: OsString,
    adapter: PathBuf,
    package: PathBuf,
    payload: Value,
    candidate_limit: u64,
}

impl Configuration {
    fn from_args(args: Vec<OsString>) -> Result<Self, Failure> {
        if args.len() != 3 {
            return Err("usage: campaign DART_EXECUTABLE ADAPTER_ROOT PAYLOAD_JSON_FILE".into());
        }
        let payload = read_payload(File::open(&args[2])?)?;
        let candidate_limit = candidate_limit(&payload)?;
        let package = package_root(&payload)?;
        let adapter = PathBuf::from(&args[1]).canonicalize()?;
        Ok(Self {
            dart: args[0].clone(),
            adapter,
            package,
            payload,
            candidate_limit,
        })
    }

    fn spec(&self) -> CommandSpec {
        CommandSpec {
            program: self.dart.clone(),
            args: vec![
                format!(
                    "--packages={}",
                    self.adapter
                        .join(".dart_tool/package_config.json")
                        .display()
                )
                .into(),
                self.adapter
                    .join("bin/failforge_dart_adapter.dart")
                    .into_os_string(),
            ],
            cwd: self.adapter.clone(),
            env: Default::default(),
            timeout_policy: Default::default(),
            capture_policy: Default::default(),
        }
    }
}

fn read_payload(input: impl Read) -> Result<Value, Failure> {
    let mut bytes = Vec::new();
    input
        .take(u64::try_from(MAX_LINE_BYTES)? + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LINE_BYTES {
        return Err("normalized payload file exceeds the eight-MiB byte limit".into());
    }
    let payload: Value = serde_json::from_slice(&bytes)?;
    if !payload.is_object() {
        return Err("normalized campaign payload must be a JSON object".into());
    }
    Ok(payload)
}

fn candidate_limit(payload: &Value) -> Result<u64, Failure> {
    let Some(limits) = payload.get("limits") else {
        return Ok(1000); // The adapter's existing default, not a new setting.
    };
    let limits = limits
        .as_object()
        .ok_or("payload limits must be a JSON object")?;
    if limits.keys().any(|key| key != "maxMutants") {
        return Err("unknown campaign limits field".into());
    }
    let Some(limit) = limits.get("maxMutants") else {
        return Ok(1000);
    };
    limit
        .as_u64()
        .filter(|count| *count <= MAX_MUTANTS)
        .ok_or_else(|| "maxMutants must be an integer from zero through 10000".into())
}

fn package_root(payload: &Value) -> Result<PathBuf, Failure> {
    let workspace = payload
        .get("workspaceRoot")
        .and_then(Value::as_str)
        .ok_or("payload workspaceRoot must be a string")?;
    let workspace = Path::new(workspace);
    if !workspace.is_absolute() {
        return Err("payload workspaceRoot must be absolute".into());
    }
    let workspace = workspace.canonicalize()?;
    if !workspace.is_dir() {
        return Err("payload workspaceRoot must be a directory".into());
    }
    let relative = payload
        .get("packageRoot")
        .and_then(Value::as_str)
        .ok_or("payload packageRoot must be a canonical relative string")?;
    if relative != "." {
        validate_relative_target(relative)?;
    }
    let package = workspace.join(relative).canonicalize()?;
    if !package.is_dir() || !package.starts_with(&workspace) {
        return Err("payload packageRoot must remain inside workspaceRoot".into());
    }
    Ok(package)
}

#[derive(Default)]
struct Statistics {
    validated: u64,
    valid: u64,
}

#[derive(Default)]
struct SessionEvidence {
    close_succeeded: bool,
    exit_code: Option<i32>,
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    let configuration = Configuration::from_args(std::env::args_os().skip(1).collect())?;
    let mut stdout = io::stdout().lock();
    execute(configuration, &mut stdout).await
}

async fn execute(configuration: Configuration, output: &mut dyn Write) -> Result<(), Failure> {
    let runner = Arc::new(Runner::new());
    let mut statistics = Statistics::default();
    let mut evidence = SessionEvidence::default();
    let outcome = session(
        runner.clone(),
        &configuration,
        output,
        &mut statistics,
        &mut evidence,
    )
    .await;
    // This executes on every session result, including transport, model,
    // validation, patch and consumer I/O failures. Only this Runner is owned.
    let shutdown = runner.shutdown().await;
    let active_runs = runner.active_runs();
    let cleanup = emit(
        output,
        "cleanup",
        &json!({
            "complete":shutdown.is_ok() && active_runs == 0,
            "adapterCloseSucceeded":evidence.close_succeeded,
            "exitCode":evidence.exit_code,
            "runnerShutdown":shutdown.is_ok(),
            "activeRuns":active_runs,
            "campaignSucceeded":outcome.is_ok(),
            "validatedCandidates":statistics.validated,
            "staticallyValidCandidates":statistics.valid,
            "writesPerformed":false,
        }),
    );
    shutdown?;
    cleanup?;
    outcome
}

async fn session(
    runner: Arc<Runner>,
    configuration: &Configuration,
    output: &mut dyn Write,
    statistics: &mut Statistics,
    evidence: &mut SessionEvidence,
) -> Result<(), Failure> {
    let mut client = AdapterClient::start(
        runner,
        configuration.spec(),
        ClientOptions {
            request_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    )?;
    let outcome = steps(&mut client, configuration, output, statistics).await;
    let closed = client.close().await;
    if let Ok(result) = &closed {
        evidence.close_succeeded = result.cleanup.is_complete();
        evidence.exit_code = result.status.and_then(|status| status.code());
    }
    outcome?;
    let result = closed?;
    if !result.cleanup.is_complete() || !result.status.is_some_and(|status| status.success()) {
        return Err("adapter campaign did not finish successfully".into());
    }
    Ok(())
}

async fn steps(
    client: &mut AdapterClient,
    configuration: &Configuration,
    output: &mut dyn Write,
    statistics: &mut Statistics,
) -> Result<(), Failure> {
    emit(output, "handshake", &client.handshake().await?)?;
    let discovery = client.discover(configuration.payload.clone()).await?;
    emit(output, "discover", &discovery)?;
    let truncated = discovery
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or("discovery response has no truncation flag")?;
    if truncated {
        return Err("discovery was truncated; narrow the campaign scope or raise maxMutants within its bound".into());
    }
    let mutants = discovery
        .get("mutants")
        .and_then(Value::as_array)
        .ok_or("discovery response has no mutants array")?;
    if u64::try_from(mutants.len())? > configuration.candidate_limit {
        return Err("discovery response exceeds the requested candidate limit".into());
    }
    for (index, original) in mutants.iter().enumerate() {
        let mut mutant: Mutant = serde_json::from_value(original.clone())?;
        mutant.validate_model()?;
        let mut validation_request = configuration.payload.clone();
        validation_request["mutant"] = original.clone();
        let validation = client.validate(validation_request).await?;
        let valid = match (
            validation.get("valid").and_then(Value::as_bool),
            validation.get("validation").and_then(Value::as_str),
        ) {
            (Some(true), Some("valid")) => true,
            (Some(false), Some("invalid")) => false,
            _ => return Err("adapter returned inconsistent validation metadata".into()),
        };
        statistics.validated += 1;
        let safe_patch = if valid {
            // Only this local DTO is changed. Original discovery JSON remains
            // unvalidated; static evidence and byte applicability are separate.
            mutant.validation = ValidationState::Valid;
            let bytes = read_and_apply_mutant(&configuration.package, &mutant, MAX_SOURCE_BYTES)?;
            let patched_hash = sha256_hex(&bytes);
            let patched_bytes = bytes.len();
            let patched_utf8 = String::from_utf8(bytes)?;
            statistics.valid += 1;
            json!({
                "applied":true,"mutantId":mutant.id,"file":mutant.file,
                "patchedHash":patched_hash,"patchedBytes":patched_bytes,
                "patchedUtf8":patched_utf8,"writesPerformed":false,
            })
        } else {
            json!({
                "applied":false,"mutantId":mutant.id,"file":mutant.file,
                "reason":"static_invalid","writesPerformed":false,
            })
        };
        emit(
            output,
            "candidate",
            &json!({"index":index,"mutant":original,"validation":validation,"safePatch":safe_patch}),
        )?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Event<'a> {
    step: &'a str,
    payload: &'a Value,
}

fn emit(output: &mut dyn Write, step: &str, payload: &Value) -> Result<(), Failure> {
    serde_json::to_writer(&mut *output, &Event { step, payload })?;
    output.write_all(b"\n")?;
    // The caller can process candidates as they arrive, without waiting for the
    // campaign to finish or buffering the aggregate output in this example.
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn payload_read_and_candidate_limits_are_bounded_and_strict() {
        assert!(read_payload(&b"[]"[..]).is_err());
        assert!(read_payload(&b"{"[..]).is_err());
        assert!(read_payload(io::repeat(b' ').take(MAX_LINE_BYTES as u64 + 1)).is_err());
        let exact = format!("{{}}{}", " ".repeat(MAX_LINE_BYTES - 2));
        assert_eq!(read_payload(exact.as_bytes()).unwrap(), json!({}));
        assert_eq!(candidate_limit(&json!({})).unwrap(), 1000);
        for limit in [0, 10_000] {
            assert_eq!(
                candidate_limit(&json!({"limits":{"maxMutants":limit}})).unwrap(),
                limit
            );
        }
        for limit in [json!(-1), json!(10001), json!(1.5), json!(true), json!("2")] {
            assert!(candidate_limit(&json!({"limits":{"maxMutants":limit}})).is_err());
        }
        assert!(candidate_limit(&json!({"limits":null})).is_err());
        assert!(candidate_limit(&json!({"limits":{"unknown":1}})).is_err());
        for count in [0, 1, 2, 4] {
            assert!(Configuration::from_args(vec![OsString::from("unused"); count]).is_err());
        }
    }

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(source: &str) -> Self {
            let path = loop {
                let path = std::env::temp_dir().join(format!(
                    "ff campaign {} {}",
                    std::process::id(),
                    NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create owned campaign fixture: {error}"),
                }
            };
            let fixture = Self(path.canonicalize().unwrap());
            std::fs::create_dir_all(path.join("lib")).unwrap();
            std::fs::create_dir_all(path.join(".dart_tool")).unwrap();
            std::fs::write(path.join("lib/source.dart"), source).unwrap();
            std::fs::write(
                path.join("pubspec.yaml"),
                "name: campaign_fixture\nenvironment:\n  sdk: '>=3.13.0 <3.14.0'\n",
            )
            .unwrap();
            std::fs::write(path.join(".dart_tool/package_config.json"), json!({"configVersion":2,"packages":[{"name":"campaign_fixture","rootUri":"../","packageUri":"lib/","languageVersion":"3.13"}]}).to_string()).unwrap();
            fixture
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn configuration(fixture: &Fixture, operator: &str, max: u64) -> Configuration {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let sdk = root.join(".toolchain/flutter/bin/cache/dart-sdk");
        Configuration {
            dart: sdk.join("bin/dart").into_os_string(),
            adapter: root.join("adapters/dart"),
            package: fixture.0.clone(),
            payload: json!({"workspaceRoot":fixture.0,"packageRoot":".","dartSdkPath":sdk,"rules":{"operators":[operator]},"limits":{"maxMutants":max}}),
            candidate_limit: max,
        }
    }

    fn events(output: &[u8]) -> Vec<Value> {
        std::str::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    #[ignore = "requires the explicitly installed local Dart SDK and adapter package configuration"]
    async fn actual_campaign_streams_every_candidate_keeps_original_json_and_releases_processes() {
        let source = "bool first() => true;\nbool second() => false;\n";
        let fixture = Fixture::new(source);
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                configuration(&fixture, "BOOLEAN_LITERAL_FLIP", 10),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let events = events(&output);
        assert_eq!(
            events
                .iter()
                .map(|event| event["step"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["handshake", "discover", "candidate", "candidate", "cleanup"]
        );
        for event in &events[2..4] {
            let payload = &event["payload"];
            assert_eq!(payload["mutant"]["validation"], "unvalidated");
            assert_eq!(payload["validation"]["valid"], true);
            assert_eq!(payload["safePatch"]["applied"], true);
            assert_eq!(payload["safePatch"]["writesPerformed"], false);
            assert_eq!(payload["mutant"]["id"], payload["safePatch"]["mutantId"]);
            let patched = payload["safePatch"]["patchedUtf8"].as_str().unwrap();
            assert_ne!(patched, source);
            assert_eq!(
                payload["safePatch"]["patchedHash"],
                sha256_hex(patched.as_bytes())
            );
        }
        assert_eq!(events.last().unwrap()["payload"]["complete"], true);
        assert_eq!(events.last().unwrap()["payload"]["activeRuns"], 0);
        assert_eq!(events.last().unwrap()["payload"]["validatedCandidates"], 2);
        assert_eq!(
            std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
            source.as_bytes()
        );
    }

    #[tokio::test]
    #[ignore = "requires the explicitly installed local Dart SDK and adapter package configuration"]
    async fn invalid_and_truncated_discovery_are_explicit_and_still_cleanup() {
        let source =
            "int length(String? value) { if(value != null) return value.length; return 0; }\n";
        let fixture = Fixture::new(source);
        let mut output = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                configuration(&fixture, "CONDITIONAL_NEGATION", 10),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let invalid = events(&output);
        let candidate = invalid
            .iter()
            .find(|event| event["step"] == "candidate")
            .unwrap();
        assert_eq!(candidate["payload"]["validation"]["validation"], "invalid");
        assert_eq!(candidate["payload"]["safePatch"]["applied"], false);
        assert!(
            candidate["payload"]["safePatch"]
                .get("patchedUtf8")
                .is_none()
        );
        assert_eq!(invalid.last().unwrap()["payload"]["complete"], true);

        let mut output = Vec::new();
        let error = tokio::time::timeout(
            Duration::from_secs(30),
            execute(
                configuration(&fixture, "CONDITIONAL_NEGATION", 0),
                &mut output,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("truncated"));
        let truncated = events(&output);
        assert_eq!(
            truncated
                .iter()
                .map(|event| event["step"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["handshake", "discover", "cleanup"]
        );
        assert_eq!(truncated[1]["payload"]["truncated"], true);
        assert_eq!(truncated.last().unwrap()["payload"]["complete"], true);
        assert_eq!(
            truncated.last().unwrap()["payload"]["campaignSucceeded"],
            false
        );
        assert_eq!(
            std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
            source.as_bytes()
        );
    }

    struct DisconnectedConsumer;
    impl Write for DisconnectedConsumer {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "consumer closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "requires the explicitly installed local Dart SDK and adapter package configuration"]
    async fn consumer_io_failure_still_closes_the_adapter_and_releases_native_processes() {
        let fixture = Fixture::new("bool first() => true;\n");
        let configuration = configuration(&fixture, "BOOLEAN_LITERAL_FLIP", 10);
        let runner = Arc::new(Runner::new());
        let mut statistics = Statistics::default();
        let mut evidence = SessionEvidence::default();
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            session(
                runner.clone(),
                &configuration,
                &mut DisconnectedConsumer,
                &mut statistics,
                &mut evidence,
            ),
        )
        .await;
        // Cleanup precedes every assertion so even a failing watchdog or
        // assertion does not leave this fixture's native child unattended.
        let shutdown = tokio::time::timeout(Duration::from_secs(10), runner.shutdown()).await;
        shutdown.unwrap().unwrap();
        assert_eq!(runner.active_runs(), 0);
        assert!(
            outcome
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("consumer closed")
        );
        assert!(evidence.close_succeeded);
        assert_eq!(evidence.exit_code, Some(0));
        assert_eq!(statistics.validated, 0);
        assert_eq!(
            std::fs::read(fixture.0.join("lib/source.dart")).unwrap(),
            b"bool first() => true;\n"
        );
    }
}
