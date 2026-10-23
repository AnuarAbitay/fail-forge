use crate::common::{self, Result, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
#[derive(Args)]
pub struct Options {
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long)]
    pub dart_sdk: Option<PathBuf>,
    #[arg(long)]
    pub installation: Option<PathBuf>,
}
fn expected() -> BTreeMap<String, String> {
    [
        ("NSM", 5, "null_safety"),
        ("DPM", 3, "parameters"),
        ("SWM", 3, "switch"),
        ("PMM", 3, "patterns"),
        ("CLM", 4, "collections"),
        ("ENM", 1, "enums"),
    ]
    .into_iter()
    .flat_map(|(prefix, n, category)| {
        (1..=n).map(move |i| (format!("{prefix}-{i:03}"), category.to_owned()))
    })
    .collect()
}
fn catalog(value: &Value) -> Result<&Vec<Value>> {
    let cases = value["cases"].as_array().ok_or("missing semantic cases")?;
    ensure(
        value["family"] == "language_specific" && value["language"] == "dart" && cases.len() == 19,
        "invalid semantic catalogue",
    )?;
    let actual: BTreeMap<_, _> = cases
        .iter()
        .map(|c| {
            Ok((
                c["operatorId"]
                    .as_str()
                    .ok_or("missing operator")?
                    .to_owned(),
                c["category"].as_str().ok_or("missing category")?.to_owned(),
            ))
        })
        .collect::<Result<_>>()?;
    ensure(
        actual == expected()
            && cases
                .iter()
                .all(|c| c["source"].as_str().is_some_and(|s| !s.is_empty())),
        "catalogue must contain exactly all 19 operators",
    )?;
    Ok(cases)
}
pub fn transcript(
    path: &Path,
    project: &Path,
    family: &str,
) -> Result<(BTreeMap<String, Value>, Vec<Value>)> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure(
        file.metadata()?.is_file(),
        "transcript is not a regular file",
    )?;
    let mut bytes = Vec::new();
    file.take(failforge_dart::discovery::MAX_TRANSCRIPT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let retained = failforge_dart::discovery::load_retained_transcript(&bytes, project, family)?;
    Ok((retained.steps, retained.candidates))
}
fn vm_outcome(code: i32, raw: &[u8], errors: &[u8], receipt: &[u8]) -> Result<&'static str> {
    let summary = crate::benchmark::summary(receipt)?;
    for (key, value) in [
        ("cleanup", "complete".to_owned()),
        ("cause", "ProcessCompleted".to_owned()),
        ("status", format!("exit:{code}")),
        ("stdout_bytes", raw.len().to_string()),
        ("stderr_bytes", errors.len().to_string()),
        ("stdout_issue", "none".to_owned()),
        ("stderr_issue", "none".to_owned()),
    ] {
        ensure(
            summary.get(key) == Some(&value),
            format!("incomplete VM {key} evidence"),
        )?;
    }
    if code == 0 {
        return Ok("UNCHANGED");
    }
    let normalized = String::from_utf8_lossy(errors).replace("\r\n", "\n");
    ensure(
        normalized.starts_with("Unhandled exception:\nBad state: MUTATION_OBSERVED\n"),
        "VM failed without fixture assertion",
    )?;
    Ok("OBSERVED")
}
fn vm(cli: &Path, dart: &Path, project: &Path, directory: &Path, source: &[u8]) -> Result<Value> {
    fs::create_dir_all(directory)?;
    let source_file = directory.join("case.dart");
    fs::write(&source_file, source)?;
    let capture = directory.join("capture");
    let mut cmd = common::command(cli);
    cmd.arg("run")
        .args(["--timeout", "5s", "--cwd"])
        .arg(project)
        .arg("--capture-dir")
        .arg(&capture)
        .arg("--")
        .arg(dart)
        .arg(format!(
            "--packages={}",
            project.join(".dart_tool/package_config.json").display()
        ))
        .arg(&source_file);
    // The VM is allowed to report its fixture assertion. Retain its actual exit
    // rather than treating a nonzero code as an infrastructure verdict.
    let start = Instant::now();
    cmd.stdout(fs::File::create(directory.join("stdout.log"))?)
        .stderr(fs::File::create(directory.join("stderr.log"))?);
    let mut owner = common::OwnedChild::new(cmd.spawn()?);
    let code = owner.wait(Duration::from_secs(30))?;
    let raw = fs::read(capture.join("stdout.bin"))?;
    let errors = fs::read(capture.join("stderr.bin"))?;
    let outcome = vm_outcome(
        code,
        &raw,
        &errors,
        &fs::read(directory.join("stderr.log"))?,
    )?;
    let result = json!({"outcome":outcome,"exitCode":code,"wallSeconds":start.elapsed().as_secs_f64(),"sourceSha256":failforge_dart::sha256_hex(source),"stdoutSha256":failforge_dart::sha256_hex(&raw),"stderrSha256":failforge_dart::sha256_hex(&errors)});
    common::write_json(&directory.join("result.json"), &result)?;
    Ok(result)
}
pub fn run(options: Options) -> Result<()> {
    let root = options
        .installation
        .unwrap_or_else(common::root)
        .canonicalize()?;
    let sdk = options
        .dart_sdk
        .unwrap_or_else(|| root.join(".toolchain/flutter/bin/cache/dart-sdk"))
        .canonicalize()?;
    common::create_output(&options.output)?;
    let output = options.output.canonicalize()?;
    let start = Instant::now();
    let catalog_file = root.join("protocol/fixtures/dart-semantic-catalog.json");
    let value = common::read_json(&catalog_file)?;
    let cases = catalog(&value)?;
    let cli = root.join("target/debug/failforge");
    let helper = root.join("target/debug/failforge-adapter-campaign");
    let dart = sdk.join("bin/dart");
    let adapter = root.join("adapters/dart");
    let tools = [
        catalog_file,
        dart.clone(),
        cli.clone(),
        helper.clone(),
        adapter.join("bin/failforge_dart_adapter.dart"),
        adapter.join(".dart_tool/package_config.json"),
    ];
    let hashes: Vec<_> = tools
        .iter()
        .map(|p| common::hash(p))
        .collect::<Result<_>>()?;
    let temporary = common::Temporary::new("ff-semantic")?;
    let project = temporary.0.join("project");
    fs::create_dir_all(project.join("lib"))?;
    fs::create_dir(project.join(".dart_tool"))?;
    fs::write(
        project.join("pubspec.yaml"),
        "name: semantic_fixture\nenvironment:\n  sdk: '>=3.13.0 <4.0.0'\n",
    )?;
    common::write_json(
        &project.join(".dart_tool/package_config.json"),
        &json!({"configVersion":2,"packages":[{"name":"semantic_fixture","rootUri":"../","packageUri":"lib/","languageVersion":"3.13"}]}),
    )?;
    common::write_json(
        &project.join(".dart_tool/package_graph.json"),
        &json!({"roots":["semantic_fixture"]}),
    )?;
    for case in cases {
        fs::write(
            project.join(format!("lib/{}.dart", case["operatorId"].as_str().unwrap())),
            case["source"].as_str().unwrap(),
        )?;
    }
    let originals = common::tree(&project)?;
    let payload = json!({"workspaceRoot":project,"packageRoot":".","dartSdkPath":sdk,"rules":{"operators":expected().keys().collect::<Vec<_>>(),"methods":["subject"],"components":cases.iter().map(|c|json!({"name":c["operatorId"],"includePaths":[format!("lib/{}.dart",c["operatorId"].as_str().unwrap())],"operators":[c["operatorId"].clone()],"methods":["subject"]})).collect::<Vec<_>>()},"limits":{"maxMutants":1000}});
    common::write_json(&output.join("payload.json"), &payload)?;
    let mut discover = common::command(&helper);
    discover
        .arg(&dart)
        .arg(&adapter)
        .arg(output.join("payload.json"));
    let receipt = common::execute(
        &mut discover,
        &output.join("discovery"),
        Duration::from_secs(180),
        0,
    )?;
    let (steps, candidates) = transcript(
        &output.join("discovery/stdout.log"),
        &project,
        "language_specific",
    )?;
    let ids: BTreeSet<_> = candidates
        .iter()
        .filter_map(|c| c["mutant"]["operatorId"].as_str())
        .collect();
    ensure(
        ids == expected().keys().map(String::as_str).collect(),
        "discovery did not exercise all 19 operators",
    )?;
    let mut results = Vec::new();
    for case in cases {
        let id = case["operatorId"].as_str().unwrap();
        ensure(
            steps["handshake"]["operators"]
                .as_array()
                .is_some_and(|operators| {
                    operators.iter().any(|operator| {
                        operator["id"] == id
                            && operator["language"] == "dart"
                            && operator["category"] == case["category"]
                    })
                }),
            "semantic operator category or language differs from catalogue",
        )?;
        println!("semantic {id}");
        let baseline = vm(
            &cli,
            &dart,
            &project,
            &output.join(id).join("baseline"),
            case["source"].as_str().unwrap().as_bytes(),
        )?;
        ensure(
            baseline["outcome"] == "UNCHANGED",
            "semantic baseline failed",
        )?;
        let mut mutants = Vec::new();
        for candidate in candidates
            .iter()
            .filter(|c| c["mutant"]["operatorId"] == id)
        {
            let proof = failforge_dart::discovery::verify_candidate(&project, candidate)?;
            ensure(
                proof.validation["valid"] == true
                    && proof.mutant.file == format!("lib/{id}.dart")
                    && candidate["mutant"]["symbol"]["methodName"] == "subject",
                "invalid semantic candidate",
            )?;
            let source =
                failforge_dart::read_and_apply_mutant(&project, &proof.mutant, 4 * 1024 * 1024)?;
            let result = vm(
                &cli,
                &dart,
                &project,
                &output.join(id).join(&proof.mutant.id),
                &source,
            )?;
            mutants.push(json!({"mutantId":proof.mutant.id,"edit":candidate["mutant"]["edit"],"result":result}));
        }
        ensure(
            mutants.iter().any(|m| m["result"]["outcome"] == "OBSERVED"),
            format!("no observable mutant for {id}"),
        )?;
        results.push(json!({"operatorId":id,"category":case["category"],"baseline":baseline,"mutants":mutants}));
    }
    ensure(
        common::tree(&project)? == originals,
        "original semantic fixture changed",
    )?;
    drop(temporary);
    ensure(!project.exists(), "temporary fixture remains")?;
    ensure(
        tools
            .iter()
            .map(|p| common::hash(p))
            .collect::<Result<Vec<_>>>()?
            == hashes,
        "tool inputs changed",
    )?;
    common::write_json(
        &output.join("summary.json"),
        &json!({"allPassed":true,"operatorsTested":results.len(),"wallSeconds":start.elapsed().as_secs_f64(),"cases":results,"helper":receipt,"cleanup":steps["cleanup"],"originalBytesUnchanged":true,"temporaryFixtureCleaned":true,"toolInputsUnchanged":true}),
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_catalogue_contains_exact_19() {
        let value =
            common::read_json(&common::root().join("protocol/fixtures/dart-semantic-catalog.json"))
                .unwrap();
        assert_eq!(catalog(&value).unwrap().len(), 19);
        let mut bad = value.clone();
        bad["cases"][0]["category"] = "wrong".into();
        assert!(catalog(&bad).is_err());
        let mut bad = value;
        bad["cases"][0] = bad["cases"][1].clone();
        assert!(catalog(&bad).is_err());
    }
    #[test]
    fn diagnostic_marker_cannot_fake_observation() {
        let errors = b"compile failed: Bad state: MUTATION_OBSERVED\n";
        let receipt = format!(
            "failforge: cause=ProcessCompleted status=exit:1 cleanup=complete stdout_bytes=0 stderr_bytes={} stdout_issue=none stderr_issue=none\n",
            errors.len()
        );
        assert!(vm_outcome(1, b"", errors, receipt.as_bytes()).is_err());
        let errors = b"Unhandled exception:\nBad state: MUTATION_OBSERVED\n";
        let receipt = format!(
            "failforge: cause=ProcessCompleted status=exit:1 cleanup=complete stdout_bytes=0 stderr_bytes={} stdout_issue=none stderr_issue=none\n",
            errors.len()
        );
        assert_eq!(
            vm_outcome(1, b"", errors, receipt.as_bytes()).unwrap(),
            "OBSERVED"
        );
        assert!(vm_outcome(1, b"unexpected", errors, receipt.as_bytes()).is_err());
    }
    #[test]
    fn incomplete_transcript_rejected() {
        let t = common::Temporary::new("ff-transcript").unwrap();
        for raw in [
            "{}",
            "{\"step\":\"candidate\",\"payload\":{}}\n",
            "{\"step\":\"handshake\",\"payload\":{}}\n",
        ] {
            fs::write(t.0.join("transcript"), raw).unwrap();
            assert!(transcript(&t.0.join("transcript"), &t.0, "classic").is_err());
        }
    }
    #[test]
    fn transcript_input_must_be_a_regular_nonsymlink_file() {
        let temporary = common::Temporary::new("ff-transcript-input").unwrap();
        assert!(transcript(&temporary.0, &temporary.0, "classic").is_err());
        let real = temporary.0.join("source");
        fs::write(&real, b"{}\n").unwrap();
        let link = temporary.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(transcript(&link, &temporary.0, "classic").is_err());
    }
}
