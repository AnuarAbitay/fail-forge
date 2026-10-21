//! Prepared, offline public-entrypoint fixtures. Every command is sequential;
//! mutating SDK or adapter inputs while this harness runs invalidates evidence.
use crate::common::{self, Result, ensure};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "regression_replay.rs"]
mod replay;

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum Suite {
    All,
    Smoke,
    Profiles,
    Classification,
    Timeouts,
    Cancellation,
    Recovery,
    Applicability,
    Flutter,
    Replay,
}
#[derive(Args)]
pub struct Options {
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    dart_sdk: Option<PathBuf>,
    #[arg(long)]
    flutter_sdk: Option<PathBuf>,
    #[arg(long)]
    installation: Option<PathBuf>,
    /// Directory containing the exact native executables under test.
    #[arg(long)]
    binary_dir: Option<PathBuf>,
    #[arg(long,value_enum,default_value_t=Suite::All)]
    suite: Suite,
}
fn file_uri(path: &Path) -> Result<String> {
    let path = path.to_str().ok_or("non-UTF-8 prepared dependency path")?;
    let mut uri = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(uri)
}
fn fixture(root: &Path, path: &Path, source: &str, tests: &str) -> Result<PathBuf> {
    let adapter = root.join("adapters/dart");
    let config_path = adapter.join(".dart_tool/package_config.json");
    let roots = failforge_workspace::dependency_roots(&config_path)?;
    let mut config = common::read_json(&config_path)?;
    for package in config["packages"]
        .as_array_mut()
        .ok_or("missing prepared packages")?
    {
        let name = package["name"].as_str().ok_or("missing package name")?;
        package["rootUri"] = file_uri(roots.get(name).ok_or("unresolved package")?)?.into();
    }
    config["packages"].as_array_mut().unwrap().push(json!({"name":"campaign_sample","rootUri":"../","packageUri":"lib/","languageVersion":"3.13"}));
    let mut graph = common::read_json(&adapter.join(".dart_tool/package_graph.json"))?;
    graph["roots"] = json!(["campaign_sample"]);
    graph["packages"].as_array_mut().ok_or("missing prepared graph")?.push(json!({"name":"campaign_sample","version":"1.0.0","dependencies":[],"devDependencies":["test"]}));
    fs::create_dir_all(path.join("lib"))?;
    fs::create_dir(path.join("test"))?;
    common::write_json(&path.join(".dart_tool/package_config.json"), &config)?;
    common::write_json(&path.join(".dart_tool/package_graph.json"), &graph)?;
    fs::write(
        path.join("pubspec.yaml"),
        "name: campaign_sample\nenvironment:\n  sdk: '>=3.13.0 <3.14.0'\ndev_dependencies:\n  test: any\n",
    )?;
    fs::copy(adapter.join("pubspec.lock"), path.join("pubspec.lock"))?;
    fs::write(path.join("lib/value.dart"), source)?;
    fs::write(path.join("test/value_test.dart"), tests)?;
    Ok(path.canonicalize()?)
}
const SOURCE: &str = "bool enabled() => true;\nFuture<int> value() async => 7;\nString? semantic(String? value) => value ?? 'fallback';\n";
const TESTS: &str = "import 'package:campaign_sample/value.dart';\nimport 'package:test/test.dart';\nvoid main() {\n test('enabled', () => expect(enabled(), true));\n test('async value', () async => expect(await value(), 7));\n test('semantic fallback', () { expect(semantic(null), 'fallback'); expect(semantic('x'), 'x'); });\n}\n";
const LOOP_TEST: &str = "import 'package:campaign_sample/value.dart';\nimport 'package:test/test.dart';\nvoid main(){ test('loop', () { while (keepGoing()) {} expect(1,1); }); }\n";
fn profile() -> Value {
    json!({"mutations":{"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}},"failure_mode":{"enabled":true,"operators":{"EXCEPTION_AT_ENTRY":true}},"language_specific":{"enabled":true,"language":"dart","operators":{"NSM-001":true}}},"rules":{"include":["lib/value.dart"],"methods":["enabled","value","semantic"]},"workers":1,"timeouts":{"test":"2s","command":"30s"}})
}
pub(crate) fn summary_contract(summary: &Value, stdout: &[u8]) -> Result<()> {
    ensure(
        summary["version"] == if summary["kind"] == "profile" { 2 } else { 1 },
        "unexpected report schema",
    )?;
    let text = std::str::from_utf8(stdout)?;
    ensure(
        text.contains("FailForge mutation summary")
            || text.contains("FailForge mutation report")
            || text.contains("FailForge profile summary"),
        "missing terminal summary",
    )?;
    let output = summary["output"].as_str().ok_or("missing summary output")?;
    ensure(
        text.contains(&format!("{output}/summary.json")),
        "terminal report path mismatch",
    )?;
    let records = summary["records"]
        .as_array()
        .ok_or("missing summary records")?;
    let mut counts = serde_json::Map::<String, Value>::new();
    for record in records {
        let outcome = record["outcome"].as_str().ok_or("missing record outcome")?;
        let n = counts.get(outcome).and_then(Value::as_u64).unwrap_or(0) + 1;
        counts.insert(outcome.to_owned(), n.into());
    }
    ensure(
        summary["outcomeCounts"] == Value::Object(counts),
        "report counts differ from records",
    )?;
    ensure(
        summary["completedMutants"].as_u64() == Some(records.len() as u64),
        "processed count mismatch",
    )?;
    let complete = summary["complete"]
        .as_bool()
        .ok_or("missing completion flag")?;
    let status = summary["status"]
        .as_str()
        .ok_or("missing campaign status")?;
    ensure(
        if complete {
            matches!(status, "completed" | "no_mutants")
        } else {
            matches!(status, "failed" | "cancelled")
        },
        "completion and terminal status disagree",
    )?;
    ensure(
        summary["originalProjectFilesUnchanged"] == true
            && summary["temporarySnapshotCleaned"] == true
            && summary["workspaceCleanupDeferred"] == false,
        "regression report lacks clean original and workspace evidence",
    )?;
    if complete {
        ensure(
            summary["expectedMutants"].as_u64() == Some(records.len() as u64),
            "complete report lacks candidates",
        )?;
        ensure(
            (status == "no_mutants") == records.is_empty(),
            "empty completion status mismatch",
        )?;
    } else {
        for key in ["mutationScore", "failureModeScore", "languageSpecificScore"] {
            ensure(summary[key].is_null(), "partial report exposes a score")?;
        }
    }
    Ok(())
}
struct Harness {
    root: PathBuf,
    binary_dir: PathBuf,
    output: PathBuf,
    sdk: PathBuf,
    flutter: Option<PathBuf>,
    reports: Vec<Value>,
    temp_root: Option<PathBuf>,
}
impl Harness {
    fn command(&self) -> std::process::Command {
        let mut command = common::command(&self.binary_dir.join("failforge"));
        command
            .current_dir(&self.root)
            .env("XDG_CONFIG_HOME", self.output.join("tool-state"))
            .env("PUB_CACHE", self.root.join(".toolchain/pub-cache"));
        if let Some(root) = &self.temp_root {
            command.env("TMPDIR", root);
        }
        command
    }
    fn direct(
        &mut self,
        name: &str,
        project: &Path,
        args: &[&str],
        expected: i32,
    ) -> Result<Value> {
        let before = common::tree(project)?;
        let destination = self.output.join(name);
        let mut command = self.command();
        command
            .arg("mutate")
            .arg("--installation")
            .arg(&self.root)
            .arg("--project")
            .arg(project)
            .args(args)
            .arg("--output")
            .arg(&destination);
        if !args.contains(&"--flutter-sdk") && !args.contains(&"--dart-sdk") {
            command.arg("--dart-sdk").arg(&self.sdk);
        }
        if !args.contains(&"--test-timeout") {
            command.args(["--test-timeout", "2s", "--command-timeout", "30s"]);
        }
        println!("regression {name}");
        let executed = common::execute(
            &mut command,
            &self.output.join(format!("{name}-command")),
            Duration::from_secs(240),
            expected,
        );
        let receipt = match executed {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Ok(summary) = common::read_json(&destination.join("summary.json")) {
                    return Err(format!(
                        "{error}; campaign diagnostics: {}",
                        failure_diagnostics(&summary)
                    )
                    .into());
                }
                return Err(error);
            }
        };
        ensure(
            common::tree(project)? == before,
            format!("{name}: source inputs changed"),
        )?;
        self.report(
            name,
            &destination,
            &self.output.join(format!("{name}-command/stdout.log")),
            receipt,
        )
    }
    fn report(
        &mut self,
        name: &str,
        destination: &Path,
        stdout: &Path,
        receipt: Value,
    ) -> Result<Value> {
        let summary = common::read_json(&destination.join("summary.json"))?;
        summary_contract(&summary, &fs::read(stdout)?)?;
        let progress = common::read_json(&destination.join("progress.json"))?;
        ensure(
            progress["status"] == "finalizing"
                && progress["intendedStatus"] == summary["status"]
                && progress["complete"] == false
                && progress["outcomeCounts"] == summary["outcomeCounts"],
            "final compact progress mismatch",
        )?;
        self.reports.push(json!({"name":name,"receipt":receipt,"status":summary["status"],"outcomeCounts":summary["outcomeCounts"],"summary":destination.join("summary.json")}));
        Ok(summary)
    }
    fn configured(
        &mut self,
        name: &str,
        project: &Path,
        mut settings: Value,
        expected: i32,
    ) -> Result<Value> {
        let destination = self.output.join(name);
        settings["output"] = serde_json::to_value(&destination)?;
        settings["dart_sdk"] = serde_json::to_value(&self.sdk)?;
        let config = project
            .join("profiles with spaces")
            .join(format!("{name}.yml"));
        if !config.is_file() || common::read_json(&config)? != settings {
            common::write_json(&config, &settings)?;
        }
        let before = common::tree(project)?;
        let caller = common::Temporary::new("ff-profile-caller")?;
        fs::write(caller.0.join("failforge.yaml"), "not: a profile\n")?;
        let mut command = self.command();
        command
            .current_dir(&caller.0)
            .arg("mutate")
            .arg("--installation")
            .arg(&self.root)
            .arg("--config")
            .arg(config);
        println!("regression {name}");
        let receipt = common::execute(
            &mut command,
            &self.output.join(format!("{name}-command")),
            Duration::from_secs(300),
            expected,
        )?;
        ensure(
            common::tree(project)? == before,
            format!("{name}: source inputs changed"),
        )?;
        if expected == 2 {
            ensure(
                !destination.exists(),
                "invalid profile created report output",
            )?;
            self.reports
                .push(json!({"name":name,"receipt":receipt,"rejectedBeforeOutput":true}));
            return Ok(Value::Null);
        }
        let summary = self.report(
            name,
            &destination,
            &self.output.join(format!("{name}-command/stdout.log")),
            receipt,
        )?;
        for (family, row) in summary["families"]
            .as_object()
            .ok_or("missing family summaries")?
        {
            if row["enabled"] != true {
                continue;
            }
            let child = common::read_json(
                &destination
                    .join("families")
                    .join(family)
                    .join("summary.json"),
            )?;
            ensure(
                child == row["summary"] && child["profileComplete"] == summary["complete"],
                "child report commit mismatch",
            )?;
        }
        Ok(summary)
    }
    fn smoke(&mut self, project: &Path) -> Result<()> {
        let direct = self.direct(
            "direct",
            project,
            &["--family", "classic", "--operator", "BOOLEAN_LITERAL_FLIP"],
            0,
        )?;
        expect_counts(&direct, json!({"KILLED":1}))?;
        let mut settings = profile();
        settings["mutations"]
            .as_object_mut()
            .unwrap()
            .retain(|k, _| k == "classic");
        let configured = self.configured("yaml", project, settings, 0)?;
        expect_counts(&configured, json!({"KILLED":1}))?;
        ensure(
            mutant_ids(&direct) == mutant_ids(&configured),
            "YAML and direct mutant identity mismatch",
        )?;
        let empty = self.direct(
            "no-mutants",
            project,
            &[
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--method",
                "noSuchMethod",
            ],
            0,
        )?;
        ensure(
            empty["status"] == "no_mutants"
                && empty["complete"] == true
                && empty["records"] == json!([]),
            "empty campaign is not complete no_mutants",
        )?;
        let failure = self.direct(
            "failure-mode",
            project,
            &[
                "--family",
                "failure_mode",
                "--operator",
                "EXCEPTION_AT_ENTRY",
                "--method",
                "value",
            ],
            0,
        )?;
        expect_counts(&failure, json!({"KILLED":1}))?;
        full_score(&failure["failureModeScore"])?;
        ensure(
            failure["mutationScore"].is_null(),
            "failure score family mismatch",
        )?;
        let semantic = self.direct(
            "language-specific",
            project,
            &[
                "--family",
                "language_specific",
                "--language",
                "dart",
                "--operator",
                "NSM-001",
                "--method",
                "semantic",
            ],
            0,
        )?;
        expect_counts(&semantic, json!({"KILLED":1}))?;
        full_score(&semantic["languageSpecificScore"])?;
        let parallel = fixture(
            &self.root,
            &self.output.join("parallel-subject"),
            &format!("{SOURCE}bool untested() => false;\n"),
            TESTS,
        )?;
        let summary = self.direct(
            "parallel",
            &parallel,
            &[
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--workers",
                "2",
            ],
            0,
        )?;
        expect_counts(&summary, json!({"KILLED":1,"SURVIVED":1}))?;
        let failed = fixture(
            &self.root,
            &self.output.join("failed-subject"),
            &SOURCE.replace("enabled() => true", "enabled() => false"),
            TESTS,
        )?;
        let summary = self.direct(
            "failed-baseline",
            &failed,
            &["--family", "classic", "--operator", "BOOLEAN_LITERAL_FLIP"],
            125,
        )?;
        ensure(
            summary["status"] == "failed"
                && summary["complete"] == false
                && summary["records"] == json!([])
                && summary["baseline"]["outcome"] == "KILLED",
            "failed baseline contract",
        )?;
        Ok(())
    }
    fn profiles(&mut self, project: &Path) -> Result<()> {
        let all = self.configured("all-families", project, profile(), 0)?;
        ensure(
            all["complete"] == true && all["status"] == "completed",
            "full profile incomplete",
        )?;
        let expected = all["expectedMutants"].as_u64().ok_or("missing expected")?;
        expect_counts(&all, json!({"KILLED":expected}))?;
        for mode in ["omitted", "disabled", "missing-enabled", "operators-off"] {
            let mut settings = profile();
            let active = if mode == "omitted" {
                settings["mutations"]
                    .as_object_mut()
                    .unwrap()
                    .retain(|k, _| k == "classic");
                "classic"
            } else {
                for family in ["classic", "language_specific"] {
                    match mode {
                        "disabled" => settings["mutations"][family]["enabled"] = false.into(),
                        "missing-enabled" => {
                            settings["mutations"][family]
                                .as_object_mut()
                                .unwrap()
                                .remove("enabled");
                        }
                        _ => {
                            settings["mutations"][family]["operators"]
                                .as_object_mut()
                                .unwrap()
                                .values_mut()
                                .for_each(|v| *v = false.into());
                        }
                    }
                }
                "failure_mode"
            };
            let report = self.configured(mode, project, settings, 0)?;
            ensure(
                report["families"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(_, v)| v["enabled"] == true)
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>()
                    == vec![active],
                "disabled family executed",
            )?;
        }
        let mut cap = profile();
        cap["limits"] = json!({"max_mutants":0});
        let failed = self.configured("global-cap", project, cap, 125)?;
        ensure(
            failed["records"] == json!([]) && failed["complete"] == false,
            "global cap executed mutants",
        )?;
        let mut zero = profile();
        zero["rules"]["methods"] = json!(["noSuchMethod"]);
        let empty = self.configured("profile-zero", project, zero, 0)?;
        ensure(
            empty["status"] == "no_mutants" && empty["expectedMutants"] == 0,
            "zero profile mismatch",
        )?;
        let mut invalid = Vec::new();
        for enabled in [true, false] {
            let mut s = profile();
            s["mutations"]["classic"]["operators"]["UNKNOWN"] = enabled.into();
            invalid.push((format!("unknown-{enabled}"), s));
        }
        let mut s = profile();
        s["mutations"]["language_specific"]
            .as_object_mut()
            .unwrap()
            .remove("language");
        s["mutations"]["language_specific"]["enabled"] = false.into();
        invalid.push(("language-required".into(), s));
        let mut s = profile();
        for entry in s["mutations"].as_object_mut().unwrap().values_mut() {
            entry["enabled"] = false.into();
        }
        invalid.push(("all-disabled".into(), s));
        for (key, value) in [("version", json!(2)), ("project", json!(project))] {
            let mut s = profile();
            s[key] = value;
            invalid.push((format!("rejected-{key}"), s));
        }
        for literal in [json!("true"), json!(1), json!("on"), json!("yes")] {
            let mut s = profile();
            s["mutations"]["classic"]["enabled"] = literal.clone();
            invalid.push((format!("invalid-boolean-{}", invalid.len()), s));
        }
        let mut s = profile();
        s["rules"]["include"] = json!(["lib/missing.dart"]);
        invalid.push(("missing-include".into(), s));
        for (name, settings) in invalid {
            self.configured(&name, project, settings, 2)?;
        }
        Ok(())
    }
    fn classification(&mut self) -> Result<()> {
        let imports =
            "import 'package:campaign_sample/value.dart';\nimport 'package:test/test.dart';\n";
        let registration = fixture(
            &self.root,
            &self.output.join("registration-subject"),
            "String? option([int start = 0]) => start == 0 ? 'value' : null;\n",
            &format!(
                "{imports}void main() {{ final value=option()!; test('registered value',()=>expect(value,'value')); }}\n"
            ),
        )?;
        let confirmed = self.direct(
            "registration-confirmation",
            &registration,
            &[
                "--family",
                "language_specific",
                "--language",
                "dart",
                "--operator",
                "DPM-001",
                "--method",
                "option",
            ],
            0,
        )?;
        expect_counts(&confirmed, json!({"KILLED":2}))?;
        for record in confirmed["records"].as_array().unwrap() {
            ensure(
                record["reason"] == "mutation_caused_initialization_failure"
                    && record["initializationConfirmation"]["confirmed"] == true,
                "registration failure unconfirmed",
            )?;
            let attempts = record["initializationConfirmation"]["attempts"]
                .as_array()
                .ok_or("missing controls")?;
            ensure(
                attempts
                    .iter()
                    .map(|a| a["role"].as_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    == ["original-before", "mutant-replay", "original-after"]
                    && attempts.iter().all(|a| a["temporaryCopyCleaned"] == true),
                "registration replay order or cleanup invalid",
            )?;
        }
        for witness in [true, false] {
            let source = "bool ready() => true;\nvoid requireReady(){ if(!ready()) throw StateError('setup failed'); }\n";
            let tests = format!(
                "{imports}void main() {{ {} group('setup',(){{setUpAll(()=>requireReady());tearDownAll((){{}});group('nested',(){{test('unstarted',()=>expect(ready(),true));}});}});}}\n",
                if witness {
                    "test('ordinary witness',()=>expect(ready(),true));"
                } else {
                    ""
                }
            );
            let name = if witness {
                "setup-witness"
            } else {
                "setup-only"
            };
            let project = fixture(
                &self.root,
                &self.output.join(format!("{name}-subject")),
                source,
                &tests,
            )?;
            let report = self.direct(
                name,
                &project,
                &[
                    "--family",
                    "classic",
                    "--operator",
                    "BOOLEAN_LITERAL_FLIP",
                    "--method",
                    "ready",
                ],
                if witness { 0 } else { 125 },
            )?;
            expect_counts(
                &report,
                if witness {
                    json!({"KILLED":1})
                } else {
                    json!({"UNCLASSIFIED":1})
                },
            )?;
            let record = &report["records"][0];
            ensure(
                record["reason"]
                    == if witness {
                        "ordinary_author_failure_with_setup_suppression"
                    } else {
                        "author_test_catalogue_incomplete_after_setup_failure"
                    },
                "setup classification reason mismatch",
            )?;
            ensure(
                record["testCatalogueSuppression"]["catalogueIdentityProven"] == false,
                "suppressed catalogue falsely complete",
            )?;
        }
        Ok(())
    }
    fn timeouts(&mut self) -> Result<()> {
        let inner = fixture(
            &self.root,
            &self.output.join("inner-timeout-subject"),
            "bool waiting() => false;\n",
            "import 'dart:async';\nimport 'package:campaign_sample/value.dart';\nimport 'package:test/test.dart';\nvoid main(){test('subject completes',()async{if(waiting()) await Completer<void>().future;expect(waiting(),false);});test('later assertion',()=>expect(2+2,4));}\n",
        )?;
        let report = self.direct(
            "inner-timeout",
            &inner,
            &[
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--test-timeout",
                "1s",
                "--command-timeout",
                "15s",
            ],
            0,
        )?;
        expect_counts(&report, json!({"KILLED":1}))?;
        let row = &report["records"][0];
        ensure(
            row["reason"] == "test_timeout"
                && row["reporterComplete"] == true
                && row["tests"]["failed"] == 1
                && row["tests"]["passed"] == 1
                && report["mutationScore"]["denominator"] == 1,
            "internal timeout classified incorrectly",
        )?;
        let outer = fixture(
            &self.root,
            &self.output.join("outer-timeout-subject"),
            "bool keepGoing() => false;\n",
            LOOP_TEST,
        )?;
        let report = self.direct(
            "outer-timeout",
            &outer,
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
        expect_counts(&report, json!({"TIMEOUT":1}))?;
        ensure(
            report["mutationScore"]["denominator"] == 0
                && report["records"][0]["runner"]["cleanup"] == "complete",
            "outer timeout fabricated kill or incomplete cleanup",
        )?;
        Ok(())
    }
    fn cancellation(&mut self) -> Result<()> {
        let project = fixture(
            &self.root,
            &self.output.join("cancel-subject"),
            "bool keepGoing() => false;\n",
            LOOP_TEST,
        )?;
        for (signal, code, name) in [
            (libc::SIGINT, 130, "cancel-int"),
            (libc::SIGTERM, 143, "cancel-term"),
        ] {
            let destination = self.output.join(name);
            let logs = self.output.join(format!("{name}-command"));
            fs::create_dir(&logs)?;
            let mut cmd = self.command();
            cmd.arg("mutate")
                .arg("--installation")
                .arg(&self.root)
                .arg("--project")
                .arg(&project)
                .arg("--dart-sdk")
                .arg(&self.sdk)
                .args([
                    "--family",
                    "classic",
                    "--operator",
                    "BOOLEAN_LITERAL_FLIP",
                    "--test-timeout",
                    "30s",
                    "--command-timeout",
                    "60s",
                ])
                .arg("--output")
                .arg(&destination)
                .stdout(File::create(logs.join("stdout.log"))?)
                .stderr(File::create(logs.join("stderr.log"))?);
            println!("regression {name}");
            let start = Instant::now();
            let mut owner = common::OwnedChild::new(cmd.spawn()?);
            let mut observed = false;
            while start.elapsed() < Duration::from_secs(120) && owner.0.try_wait()?.is_none() {
                common::check_interrupt()?;
                if author_started(&destination, "loop")? {
                    observed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            ensure(observed, "cancel fixture did not start author test")?;
            owner.signal(signal)?;
            let actual = owner.wait(Duration::from_secs(120))?;
            ensure(actual == code, "cancellation exit mismatch")?;
            let report=self.report(name,&destination,&logs.join("stdout.log"),json!({"exitCode":actual,"wallSeconds":start.elapsed().as_secs_f64(),"signal":signal}))?;
            expect_counts(&report, json!({"CANCELLED":1}))?;
            ensure(
                report["status"] == "cancelled"
                    && report["complete"] == false
                    && report["temporarySnapshotCleaned"] == true
                    && report["records"][0]["runner"]["cleanup"] == "complete",
                "cancellation lost cleanup",
            )?;
        }
        Ok(())
    }

    fn recovery(&mut self) -> Result<()> {
        let project = fixture(
            &self.root,
            &self.output.join("recovery-subject"),
            "bool keepGoing() => false;\n",
            LOOP_TEST,
        )?;
        // Retain this directory on a failed proof: an owner may still be
        // draining descendants and holding a lease after its controller died.
        let temporary = recovery_temporary()?;
        let temp_root = temporary.0.clone();
        std::mem::forget(temporary);
        fs::set_permissions(&temp_root, fs::Permissions::from_mode(0o700))?;
        self.temp_root = Some(temp_root.clone());
        let registry = temp_root.join(format!("failforge-workspaces-{}", unsafe {
            libc::geteuid()
        }));
        let destination = self.output.join("controller-crash");
        let logs = self.output.join("controller-crash-command");
        fs::create_dir(&logs)?;
        let mut empty = profile();
        empty["mutations"]
            .as_object_mut()
            .unwrap()
            .retain(|key, _| key == "classic");
        empty["rules"]["methods"] = json!(["noSuchMethod"]);
        // Freeze every profile input before the first owner's binding. The
        // concurrent active-lease probe must not alter its original project.
        for name in [
            "active-owner-preserved",
            "controller-death-recovered",
            "stale-lease-recovered",
        ] {
            let mut settings = empty.clone();
            settings["output"] = serde_json::to_value(self.output.join(name))?;
            settings["dart_sdk"] = serde_json::to_value(&self.sdk)?;
            common::write_json(
                &project
                    .join("profiles with spaces")
                    .join(format!("{name}.yml")),
                &settings,
            )?;
        }
        let mut cmd = self.command();
        cmd.arg("mutate")
            .arg("--installation")
            .arg(&self.root)
            .arg("--project")
            .arg(&project)
            .arg("--dart-sdk")
            .arg(&self.sdk)
            .args([
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--test-timeout",
                "1s",
                "--command-timeout",
                "60s",
            ])
            .arg("--output")
            .arg(&destination)
            .stdout(File::create(logs.join("stdout.log"))?)
            .stderr(File::create(logs.join("stderr.log"))?);
        let start = Instant::now();
        let mut controller = common::OwnedChild::new(cmd.spawn()?);
        while !author_started(&destination, "loop")? {
            common::check_interrupt()?;
            ensure(
                controller.0.try_wait()?.is_none() && start.elapsed() < Duration::from_secs(120),
                "recovery fixture did not start its author test",
            )?;
            std::thread::sleep(Duration::from_millis(25));
        }
        let campaigns = fs::read_dir(&registry)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("campaign-"))
            })
            .collect::<Vec<_>>();
        ensure(
            campaigns.len() == 1,
            "expected exactly one leased recovery campaign",
        )?;
        let campaign = &campaigns[0];
        let name = campaign.file_name().unwrap().to_string_lossy().into_owned();
        // Cleanup renames campaign-* to pending-* before removing its tree.
        // Keep the lease descriptor so disappearance of the original pathname
        // cannot be mistaken for release of the live owner's lease.
        let lease = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(campaign.join("lease"))?;
        ensure(
            lease_busy(&lease)?,
            "active author test has no workspace lease",
        )?;
        // An active owner must be protected independently of controller death.
        let active = self.configured("active-owner-preserved", &project, empty.clone(), 0)?;
        ensure(
            active["workspaceRecovery"]["skipped"]
                .as_array()
                .is_some_and(|rows| {
                    rows.iter()
                        .any(|row| row["name"] == name && row["reason"] == "active_lease")
                }),
            "new campaign did not preserve an active workspace lease",
        )?;
        ensure(
            campaign.is_dir() && lease_busy(&lease)?,
            "live workspace was removed",
        )?;
        controller.signal(libc::SIGKILL)?;
        ensure(
            controller.wait(Duration::from_secs(5))? == -libc::SIGKILL,
            "controller was not killed",
        )?;
        // The reaper reacquires the released lease while completing cleanup.
        // Probe only after its final report: probing in that handoff could
        // momentarily take the lock and make a correct reaper defer cleanup.
        wait_watchdog_summary(&destination.join("summary.json"), Duration::from_secs(90))?;
        ensure(
            !lease_busy(&lease)?,
            "surviving owner retained its lease after reporting cleanup",
        )?;
        // The surviving native owner normally cleans its own workspace.
        // A pathname observed before this command cannot establish which owner
        // performed recovery; the deterministic stale lease below proves that.
        self.configured("controller-death-recovered", &project, empty.clone(), 0)?;
        ensure(!campaign.exists(), "controller-death workspace remains")?;
        // Exercise deterministic stale-directory recovery even when the native
        // watchdog has already cleaned the real crashed campaign.
        let cancel = failforge_workspace::Cancellation::default();
        let abandoned = failforge_workspace::lifecycle::start(&registry, &cancel)?;
        let abandoned_name = abandoned.campaign_name.clone();
        let allocation = failforge_workspace::lifecycle::allocate(
            &registry,
            &abandoned_name,
            abandoned.caps.try_clone()?,
            "snapshot",
            &cancel,
        )?;
        fs::write(allocation.path.join("fixture"), b"abandoned")?;
        drop(abandoned);
        let recovered = self.configured("stale-lease-recovered", &project, empty, 0)?;
        ensure(
            recovered["workspaceRecovery"]["recovered"]
                .as_array()
                .is_some_and(|rows| rows.contains(&json!(abandoned_name))),
            "released stale lease not recovered",
        )?;
        ensure(
            !registry.join(&abandoned_name).exists(),
            "stale directory remains",
        )?;
        ensure(
            destination.join("manifest.json").is_file() && author_started(&destination, "loop")?,
            "crash evidence was removed",
        )?;
        for entry in fs::read_dir(&registry)? {
            let name = entry?.file_name();
            ensure(
                name == ".global.lock" || name == ".owner",
                "recovery registry still has a campaign or pending cleanup",
            )?;
        }
        self.reports.push(
            json!({"name":"controller-sigkill","exitCode":-libc::SIGKILL,
            "activeNativeOwnerProtected":true,"nativeOwnerDrained":true,
            "watchdogSummary":destination.join("summary.json"),
            "staleLeaseRecovered":true,"registryClean":true,"reportsPreserved":true}),
        );
        self.temp_root = None;
        fs::remove_dir_all(temp_root)?;
        Ok(())
    }
    fn applicability(&mut self) -> Result<()> {
        let marker = self.output.join("author-executed.marker");
        let source = "bool enabled()=>true;\nint length(String? value){if(value!=null)return value.length;return 0;}\n";
        let tests = format!(
            "import 'dart:io';\nvoid main(){{File({}).writeAsStringSync('executed');throw StateError('must not run');}}\n",
            serde_json::to_string(&marker.to_string_lossy())?
        );
        let project = fixture(
            &self.root,
            &self.output.join("applicability-subject"),
            source,
            &tests,
        )?;
        fs::write(
            project.join("lib/other.dart"),
            "bool elsewhere(int x,int y)=>x<y;\n",
        )?;
        for (name, operators, expected) in [
            (
                "valid-invalid-absent",
                vec![
                    "BOOLEAN_LITERAL_FLIP",
                    "RELATIONAL_BOUNDARY",
                    "CONDITIONAL_NEGATION",
                ],
                0,
            ),
            ("all-false", vec!["RELATIONAL_BOUNDARY"], 2),
        ] {
            let target = project.join("profiles").join(format!("{name}.yml"));
            let before = common::tree(&project)?;
            let mut cmd = self.command();
            cmd.args(["profile", "generate", "--installation"])
                .arg(&self.root)
                .arg("--file")
                .arg(project.join("lib/value.dart"))
                .arg("--output")
                .arg(&target)
                .arg("--dart-sdk")
                .arg(&self.sdk)
                .args(["--family", "classic", "--dry-run"]);
            for operator in operators {
                cmd.arg("--operator").arg(operator);
            }
            let logs = self.output.join(format!("{name}-command"));
            let receipt = common::execute(&mut cmd, &logs, Duration::from_secs(180), expected)?;
            ensure(
                !marker.exists()
                    && !target.exists()
                    && !project.join(".failforge").exists()
                    && common::tree(&project)? == before,
                "generation executed tests or wrote source/report",
            )?;
            if expected == 0 {
                let value = failforge_config::yaml::parse_yaml(&fs::read_to_string(
                    logs.join("stdout.log"),
                )?)?;
                let map = value["mutations"]["classic"]["operators"]
                    .as_object()
                    .ok_or("missing profile operators")?;
                let enabled: BTreeSet<_> = map
                    .iter()
                    .filter(|(_, v)| v == &&Value::Bool(true))
                    .map(|(k, _)| k.as_str())
                    .collect();
                ensure(
                    enabled == BTreeSet::from(["BOOLEAN_LITERAL_FLIP"]),
                    "invalid or absent operator enabled",
                )?;
                ensure(
                    map.len() == failforge_config::catalogue::CLASSIC_OPERATORS.len(),
                    "generated operator catalogue incomplete",
                )?;
            }
            self.reports
                .push(json!({"name":name,"receipt":receipt,"authorTestsExecuted":false}));
        }
        let unprepared = self.output.join("unprepared");
        fs::create_dir_all(unprepared.join("lib"))?;
        fs::write(unprepared.join("pubspec.yaml"), "name: unprepared\n")?;
        fs::write(unprepared.join("lib/value.dart"), "bool enabled()=>true;\n")?;
        for template in [false, true] {
            let name = if template {
                "unprepared-template"
            } else {
                "unprepared-analysis"
            };
            let target = unprepared.join(format!("{name}.yml"));
            let mut cmd = self.command();
            cmd.args(["profile", "generate", "--installation"])
                .arg(&self.root)
                .arg("--file")
                .arg(unprepared.join("lib/value.dart"))
                .args(["--family", "classic", "--output"])
                .arg(&target);
            if template {
                cmd.arg("--template");
            }
            let receipt = common::execute(
                &mut cmd,
                &self.output.join(format!("{name}-command")),
                Duration::from_secs(30),
                if template { 0 } else { 2 },
            )?;
            ensure(
                target.exists() == template && !unprepared.join(".dart_tool").exists(),
                "template readiness side effect",
            )?;
            self.reports.push(json!({"name":name,"receipt":receipt}));
        }
        Ok(())
    }
    fn flutter(&mut self) -> Result<()> {
        let sdk = self
            .flutter
            .clone()
            .ok_or("--flutter-sdk required for Flutter suite")?;
        let project = self.root.join("examples/flutter_fixture");
        let path = sdk.to_str().ok_or("non-UTF-8 Flutter path")?;
        for (name, family, operator, method, outcome) in [
            (
                "flutter-classic-killed",
                "classic",
                "RELATIONAL_BOUNDARY",
                "accepts",
                "KILLED",
            ),
            (
                "flutter-classic-survived",
                "classic",
                "BOOLEAN_LITERAL_FLIP",
                "untestedFlag",
                "SURVIVED",
            ),
            (
                "flutter-promotion-invalid",
                "classic",
                "CONDITIONAL_NEGATION",
                "nullableLength",
                "INVALID",
            ),
            (
                "flutter-entry",
                "failure_mode",
                "EXCEPTION_AT_ENTRY",
                "enabled",
                "KILLED",
            ),
            (
                "flutter-async-entry",
                "failure_mode",
                "EXCEPTION_AT_ENTRY",
                "asyncEnabled",
                "KILLED",
            ),
        ] {
            let report = self.direct(
                name,
                &project,
                &[
                    "--family",
                    family,
                    "--operator",
                    operator,
                    "--method",
                    method,
                    "--flutter-sdk",
                    path,
                    "--test-timeout",
                    "10s",
                    "--command-timeout",
                    "60s",
                ],
                0,
            )?;
            expect_counts(&report, json!({outcome:1}))?;
        }
        let summary = self.direct(
            "flutter-channel",
            &project,
            &[
                "--family",
                "failure_mode",
                "--operator",
                "METHOD_CHANNEL_ERROR",
                "--method",
                "readValue",
                "--flutter-sdk",
                path,
                "--test-timeout",
                "10s",
                "--command-timeout",
                "60s",
            ],
            0,
        )?;
        expect_counts(&summary, json!({"KILLED":2}))?;
        ensure(
            summary["failureModeScore"]["denominator"] == 2,
            "Flutter score mismatch",
        )?;
        Ok(())
    }
}

fn failure_diagnostics(summary: &Value) -> Value {
    fn attempt(value: &Value) -> Value {
        json!({"outcome":value["outcome"],"reason":value["reason"],
            "wallSeconds":value["wallSeconds"],"exitCode":value["exitCode"],
            "runner":{"cause":value["runner"]["cause"],"cleanup":value["runner"]["cleanup"],"status":value["runner"]["status"]}})
    }
    let records = summary["records"].as_array();
    json!({"status":summary["status"],"error":summary["error"],
        "outcomeCounts":summary["outcomeCounts"],"timings":summary["timings"],
        "baseline":attempt(&summary["baseline"]),
        "originalControls":summary["originalControls"].as_array().into_iter().flatten().take(8).map(attempt).collect::<Vec<_>>(),
        "recordCount":records.map(Vec::len),
        "records":records.into_iter().flatten().take(8).map(attempt).collect::<Vec<_>>()})
}
fn mutant_ids(summary: &Value) -> Vec<&str> {
    summary["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["mutantId"].as_str())
        .collect()
}

fn full_score(score: &Value) -> Result<()> {
    let denominator = score["denominator"]
        .as_u64()
        .ok_or("missing score denominator")?;
    ensure(
        denominator > 0
            && score["killed"].as_u64() == Some(denominator)
            && score["survived"] == 0
            && score["percent"].as_f64() == Some(100.0),
        "expected a complete 100 percent score",
    )
}

fn recovery_temporary() -> Result<common::Temporary> {
    // macOS sockaddr_un paths are limited to 104 bytes; an inherited TMPDIR
    // plus a nested registry can exceed the limit before the SDK creates one.
    common::Temporary::new_in(Path::new("/tmp"), "ff-ws")
}

fn lease_busy(lease: &File) -> Result<bool> {
    let result = unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(true)
    } else {
        Err(error.into())
    }
}
fn watchdog_complete(summary: &Value) -> Result<()> {
    ensure(
        summary["status"] == "cancelled"
            && summary["complete"] == false
            && summary["originalProjectFilesUnchanged"] == true
            && summary["temporarySnapshotCleaned"] == true
            && summary["workspaceCleanupDeferred"] == false,
        "watchdog did not publish cancelled completion with clean workspaces",
    )?;
    expect_counts(summary, json!({"CANCELLED":1}))
}
fn wait_watchdog_summary(path: &Path, budget: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        common::check_interrupt()?;
        match fs::read(path) {
            Ok(bytes) => {
                let summary = failforge_workspace::parse_json(&bytes)?;
                // The product publishes an initial running summary before
                // execution and atomically replaces it at final commitment.
                if summary["status"] != "running" {
                    return watchdog_complete(&summary);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure(
            start.elapsed() < budget,
            "surviving owner did not publish its final cleanup report",
        )?;
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn expect_counts(summary: &Value, counts: Value) -> Result<()> {
    ensure(
        summary["outcomeCounts"] == counts,
        format!(
            "unexpected mutation outcomes: {} != {counts}",
            summary["outcomeCounts"]
        ),
    )
}
fn author_started(root: &Path, name: &str) -> Result<bool> {
    fn visit(path: &Path, name: &str) -> Result<bool> {
        if !path.is_dir() {
            return Ok(false);
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if visit(&entry.path(), name)? {
                    return Ok(true);
                }
            } else if entry.file_name() == "stdout.bin" {
                for line in fs::read(entry.path())?.split(|b| *b == b'\n') {
                    if let Ok(row) = serde_json::from_slice::<Value>(line) {
                        if row["type"] == "testStart" && row["test"]["name"] == name {
                            return Ok(true);
                        }
                    }
                }
            }
        }
        Ok(false)
    }
    visit(&root.join("mutants"), name)
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
    let binary_dir = options
        .binary_dir
        .unwrap_or_else(|| root.join("target/debug"))
        .canonicalize()?;
    common::create_output(&options.output)?;
    let output = options.output.canonicalize()?;
    let mut harness = Harness {
        root,
        binary_dir,
        output,
        sdk,
        flutter: options.flutter_sdk,
        reports: Vec::new(),
        temp_root: None,
    };
    let start = Instant::now();
    let source = fixture(
        &harness.root,
        &harness.output.join("author project with spaces"),
        SOURCE,
        TESTS,
    )?;
    let all = options.suite == Suite::All;
    if all || options.suite == Suite::Smoke {
        harness.smoke(&source)?;
    }
    if all || options.suite == Suite::Profiles {
        harness.profiles(&source)?;
    }
    if all || options.suite == Suite::Classification {
        harness.classification()?;
    }
    if all || options.suite == Suite::Timeouts {
        harness.timeouts()?;
    }
    if all || options.suite == Suite::Cancellation {
        harness.cancellation()?;
    }
    if all || options.suite == Suite::Recovery {
        harness.recovery()?;
    }
    if all || options.suite == Suite::Applicability {
        harness.applicability()?;
    }
    if options.suite == Suite::Flutter || (all && harness.flutter.is_some()) {
        harness.flutter()?;
    }
    if all || options.suite == Suite::Replay {
        harness.replay()?;
    }
    common::write_json(
        &harness.output.join("summary.json"),
        &json!({"allPassed":true,"sequentialCommands":true,"cases":harness.reports,"wallSeconds":start.elapsed().as_secs_f64()}),
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uri_preserves_native_bytes() {
        assert_eq!(
            file_uri(Path::new("/a b/c#d%é")).unwrap(),
            "file:///a%20b/c%23d%25%C3%A9"
        );
    }
    #[test]
    fn recovery_uses_short_private_root_for_native_sockets() {
        let temporary = recovery_temporary().unwrap();
        assert_eq!(
            temporary.0.parent().unwrap(),
            Path::new("/tmp").canonicalize().unwrap()
        );
        assert!(temporary.0.as_os_str().len() + "/native-control-1234567890.sock".len() < 104);
        assert_eq!(
            fs::metadata(&temporary.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn recovery_lease_probe_survives_rename_until_owner_releases_lock() {
        let temporary = recovery_temporary().unwrap();
        let campaign = temporary.0.join("campaign-fixture");
        fs::create_dir(&campaign).unwrap();
        let lease_path = campaign.join("lease");
        let owner = File::create(&lease_path).unwrap();
        let observer = File::open(&lease_path).unwrap();
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(lease_busy(&observer).unwrap());
        let pending = temporary.0.join("pending-fixture");
        fs::rename(&campaign, &pending).unwrap();
        assert!(!campaign.exists());
        assert!(lease_busy(&observer).unwrap());
        assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(!lease_busy(&observer).unwrap());
        // A successful probe must not retain an exclusive lock and prevent the
        // next production recovery owner from acquiring the released lease.
        assert_eq!(
            unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(lease_busy(&observer).unwrap());
        drop(owner);
        fs::remove_dir_all(&pending).unwrap();
        assert!(!lease_busy(&observer).unwrap());
    }
    #[test]
    fn released_lease_alone_does_not_prove_final_watchdog_cleanup() {
        let temporary = recovery_temporary().unwrap();
        let path = temporary.0.join("summary.json");
        assert!(wait_watchdog_summary(&path, Duration::ZERO).is_err());
        common::write_json(&path, &json!({"status":"running","complete":false})).unwrap();
        assert!(wait_watchdog_summary(&path, Duration::ZERO).is_err());
        let valid = json!({"status":"cancelled","complete":false,
            "originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,
            "workspaceCleanupDeferred":false,"outcomeCounts":{"CANCELLED":1}});
        common::write_json(&path, &valid).unwrap();
        wait_watchdog_summary(&path, Duration::ZERO).unwrap();
        for (key, value) in [
            ("status", json!("running")),
            ("complete", json!(true)),
            ("originalProjectFilesUnchanged", json!(false)),
            ("temporarySnapshotCleaned", json!(false)),
            ("workspaceCleanupDeferred", json!(true)),
            ("outcomeCounts", json!({})),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(watchdog_complete(&invalid).is_err(), "{key}");
        }
    }
    #[test]
    fn watchdog_waits_for_running_summary_to_be_atomically_finalized() {
        let temporary = recovery_temporary().unwrap();
        let path = temporary.0.join("summary.json");
        common::write_json(&path, &json!({"status":"running","complete":false})).unwrap();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            let pending = writer_path.with_extension("pending");
            common::write_json(
                &pending,
                &json!({"status":"cancelled","complete":false,
                    "originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,
                    "workspaceCleanupDeferred":false,"outcomeCounts":{"CANCELLED":1}}),
            )
            .unwrap();
            fs::rename(pending, writer_path).unwrap();
        });
        wait_watchdog_summary(&path, Duration::from_secs(1)).unwrap();
        writer.join().unwrap();
    }
    #[test]
    fn report_rejects_record_count_and_partial_score() {
        let mut report = json!({"version":1,"output":"/report","status":"completed","records":[{"mutantId":"m","outcome":"KILLED"}],"outcomeCounts":{"KILLED":1},"complete":true,"expectedMutants":1,"completedMutants":1,"originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,"workspaceCleanupDeferred":false});
        let text = b"FailForge mutation summary\nReport: /report/summary.json\n";
        assert!(summary_contract(&report, text).is_ok());
        report["outcomeCounts"]["KILLED"] = 2.into();
        assert!(summary_contract(&report, text).is_err());
        report["outcomeCounts"]["KILLED"] = 1.into();
        report["complete"] = false.into();
        report["mutationScore"] = json!({"percent":100});
        assert!(summary_contract(&report, text).is_err());
    }
    #[test]
    fn report_requires_terminal_path() {
        let report = json!({"version":1,"output":"/report","status":"no_mutants","records":[],"outcomeCounts":{},"complete":true,"expectedMutants":0,"completedMutants":0,"originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,"workspaceCleanupDeferred":false});
        assert!(
            summary_contract(&report, b"FailForge mutation summary\nReport: /other\n").is_err()
        );
    }
    #[test]
    fn author_barrier_ignores_text_and_incomplete_rows() {
        let t = common::Temporary::new("ff-author").unwrap();
        let path = t.0.join("mutants/m/capture");
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("stdout.bin"),
            b"loop\n{\"type\":\"testStart\",\"test\":{\"name\":\"other\"}}\n{",
        )
        .unwrap();
        assert!(!author_started(&t.0, "loop").unwrap());
        fs::write(
            path.join("stdout.bin"),
            b"{\"type\":\"testStart\",\"test\":{\"name\":\"loop\"}}\n",
        )
        .unwrap();
        assert!(author_started(&t.0, "loop").unwrap());
    }
    #[test]
    fn counts_require_exact_equality() {
        assert!(
            expect_counts(
                &json!({"outcomeCounts":{"KILLED":1,"SURVIVED":1}}),
                json!({"KILLED":1})
            )
            .is_err()
        );
    }
    #[test]
    fn full_score_accepts_native_float_but_rejects_contradictory_numbers() {
        for percent in [json!(100), json!(100.0)] {
            assert!(
                full_score(&json!({"denominator":2,"killed":2,"survived":0,"percent":percent}))
                    .is_ok()
            );
        }
        for wrong in [
            json!({"denominator":2,"killed":1,"survived":1,"percent":100.0}),
            json!({"denominator":0,"killed":0,"survived":0,"percent":100.0}),
            json!({"denominator":2,"killed":2,"survived":0,"percent":"100"}),
        ] {
            assert!(full_score(&wrong).is_err());
        }
    }
}

#[cfg(test)]
#[path = "regression_adversarial_tests.rs"]
mod adversarial_tests;
