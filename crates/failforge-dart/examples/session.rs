//! Run a persistent adapter session; emit evidence without mutating originals.
use std::error::Error;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use failforge_dart::{
    AdapterClient, ClientOptions, Mutant, ValidationState, read_and_apply_mutant, sha256_hex,
};
use failforge_runner::{CommandSpec, Runner};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !(5..=8).contains(&args.len()) {
        return Err("usage: session DART_EXECUTABLE ADAPTER_ROOT WORKSPACE_ROOT PACKAGE_ROOT DART_SDK_ROOT [FLUTTER_SDK_ROOT] [NORMALIZED_RULES_JSON_FILE] [MUTANT_INDEX]".into());
    }
    let selected_index = args
        .get(7)
        .map(|index| index.parse::<usize>())
        .transpose()?;
    let adapter = PathBuf::from(&args[1]).canonicalize()?;
    let workspace = PathBuf::from(&args[2]).canonicalize()?;
    let workspace_text = workspace
        .to_str()
        .ok_or("workspace root must be UTF-8 for the adapter protocol")?;
    let mut payload = json!({"workspaceRoot":workspace_text,"packageRoot":args[3],"dartSdkPath":args[4],"rules":{},"limits":{"maxMutants":32}});
    if let Some(flutter) = args.get(5).filter(|path| !path.is_empty()) {
        payload["flutterSdkPath"] = json!(flutter);
    }
    if let Some(rules) = args.get(6) {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(rules)?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8 * 1024 * 1024 {
            return Err("normalized rules file exceeds its byte limit".into());
        }
        let rules: Value = serde_json::from_slice(&bytes)?;
        if !rules.is_object() {
            return Err("normalized rules must be a JSON object".into());
        }
        payload["rules"] = rules;
    }
    let mut packages = OsString::from("--packages=");
    packages.push(failforge_workspace::dart_vm_argument(
        &adapter.join(".dart_tool/package_config.json"),
    )?);
    let spec = CommandSpec {
        program: args[0].clone().into(),
        args: vec![
            packages,
            failforge_workspace::dart_vm_argument(
                &adapter.join("bin/failforge_dart_adapter.dart"),
            )?,
        ],
        cwd: adapter,
        env: Default::default(),
        timeout_policy: Default::default(),
        capture_policy: Default::default(),
    };
    let runner = Arc::new(Runner::new());
    let outcome = run_session(
        runner.clone(),
        spec,
        payload,
        &workspace.join(&args[3]),
        selected_index,
    )
    .await;
    let cleanup = runner.shutdown().await;
    cleanup?;
    outcome
}

async fn run_session(
    runner: Arc<Runner>,
    spec: CommandSpec,
    payload: Value,
    package: &std::path::Path,
    selected_index: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    let mut client = AdapterClient::start(
        runner,
        spec,
        ClientOptions {
            request_timeout: Duration::from_secs(30),
            ..Default::default()
        },
    )?;
    let outcome = steps(&mut client, payload, package, selected_index).await;
    let closed = client.close().await;
    outcome?;
    let result = closed?;
    if !result.status.is_some_and(|status| status.success()) || !result.cleanup.is_complete() {
        return Err("adapter session did not finish successfully".into());
    }
    emit(
        "cleanup",
        json!({"complete":true,"exitCode":result.status.and_then(|status|status.code())}),
    )?;
    Ok(())
}

async fn steps(
    client: &mut AdapterClient,
    payload: Value,
    package: &std::path::Path,
    selected_index: Option<usize>,
) -> Result<(), Box<dyn Error>> {
    emit("handshake", client.handshake().await?)?;
    let discovery = client.discover(payload.clone()).await?;
    emit("discover", discovery.clone())?;
    let mutants = discovery
        .get("mutants")
        .and_then(Value::as_array)
        .ok_or("discovery response has no mutants array")?;
    if selected_index.is_some_and(|index| index >= mutants.len()) {
        return Err("requested mutant index is outside the discovery response".into());
    }
    for (index, value) in mutants.iter().enumerate() {
        if selected_index.is_some_and(|selected| selected != index) {
            continue;
        }
        let mut mutant: Mutant = serde_json::from_value(value.clone())?;
        mutant.validate_model()?;
        let mut validation_request = payload.clone();
        validation_request["mutant"] = value.clone();
        let validation = client.validate(validation_request).await?;
        emit("validate", validation.clone())?;
        if validation.get("validation").and_then(Value::as_str) != Some("valid") {
            continue;
        }
        mutant.validation = ValidationState::Valid;
        let bytes = read_and_apply_mutant(package, &mutant, 4 * 1024 * 1024)?;
        let patched_utf8 = String::from_utf8(bytes.clone())?;
        emit(
            "safePatch",
            json!({"applied":true,"mutantId":mutant.id,"file":mutant.file,"patchedHash":sha256_hex(&bytes),"patchedBytes":bytes.len(),"patchedUtf8":patched_utf8,"writesPerformed":false}),
        )?;
        return Ok(());
    }
    emit(
        "safePatch",
        json!({"applied":false,"reason":"no_statically_valid_candidate","writesPerformed":false}),
    )?;
    Ok(())
}

fn emit(step: &str, payload: Value) -> Result<(), Box<dyn Error>> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &json!({"step":step,"payload":payload}))?;
    stdout.write_all(b"\n")?;
    Ok(())
}
