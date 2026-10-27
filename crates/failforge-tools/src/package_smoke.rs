//! Exercise a trusted, already extracted distribution using its public CLI.
use crate::common::{self, Result, ensure};
use clap::Args;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
#[cfg(all(test, unix))]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Args)]
pub struct Options {
    /// Root of a trusted, already extracted FailForge archive.
    #[arg(long)]
    installation: PathBuf,
    #[arg(long)]
    dart_sdk: PathBuf,
    /// New directory for command receipts, reports and dependency cache.
    #[arg(long)]
    output: PathBuf,
    /// Use the caller's existing PUB_CACHE and forbid dependency downloads.
    #[arg(long)]
    offline: bool,
}

const SOURCE: &str = "bool enabled() => true;\n";
const TEST: &str = "import 'package:failforge_package_smoke/value.dart';\nimport 'package:test/test.dart';\nvoid main() { test('enabled', () => expect(enabled(), isTrue)); }\n";

fn timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

fn prepared_pubspec(adapter: &Path) -> Result<String> {
    let raw = fs::read_to_string(adapter.join("pubspec.yaml"))?;
    let mut value = failforge_config::yaml::parse_yaml(&raw)?;
    ensure(
        value["name"] == "failforge_dart_adapter" && value["dev_dependencies"]["test"].is_string(),
        "adapter pubspec must identify its locked test dependency",
    )?;
    value["name"] = "failforge_package_smoke".into();
    // JSON is a YAML subset; preserve the complete locked dependency graph.
    Ok(serde_json::to_string_pretty(&value)? + "\n")
}

fn fresh_output(path: &Path, installation: &Path, sdk: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => resolved.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = resolved.canonicalize()?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Component::Prefix(_) => resolved.push(component),
        }
    }
    ensure(
        !resolved.starts_with(installation) && !resolved.starts_with(sdk),
        "smoke output must be outside the installation and SDK",
    )?;
    match fs::symlink_metadata(&resolved) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
        _ => Err("smoke output must not already exist".into()),
    }
}

fn payload(root: &Path, manifest: &Value) -> Result<usize> {
    ensure(
        manifest["schemaVersion"] == 1
            && manifest["version"].as_str().is_some_and(|v| !v.is_empty()),
        "unsupported package manifest",
    )?;
    let files = manifest["files"]
        .as_array()
        .ok_or("missing package files")?;
    let mut seen = BTreeSet::new();
    for file in files {
        let relative = file["path"].as_str().ok_or("missing package file path")?;
        ensure(
            !relative.is_empty()
                && Path::new(relative)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
                && seen.insert(relative),
            "invalid or duplicate package file path",
        )?;
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)?;
        ensure(
            metadata.is_file()
                && path.canonicalize()? == path
                && file["bytes"].as_u64() == Some(metadata.len())
                && file["executable"].as_bool() == Some(common::executable(&path, &metadata))
                && file["sha256"].as_str() == Some(common::hash(&path)?.as_str()),
            format!("package file differs from manifest: {relative}"),
        )?;
    }
    ensure(
        [
            if cfg!(windows) {
                "bin/failforge.exe"
            } else {
                "bin/failforge"
            },
            if cfg!(windows) {
                "bin/failforge-adapter-campaign.exe"
            } else {
                "bin/failforge-adapter-campaign"
            },
            "adapters/dart/pubspec.yaml",
            "adapters/dart/pubspec.lock",
        ]
        .iter()
        .all(|path| seen.contains(path)),
        "package manifest omits required inputs",
    )?;
    Ok(files.len())
}

fn version(stdout: &str, manifest: &Value) -> Result<()> {
    let expected = manifest["version"]
        .as_str()
        .ok_or("missing package version")?;
    ensure(
        stdout.trim() == format!("failforge {expected}"),
        "installed CLI version differs from package manifest",
    )
}

fn attempt(value: &Value, killed: bool) -> Result<PathBuf> {
    ensure(
        value["outcome"] == if killed { "KILLED" } else { "SURVIVED" }
            && value["exitCode"] == if killed { 1 } else { 0 }
            && value["reporterComplete"] == true
            && value["temporaryCopyCleaned"] == true
            && value["runner"]["cause"] == "ProcessCompleted"
            && value["runner"]["cleanup"] == "complete"
            && value["runner"]["status"] == if killed { "exit:1" } else { "exit:0" }
            && value["tests"]["started"] == 1
            && value["tests"]["failed"] == if killed { 1 } else { 0 }
            && value["tests"]["passed"] == if killed { 0 } else { 1 }
            && value["tests"]["skipped"] == 0
            && value["tests"]["hidden"] == 0
            && value["tests"]["loadingPassed"] == 1
            && value["tests"]["loadingFailed"] == 0
            && value["loadFailures"] == json!([])
            && value["testIdentities"] == json!([["test/value_test.dart", "enabled"]]),
        "smoke attempt lacks the expected author result and complete cleanup",
    )?;
    let path = PathBuf::from(
        value["isolatedWorkingDirectory"]
            .as_str()
            .ok_or("missing attempt directory")?,
    );
    ensure(path.is_absolute(), "attempt directory must be absolute")?;
    Ok(path)
}

fn completed(summary: &Value) -> Result<()> {
    ensure(
        summary["complete"] == true
            && summary["status"] == "completed"
            && summary["completedMutants"] == 1
            && summary["expectedMutants"] == 1
            && summary["outcomeCounts"] == json!({"KILLED":1})
            && summary["originalProjectFilesUnchanged"] == true
            && summary["temporarySnapshotCleaned"] == true
            && summary["workspaceCleanupDeferred"] == false,
        "smoke campaign is incomplete or lacks source and cleanup evidence",
    )
}

fn campaign(
    summary: &Value,
    manifest: &Value,
    installation: &Path,
    sdk: &Path,
    project: &Path,
    profile: bool,
) -> Result<(String, Vec<PathBuf>)> {
    completed(summary)?;
    let family = if profile {
        ensure(
            summary["kind"] == "profile"
                && summary["version"] == 2
                && summary["mutationScore"].is_null()
                && summary["families"]["classic"]["enabled"] == true
                && summary["families"]["classic"]["complete"] == true,
            "invalid profile summary",
        )?;
        &summary["families"]["classic"]["summary"]
    } else {
        ensure(summary["version"] == 1, "invalid direct report version")?;
        summary
    };
    completed(family)?;
    ensure(
        family["family"] == "classic"
            && family["allDiscoveredMutantsProcessed"] == true
            && family["error"].is_null()
            && family["mutationScore"]["denominator"] == 1
            && family["mutationScore"]["killed"] == 1
            && family["mutationScore"]["survived"] == 0
            && family["mutationScore"]["percent"].as_f64() == Some(100.0),
        "smoke family has unexpected score or discovery completion",
    )?;
    let records = family["records"]
        .as_array()
        .ok_or("missing mutant records")?;
    ensure(records.len() == 1, "expected exactly one mutant")?;
    let record = &records[0];
    ensure(
        record["operatorId"] == "BOOLEAN_LITERAL_FLIP"
            && record["file"] == "lib/value.dart"
            && record["compileChecked"] == true
            && record["validation"]["valid"] == true
            && summary["records"] == family["records"],
        "unexpected mutant or inconsistent root and family records",
    )?;
    let id = record["mutantId"].as_str().ok_or("missing mutant ID")?;
    ensure(
        id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "invalid mutant ID",
    )?;
    let controls = family["originalControls"]
        .as_array()
        .ok_or("missing final original")?;
    ensure(controls.len() == 1, "expected one final original control")?;
    let paths = vec![
        attempt(&family["baseline"], false)?,
        attempt(record, true)?,
        attempt(&controls[0], false)?,
    ];
    ensure(
        paths.iter().collect::<BTreeSet<_>>().len() == 3 && paths.iter().all(|p| p != project),
        "baseline, mutant and final original must use distinct fresh copies",
    )?;
    ensure(
        manifest["configuration"]["adapter_root"] == json!(installation.join("adapters/dart"))
            && manifest["configuration"]["dart_sdk"] == json!(sdk)
            && manifest["configuration"]["project"] == json!(project),
        "campaign selected a different installation, SDK or project",
    )?;
    Ok((id.to_owned(), paths))
}

struct Harness {
    installation: PathBuf,
    sdk: PathBuf,
    output: PathBuf,
    work: PathBuf,
    cache: PathBuf,
    offline: bool,
    commands: Vec<Value>,
}

impl Harness {
    fn command(&self, program: &Path) -> Command {
        let mut command = common::command(program);
        command
            .current_dir(self.work.join("foreign cwd"))
            .env("PUB_CACHE", &self.cache)
            .env("TMPDIR", self.work.join("temporary"))
            .env("TMP", self.work.join("temporary"))
            .env("TEMP", self.work.join("temporary"))
            .env("XDG_CONFIG_HOME", self.work.join("tool-state"))
            .env("DASH__SUPPRESS_ANALYTICS", "true")
            .env_remove("FAILFORGE_MUTATION_PARENT_FD");
        command
    }

    fn execute(&mut self, name: &str, command: &mut Command, seconds: u64) -> Result<()> {
        let directory = self.output.join("commands").join(name);
        let started = timestamp()?;
        let result = common::execute(command, &directory, Duration::from_secs(seconds), 0);
        let mut receipt = if directory.join("command.json").is_file() {
            common::read_json(&directory.join("command.json"))?
        } else {
            json!({"error":result.as_ref().err().map(ToString::to_string)})
        };
        receipt["name"] = name.into();
        receipt["startedUnixMillis"] = started.into();
        receipt["finishedUnixMillis"] = timestamp()?.into();
        receipt["cwd"] = json!(command.get_current_dir());
        common::write_json(&directory.join("command.json"), &receipt)?;
        self.commands.push(receipt);
        result.map(|_| ())
    }

    fn pub_get(&mut self, name: &str, directory: &Path) -> Result<()> {
        let lock = common::hash(&directory.join("pubspec.lock"))?;
        let mut command = self.command(&self.sdk.join(failforge_config::platform::DART_BIN));
        command.current_dir(directory).args([
            "--suppress-analytics",
            "pub",
            "get",
            "--enforce-lockfile",
        ]);
        if self.offline {
            command.arg("--offline");
        }
        self.execute(name, &mut command, 600)?;
        ensure(
            common::hash(&directory.join("pubspec.lock"))? == lock,
            "pub get changed the lockfile",
        )
    }

    fn run(&mut self) -> Result<Value> {
        let package_manifest = self.installation.join("package-manifest.json");
        let manifest_hash = common::hash(&package_manifest)?;
        let manifest = common::read_json(&package_manifest)?;
        let file_count = payload(&self.installation, &manifest)?;
        let adapter = self.installation.join("adapters/dart");
        #[cfg(unix)]
        let executable = self
            .work
            .join("foreign cwd")
            .join(failforge_config::platform::binary_name("failforge").as_ref());
        #[cfg(unix)]
        symlink(
            self.installation
                .join("bin")
                .join(failforge_config::platform::binary_name("failforge").as_ref()),
            &executable,
        )?;
        #[cfg(windows)]
        let executable = self
            .installation
            .join("bin")
            .join(failforge_config::platform::binary_name("failforge").as_ref());
        let binary_hashes = [
            common::hash(
                &self
                    .installation
                    .join("bin")
                    .join(failforge_config::platform::binary_name("failforge").as_ref()),
            )?,
            common::hash(&self.installation.join("bin").join(
                failforge_config::platform::binary_name("failforge-adapter-campaign").as_ref(),
            ))?,
        ];
        for (name, program, arguments) in [
            ("version", executable.clone(), vec!["--version"]),
            ("help", executable.clone(), vec!["--help"]),
            (
                "sdk-version",
                self.sdk.join(failforge_config::platform::DART_BIN),
                vec!["--version"],
            ),
        ] {
            let mut command = self.command(&program);
            command.args(arguments);
            self.execute(name, &mut command, 30)?;
        }
        version(
            &fs::read_to_string(self.output.join("commands/version/stdout.log"))?,
            &manifest,
        )?;
        self.pub_get("adapter-pub-get", &adapter)?;
        let project = self.work.join("subject");
        fs::create_dir_all(project.join("lib"))?;
        fs::create_dir(project.join("test"))?;
        fs::write(project.join("pubspec.yaml"), prepared_pubspec(&adapter)?)?;
        fs::copy(adapter.join("pubspec.lock"), project.join("pubspec.lock"))?;
        fs::write(project.join("lib/value.dart"), SOURCE)?;
        fs::write(project.join("test/value_test.dart"), TEST)?;
        self.pub_get("subject-pub-get", &project)?;
        let mut baseline = self.command(&self.sdk.join(failforge_config::platform::DART_BIN));
        baseline
            .current_dir(&project)
            .args(["--suppress-analytics", "test", "--concurrency", "1"]);
        self.execute("subject-baseline", &mut baseline, 180)?;
        let before_generation = common::tree(&project)?;
        let profile = project.join("profiles/value.yml");
        let mut generate = self.command(&executable);
        generate
            .args(["profile", "generate", "--file"])
            .arg(project.join("lib/value.dart"))
            .args([
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--dart-sdk",
            ])
            .arg(&self.sdk)
            .arg("--output")
            .arg(&profile);
        self.execute("profile-generate", &mut generate, 300)?;
        let mut after_generation = common::tree(&project)?;
        ensure(
            after_generation.remove("profiles/value.yml").is_some()
                && after_generation == before_generation,
            "profile generation changed inputs other than its output",
        )?;
        let generated = fs::read_to_string(&profile)?;
        common::write_json(
            &self.output.join("generated-profile.json"),
            &failforge_config::yaml::parse_yaml(&generated)?,
        )?;
        fs::write(self.output.join("generated-profile.yml"), &generated)?;
        // Keep the generated selection, but retain its reports outside the source.
        let profile_output = self.output.join("profile-report");
        fs::write(
            &profile,
            format!("{generated}\noutput: {}\n", json!(profile_output)),
        )?;
        let original = common::tree(&project)?;
        common::write_json(&self.output.join("original-inputs.json"), &json!(original))?;
        let mut outcomes = Vec::new();
        let mut previous_id = None;
        for use_profile in [false, true] {
            let name = if use_profile { "yaml" } else { "direct" };
            let report = if use_profile {
                profile_output.clone()
            } else {
                self.output.join("direct-report")
            };
            let mut command = self.command(&executable);
            command.arg("mutate");
            if use_profile {
                command.arg("--config").arg(&profile);
            } else {
                command
                    .arg("--project")
                    .arg(&project)
                    .args([
                        "--family",
                        "classic",
                        "--operator",
                        "BOOLEAN_LITERAL_FLIP",
                        "--include",
                        "lib/value.dart",
                        "--dart-sdk",
                    ])
                    .arg(&self.sdk)
                    .arg("--output")
                    .arg(&report);
            }
            self.execute(name, &mut command, 420)?;
            ensure(
                common::tree(&project)? == original,
                "campaign changed original inputs",
            )?;
            let summary = common::read_json(&report.join("summary.json"))?;
            let manifest = common::read_json(&report.join(if use_profile {
                "families/classic/manifest.json"
            } else {
                "manifest.json"
            }))?;
            let (id, attempts) = campaign(
                &summary,
                &manifest,
                &self.installation,
                &self.sdk,
                &project,
                use_profile,
            )?;
            for path in &attempts {
                ensure(
                    path.starts_with(self.work.join("temporary")) && !path.try_exists()?,
                    "attempt copy remains or escaped the smoke temporary root",
                )?;
            }
            let workspace = Path::new(
                summary["workspaceDirectory"]
                    .as_str()
                    .ok_or("missing workspace directory")?,
            );
            ensure(
                workspace.starts_with(self.work.join("temporary")) && !workspace.try_exists()?,
                "campaign workspace remains or escaped the smoke temporary root",
            )?;
            if let Some(expected) = &previous_id {
                ensure(
                    expected == &id,
                    "direct and YAML selected different mutants",
                )?;
            }
            previous_id = Some(id.clone());
            outcomes.push(json!({"entry":name,"mutantId":id,"outcomeCounts":summary["outcomeCounts"],"wallSeconds":summary["wallSeconds"],"report":report,"freshAttemptDirectories":attempts,"sourceUnchanged":true,"cleanupConfirmed":true}));
        }
        ensure(
            binary_hashes
                == [
                    common::hash(
                        &self
                            .installation
                            .join("bin")
                            .join(failforge_config::platform::binary_name("failforge").as_ref()),
                    )?,
                    common::hash(
                        &self.installation.join("bin").join(
                            failforge_config::platform::binary_name("failforge-adapter-campaign")
                                .as_ref(),
                        ),
                    )?,
                ],
            "installed executables changed during smoke",
        )?;
        ensure(
            common::hash(&package_manifest)? == manifest_hash,
            "package manifest changed during smoke",
        )?;
        payload(&self.installation, &manifest)?;
        Ok(
            json!({"campaigns":outcomes,"binarySha256":binary_hashes,"packageVersion":manifest["version"],"packageManifestSha256":manifest_hash,"payloadFilesVerified":file_count,"payloadUnchanged":true,"directYamlParity":true,"sourceUnchanged":true,"cleanupConfirmed":true}),
        )
    }
}

pub fn run(options: Options) -> Result<()> {
    let installation = options.installation.canonicalize()?;
    let sdk = options.dart_sdk.canonicalize()?;
    for path in [
        installation
            .join("bin")
            .join(failforge_config::platform::binary_name("failforge").as_ref()),
        installation
            .join("bin")
            .join(failforge_config::platform::binary_name("failforge-adapter-campaign").as_ref()),
        sdk.join(failforge_config::platform::DART_BIN),
    ] {
        let metadata = fs::metadata(&path)?;
        ensure(
            metadata.is_file() && common::executable(&path, &metadata),
            "required executable is unavailable",
        )?;
    }
    let output = fresh_output(&options.output, &installation, &sdk)?;
    common::create_output(&output)?;
    let output = output.canonicalize()?;
    let cache = if options.offline {
        let cache =
            std::env::var_os("PUB_CACHE").ok_or("--offline requires an existing PUB_CACHE")?;
        let cache = PathBuf::from(cache).canonicalize()?;
        ensure(cache.is_dir(), "PUB_CACHE must be a directory")?;
        cache
    } else {
        let cache = output.join("pub-cache");
        fs::create_dir(&cache)?;
        cache
    };
    let temporary = common::Temporary::new_in(&output, "smoke-inputs")?;
    for directory in ["foreign cwd", "temporary", "tool-state"] {
        fs::create_dir(temporary.0.join(directory))?;
    }
    let started = timestamp()?;
    let elapsed = Instant::now();
    let mut harness = Harness {
        installation,
        sdk,
        output: output.clone(),
        work: temporary.0.clone(),
        cache,
        offline: options.offline,
        commands: Vec::new(),
    };
    common::write_json(
        &output.join("summary.json"),
        &json!({"allPassed":false,"status":"running","startedUnixMillis":started}),
    )?;
    let result = harness.run();
    let work = temporary.0.clone();
    drop(temporary);
    let removed = !work.try_exists()?;
    let passed = result.is_ok() && removed && common::cleanup_confirmed();
    let summary = json!({"allPassed":passed,"status":if passed {"completed"} else {"failed"},"installation":harness.installation,"dartSdk":harness.sdk,"offline":options.offline,"cacheMode":if options.offline {"existing-caller-cache"} else {"new-isolated-cache"},"startedUnixMillis":started,"finishedUnixMillis":timestamp()?,"wallSeconds":elapsed.elapsed().as_secs_f64(),"commands":harness.commands,"results":result.as_ref().ok(),"error":result.as_ref().err().map(ToString::to_string),"temporaryInputsRemoved":removed,"ownerCleanupConfirmed":common::cleanup_confirmed()});
    common::write_json(&output.join("summary.json"), &summary)?;
    result?;
    ensure(passed, "smoke temporary cleanup was not confirmed")?;
    println!(
        "package smoke passed: {}",
        output.join("summary.json").display()
    );
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "package_smoke_tests.rs"]
mod tests;
