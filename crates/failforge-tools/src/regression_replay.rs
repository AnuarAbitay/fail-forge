//! Real retained evidence replay through the same public native campaign owner.
use super::*;
use std::process::Command;

struct Archive {
    project: PathBuf,
    framework: PathBuf,
    transcript: PathBuf,
    baseline: PathBuf,
}

fn archive(harness: &Harness, project: PathBuf, seed: &Path, name: &str) -> Result<Archive> {
    let framework = fixture(
        &harness.root,
        &harness.output.join(format!("{name}-framework")),
        &fs::read_to_string(project.join("lib/value.dart"))?,
        &fs::read_to_string(project.join("test/value_test.dart"))?,
    )?;
    let retained = harness.output.join(format!("{name}-archive"));
    fs::create_dir(&retained)?;
    let baseline = common::read_json(&seed.join("baseline/result.json"))?;
    ensure(
        baseline["outcome"] == "SURVIVED",
        "replay seed baseline failed",
    )?;
    fs::copy(
        seed.join("baseline/capture/stdout.bin"),
        retained.join("stdout.bin"),
    )?;
    common::write_json(
        &retained.join("baseline.json"),
        &json!({
            "outputSource":"stdout.bin",
            "workingDirectory":baseline["isolatedWorkingDirectory"],
            "runnerExitCode":baseline["exitCode"],
            "runnerSummary":fs::read_to_string(seed.join("baseline/runner.stderr"))?,
            "completedTestIdentities":baseline["testIdentities"]
        }),
    )?;
    common::write_json(
        &retained.join("provenance.json"),
        &json!({"sourceFileHashes":failforge_workspace::source_hashes(
            &project, &failforge_workspace::Cancellation::default())?}),
    )?;
    common::write_json(
        &retained.join("dependency-result.json"),
        &json!({
            "generatedLockSha256":common::hash(&project.join("pubspec.lock"))?,
            "generatedPackageConfigSha256":common::hash(&project.join(".dart_tool/package_config.json"))?
        }),
    )?;
    let transcript = retained.join("transcript.jsonl");
    fs::copy(seed.join("discovery/adapter-session.jsonl"), &transcript)?;
    Ok(Archive {
        project,
        framework,
        transcript,
        baseline: retained.join("baseline.json"),
    })
}

fn replay_command(harness: &Harness, archive: &Archive, name: &str) -> Command {
    let mut command = harness.command();
    command
        .arg("replay")
        .arg("--installation")
        .arg(&harness.root)
        .arg("--project")
        .arg(&archive.project)
        .arg("--framework")
        .arg(&archive.framework)
        .arg("--transcript")
        .arg(&archive.transcript)
        .arg("--baseline")
        .arg(&archive.baseline)
        .arg("--output")
        .arg(harness.output.join(name))
        .arg("--dart-sdk")
        .arg(&harness.sdk)
        .args(["--workers", "1"]);
    command
}

impl Harness {
    fn checked_replay(&mut self, archive: &Archive, name: &str, expected: i32) -> Result<Value> {
        println!("regression {name}");
        let source = common::tree(&archive.project)?;
        let framework = common::tree(&archive.framework)?;
        let evidence = common::tree(archive.baseline.parent().unwrap())?;
        let logs = self.output.join(format!("{name}-command"));
        let receipt = common::execute(
            replay_command(self, archive, name).args([
                "--test-timeout",
                "2s",
                "--command-timeout",
                "30s",
            ]),
            &logs,
            Duration::from_secs(240),
            expected,
        )?;
        ensure(
            common::tree(&archive.project)? == source
                && common::tree(&archive.framework)? == framework
                && common::tree(archive.baseline.parent().unwrap())? == evidence,
            "replay changed retained inputs",
        )?;
        let output = self.output.join(name);
        if expected != 0 {
            ensure(!output.exists(), "invalid retained evidence created output")?;
            self.reports
                .push(json!({"name":name,"receipt":receipt,"rejectedBeforeOutput":true}));
            return Ok(Value::Null);
        }
        self.report(name, &output, &logs.join("stdout.log"), receipt)
    }

    pub(super) fn replay(&mut self) -> Result<()> {
        let project = fixture(
            &self.root,
            &self.output.join("replay-subject"),
            "bool enabled()=>true;\nbool offScope()=>true;\n",
            "import 'package:campaign_sample/value.dart';\nimport 'package:test/test.dart';\nvoid main(){test('selected author',()=>expect(enabled(),true));}\n",
        )?;
        let seed = self.direct(
            "replay-seed",
            &project,
            &[
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--method",
                "enabled",
            ],
            0,
        )?;
        expect_counts(&seed, json!({"KILLED":1}))?;
        let retained = archive(self, project, &self.output.join("replay-seed"), "replay")?;
        let report = self.checked_replay(&retained, "retained-replay", 0)?;
        expect_counts(&report, json!({"KILLED":1}))?;
        ensure(
            mutant_ids(&report) == mutant_ids(&seed)
                && report["staticValidationReused"] == true
                && report["retainedReplay"]["transcriptSha256"]
                    == common::hash(&retained.transcript)?,
            "replay did not reuse exactly the retained mutation evidence",
        )?;
        ensure(
            !self.output.join("retained-replay/discovery").exists(),
            "replay started an additional discovery session",
        )?;
        let mut copies = BTreeSet::new();
        for name in ["baseline", "original-before", "original-after"] {
            let control = common::read_json(
                &self
                    .output
                    .join("retained-replay")
                    .join(name)
                    .join("result.json"),
            )?;
            ensure(
                control["outcome"] == "SURVIVED"
                    && control["temporaryCopyCleaned"] == true
                    && control["namespaceIsolation"]["alias"] == "failforge_subject",
                "replay original control or forced isolation missing",
            )?;
            copies.insert(
                control["isolatedWorkingDirectory"]
                    .as_str()
                    .ok_or("missing control copy")?
                    .to_owned(),
            );
        }
        for record in report["records"].as_array().unwrap() {
            ensure(
                record["namespaceIsolation"]["alias"] == "failforge_subject",
                "replay mutant did not use forced namespace isolation",
            )?;
            copies.insert(
                record["isolatedWorkingDirectory"]
                    .as_str()
                    .ok_or("missing mutant copy")?
                    .to_owned(),
            );
        }
        ensure(copies.len() == 4, "replay reused an attempt copy")?;

        // Change one retained input at a time and restore it even when the
        // expected rejection fails; failed artifacts remain for diagnosis.
        for kind in ["source", "baseline", "transcript", "framework"] {
            let path = match kind {
                "source" => retained.project.join("lib/value.dart"),
                "framework" => retained.framework.join("lib/value.dart"),
                "baseline" => retained.baseline.clone(),
                _ => retained.transcript.clone(),
            };
            let original = fs::read(&path)?;
            let changed = match kind {
                "baseline" => {
                    let mut value = common::read_json(&path)?;
                    value["completedTestIdentities"] = json!([["test/value_test.dart", "foreign"]]);
                    serde_json::to_vec(&value)?
                }
                "transcript" => {
                    let raw = std::str::from_utf8(&original)?;
                    let mut lines = raw.lines().collect::<Vec<_>>();
                    lines.pop();
                    format!("{}\n", lines.join("\n")).into_bytes()
                }
                _ => [original.as_slice(), b"\n// changed retained input\n"].concat(),
            };
            fs::write(&path, changed)?;
            let result = self.checked_replay(&retained, &format!("replay-invalid-{kind}"), 125);
            fs::write(&path, original)?;
            result?;
        }
        self.cancel_replay()
    }

    fn cancel_replay(&mut self) -> Result<()> {
        let project = fixture(
            &self.root,
            &self.output.join("replay-cancel-subject"),
            "bool keepGoing()=>false;\n",
            LOOP_TEST,
        )?;
        let seed = self.direct(
            "replay-cancel-seed",
            &project,
            &[
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--test-timeout",
                "1s",
                "--command-timeout",
                "10s",
            ],
            0,
        )?;
        expect_counts(&seed, json!({"TIMEOUT":1}))?;
        let retained = archive(
            self,
            project,
            &self.output.join("replay-cancel-seed"),
            "replay-cancel",
        )?;
        let name = "replay-cancelled";
        let destination = self.output.join(name);
        let logs = self.output.join(format!("{name}-command"));
        fs::create_dir(&logs)?;
        let mut command = replay_command(self, &retained, name);
        command
            .args(["--test-timeout", "30s", "--command-timeout", "60s"])
            .stdout(File::create(logs.join("stdout.log"))?)
            .stderr(File::create(logs.join("stderr.log"))?);
        println!("regression {name}");
        let start = Instant::now();
        let mut owner = common::OwnedChild::new(command.spawn()?);
        while !author_started(&destination, "loop")? {
            common::check_interrupt()?;
            ensure(
                owner.0.try_wait()?.is_none() && start.elapsed() < Duration::from_secs(120),
                "retained replay did not reach its mutant author test",
            )?;
            std::thread::sleep(Duration::from_millis(25));
        }
        owner.signal(libc::SIGINT)?;
        let exit = owner.wait(Duration::from_secs(120))?;
        ensure(exit == 130, "retained replay cancellation exit mismatch")?;
        let report = self.report(
            name, &destination, &logs.join("stdout.log"),
            json!({"exitCode":exit,"wallSeconds":start.elapsed().as_secs_f64(),"signal":libc::SIGINT}),
        )?;
        expect_counts(&report, json!({"CANCELLED":1}))?;
        ensure(
            mutant_ids(&report) == mutant_ids(&seed)
                && report["staticValidationReused"] == true
                && report["status"] == "cancelled"
                && report["records"][0]["runner"]["cleanup"] == "complete",
            "retained replay cancellation lost candidate or cleanup evidence",
        )
    }
}
