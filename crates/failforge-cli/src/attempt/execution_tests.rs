use super::*;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use failforge_runner::{CleanupReport, CompletionCause, GroupState};
use serde_json::{Value, json};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-attempt-execution-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        for directory in [
            "copy/.dart_tool",
            "sdk/bin",
            "flutter/bin",
            "test-package/bin",
        ] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        let fixture = Self(root);
        fixture.write_package_config(&fixture.valid_package_config());
        fixture
    }

    fn config(&self) -> ExecutionConfig {
        ExecutionConfig {
            // A deliberately nonexistent path proves it is only report metadata.
            runner: self.0.join("never-execute-this-cli"),
            dart_sdk: self.0.join("sdk"),
            flutter_sdk: Some(self.0.join("flutter")),
            // Functional execution checks share CPU with the parallel CLI
            // suite. The dedicated timeout case overrides this larger budget.
            command_timeout: "15s".into(),
            test_timeout: "1s".into(),
        }
    }

    fn project(&self, flutter: bool) -> PreparedProject {
        PreparedProject {
            root: self.0.join("original"),
            name: "subject".into(),
            packages: BTreeMap::from([("test".into(), self.0.join("test-package"))]),
            graph: Value::Null,
            flutter,
            framework_collision: false,
        }
    }

    fn script(&self, source: &str) {
        for executable in ["sdk/bin/dart", "flutter/bin/flutter"] {
            let path = self.0.join(executable);
            fs::write(&path, format!("#!/bin/sh\n{source}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    fn copied(&self) -> PathBuf {
        self.0.join("copy")
    }

    fn capture(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn valid_package_config(&self) -> Value {
        json!({
            "configVersion": 2,
            "packages": [
                {"name": "subject", "rootUri": "../", "packageUri": "lib/"},
                {"name": "test", "rootUri": "../../test-package", "packageUri": "lib/"}
            ]
        })
    }

    fn write_package_config(&self, value: &Value) {
        fs::write(
            self.copied().join(".dart_tool/package_config.json"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn reporter(failed: bool, flutter: bool) -> Vec<u8> {
    let mut events = vec![
        json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":if flutter {Value::Null}else {"1.33.0".into()},"pid":100}),
        json!({"type":"suite","time":1,"suite":{"id":0,"platform":"vm","path":"test/value_test.dart"}}),
        json!({"type":"group","time":2,"group":{"id":1,"suiteID":0,"parentID":null,"name":"","testCount":1,"metadata":{"skip":false}}}),
        json!({"type":"testStart","time":3,"test":{"id":2,"suiteID":0,"groupIDs":[1],"name":"author","metadata":{"skip":false}}}),
    ];
    if failed {
        events.push(json!({"type":"error","time":4,"testID":2,"error":"Expected 1, actual 2","stackTrace":"test/value_test.dart 1:1","isFailure":true}));
    }
    events.extend([
        json!({"type":"testDone","time":5,"testID":2,"result":if failed {"failure"}else {"success"},"hidden":false,"skipped":false}),
        json!({"type":"allSuites","time":6,"count":1}),
        json!({"type":"done","time":7,"success":!failed}),
    ]);
    events
        .into_iter()
        .flat_map(|event| {
            let mut bytes = serde_json::to_vec(&event).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect()
}

#[test]
fn specifications_preserve_dart_flutter_argv_limits_and_private_environment() {
    let fixture = Fixture::new();
    for flutter in [false, true] {
        let config = fixture.config();
        let (spec, command, mode) = specification(
            &config,
            &fixture.project(flutter),
            &fixture.copied(),
            &fixture.capture("capture"),
        )
        .unwrap();
        assert_eq!(mode == TestReporter::Flutter, flutter);
        assert_eq!(spec.cwd, fixture.copied());
        assert_eq!(spec.timeout_policy.timeout, Some(Duration::from_secs(15)));
        assert_eq!(spec.timeout_policy.grace_period, Duration::from_secs(2));
        assert_eq!(
            spec.capture_policy.file_quota_bytes_per_stream,
            256 * 1024 * 1024
        );
        assert_eq!(
            spec.capture_policy.mode,
            failforge_runner::OutputMode::Capture
        );
        assert!(!spec.env.clear);
        for name in PRIVATE_ENV {
            assert!(spec.env.remove.contains(OsStr::new(name)));
            assert!(!spec.env.set.contains_key(OsStr::new(name)));
        }
        assert!(spec.env.remove.contains(OsStr::new("FAILFORGE_REPORT_FDS")));
        let expected_child = if flutter {
            vec![
                fixture.0.join("flutter/bin/flutter").display().to_string(),
                "--suppress-analytics".into(),
                "test".into(),
                "--no-pub".into(),
                "--machine".into(),
                "--concurrency".into(),
                "1".into(),
                "--timeout".into(),
                "1s".into(),
            ]
        } else {
            vec![
                fixture.0.join("sdk/bin/dart").display().to_string(),
                format!(
                    "--packages={}",
                    fixture
                        .copied()
                        .join(".dart_tool/package_config.json")
                        .display()
                ),
                fixture
                    .0
                    .join("test-package/bin/test.dart")
                    .display()
                    .to_string(),
                "--reporter".into(),
                "json".into(),
                "--platform".into(),
                "vm".into(),
                "--concurrency".into(),
                "1".into(),
                "--timeout".into(),
                "1s".into(),
            ]
        };
        assert_eq!(command[0], config.runner.display().to_string());
        assert_eq!(command[10], "--");
        assert_eq!(command[11..], expected_child);
        assert_eq!(spec.program, OsStr::new(&expected_child[0]));
        assert_eq!(
            spec.args,
            expected_child[1..]
                .iter()
                .map(Into::into)
                .collect::<Vec<std::ffi::OsString>>()
        );
    }
}

#[test]
fn invalid_paths_missing_dependencies_and_invalid_timeouts_fail_before_capture() {
    let fixture = Fixture::new();
    let base = fixture.config();
    for (field, value) in [
        ("command", "0s"),
        ("command", "999999999999999999999s"),
        ("test", "1s\0"),
        ("test", "-1s"),
    ] {
        let mut config = base.clone();
        if field == "command" {
            config.command_timeout = value.into();
        } else {
            config.test_timeout = value.into();
        }
        assert!(
            specification(
                &config,
                &fixture.project(false),
                &fixture.copied(),
                &fixture.capture("capture")
            )
            .is_err()
        );
    }
    let mut project = fixture.project(false);
    project.packages.clear();
    assert!(
        specification(
            &base,
            &project,
            &fixture.copied(),
            &fixture.capture("capture")
        )
        .is_err()
    );
    let mut no_flutter = base.clone();
    no_flutter.flutter_sdk = None;
    assert!(
        specification(
            &no_flutter,
            &fixture.project(true),
            &fixture.copied(),
            &fixture.capture("capture")
        )
        .is_err()
    );
    for path in [Path::new("relative"), Path::new("/bad\0path")] {
        assert!(
            specification(
                &base,
                &fixture.project(false),
                path,
                &fixture.capture("capture")
            )
            .is_err()
        );
    }
    assert!(!fixture.capture("capture").exists());
}

fn process_result() -> CommandResult {
    CommandResult {
        cause: CompletionCause::ProcessCompleted,
        status: Some(std::process::ExitStatus::from_raw(1 << 8)),
        timings: Default::default(),
        stdout: Default::default(),
        stderr: Default::default(),
        cleanup: CleanupReport {
            child_reaped: true,
            output_finished: true,
            group: GroupState::NoLiveMembers { zombies: 0 },
            ..Default::default()
        },
    }
}

#[test]
fn partial_results_and_failed_shutdown_preserve_evidence_but_never_native_success() {
    let mut result = process_result();
    result.cause = CompletionCause::TimedOut;
    let partial = completed(
        vec![],
        Err(RunnerError::Partial {
            operation: "capture",
            partial: Box::new(result),
        }
        .into()),
        Ok(()),
    );
    assert!(!partial.native_ok);
    assert_eq!(partial.exit_code, 125);
    assert_eq!(partial.result.unwrap().cause, CompletionCause::TimedOut);
    assert!(partial.diagnostics.contains("cause=TimedOut"));
    let shutdown = completed(
        vec![],
        Ok(process_result().into()),
        Err(RunnerError::ShutdownIncomplete { active: 1 }),
    );
    assert!(!shutdown.native_ok);
    assert_eq!(shutdown.exit_code, 125);
    assert!(shutdown.result.is_some());
    assert!(shutdown.diagnostics.contains("shutdown incomplete"));
    let ordinary = completed(vec![], Ok(process_result().into()), Ok(()));
    assert!(ordinary.native_ok);
    assert_eq!(ordinary.exit_code, 1);
}

// Keep the real Runner scenarios sequential within this test. Capture writer
// capacity is process-wide and separately exercised by the Runner test suite.
#[tokio::test]
async fn direct_execution_verifies_pass_failure_timeout_cancellation_and_no_spawn_guards() {
    let fixture = Fixture::new();
    let context = SelectionContext {
        alias: "subject".into(),
        ..Default::default()
    };
    for (index, (failed, flutter)) in [(false, false), (true, false), (false, true)]
        .into_iter()
        .enumerate()
    {
        let raw = reporter(failed, flutter);
        fs::write(fixture.copied().join("events"), &raw).unwrap();
        fixture.script(&format!(
            "/bin/cat events; printf diagnostic >&2; exit {}",
            i32::from(failed)
        ));
        let attempt = execute(
            &fixture.config(),
            &fixture.project(flutter),
            &fixture.copied(),
            &fixture.capture(&format!("capture-{index}")),
            &context,
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert!(attempt.native_ok, "{}", attempt.diagnostics);
        assert_eq!(attempt.preflight_rejection, None);
        assert_eq!(
            attempt.result.as_ref().unwrap().cause,
            CompletionCause::ProcessCompleted,
            "functional case {index}: {attempt:#?}"
        );
        assert_eq!(
            attempt.exit_code,
            u8::from(failed),
            "functional case {index}: {attempt:#?}"
        );
        assert!(attempt.result.as_ref().unwrap().cleanup.is_complete());
        let evidence = attempt.evidence.unwrap();
        assert_eq!(
            evidence.classification["outcome"],
            if failed { "KILLED" } else { "SURVIVED" }
        );
        assert_eq!(evidence.stdout_hash, failforge_dart::sha256_hex(&raw));
        assert_eq!(
            evidence.stderr_hash,
            failforge_dart::sha256_hex(b"diagnostic")
        );
        assert_eq!(
            evidence.catalogue.unwrap().identities,
            [["test/value_test.dart", "author"]]
        );
        assert!(evidence.initialization.observation.observation.is_some());
    }

    fixture.script("echo $$ > started; exec /bin/sleep 30");
    let mut config = fixture.config();
    config.command_timeout = "50ms".into();
    let timeout = execute(
        &config,
        &fixture.project(false),
        &fixture.copied(),
        &fixture.capture("timeout"),
        &context,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(timeout.exit_code, 124, "timeout case: {timeout:#?}");
    assert_eq!(
        timeout.evidence.unwrap().classification["outcome"],
        "TIMEOUT"
    );
    assert!(timeout.result.unwrap().cleanup.is_complete());
    // The timeout is allowed to win before this fixture writes its readiness
    // marker. The cancellation scenario below has an explicit readiness wait.
    let _ = fs::remove_file(fixture.copied().join("started"));

    let cancel = Cancellation::default();
    let requested = cancel.clone();
    let marker = fixture.copied().join("started");
    let signal = tokio::spawn(async move {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "test child did not start");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        requested.request(libc::SIGTERM);
    });
    let cancelled = execute(
        &fixture.config(),
        &fixture.project(false),
        &fixture.copied(),
        &fixture.capture("cancelled"),
        &context,
        &cancel,
    )
    .await
    .unwrap();
    signal.await.unwrap();
    assert_eq!(
        cancelled.exit_code, 143,
        "cancellation case: {cancelled:#?}"
    );
    assert_eq!(
        cancelled.result.as_ref().unwrap().cause,
        CompletionCause::Cancelled(CancelReason::SigTerm)
    );
    assert!(cancelled.result.unwrap().cleanup.is_complete());
    assert_eq!(
        cancelled.evidence.unwrap().classification["reason"],
        "runner_not_completed"
    );

    fs::remove_file(fixture.copied().join("started")).unwrap();
    assert!(matches!(
        execute(
            &fixture.config(),
            &fixture.project(false),
            &fixture.copied(),
            &fixture.capture("pre-cancelled"),
            &context,
            &cancel
        )
        .await,
        Err(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    assert!(!fixture.copied().join("started").exists());
    assert!(!fixture.capture("pre-cancelled").exists());

    let invalid_context = SelectionContext {
        baseline: Some(failforge_dart::CatalogSnapshot {
            raw_sha256: "bad".into(),
            cwd: "/".into(),
            identities: vec![],
            view: None,
        }),
        ..Default::default()
    };
    assert!(
        execute(
            &fixture.config(),
            &fixture.project(false),
            &fixture.copied(),
            &fixture.capture("invalid-context"),
            &invalid_context,
            &Cancellation::default()
        )
        .await
        .is_err()
    );
    assert!(!fixture.copied().join("started").exists());
    assert!(!fixture.capture("invalid-context").exists());

    fs::remove_file(fixture.0.join("sdk/bin/dart")).unwrap();
    let failed = execute(
        &fixture.config(),
        &fixture.project(false),
        &fixture.copied(),
        &fixture.capture("missing-sdk"),
        &context,
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert!(!failed.native_ok);
    assert_eq!(failed.exit_code, 125);
    assert!(failed.result.is_none() && failed.evidence.is_none());
    assert!(failed.diagnostics.contains("failed"));
    assert_eq!(failed.preflight_rejection, None);
}

#[tokio::test]
async fn invalid_actual_package_configuration_rejects_before_dart_or_flutter_spawn() {
    let fixture = Fixture::new();
    fixture.script("echo started > started; exit 0");
    let config_path = fixture.copied().join(".dart_tool/package_config.json");
    let context = SelectionContext {
        alias: "subject".into(),
        ..Default::default()
    };
    for flutter in [false, true] {
        for case in [
            "malformed",
            "missing",
            "duplicate-name",
            "duplicate-root",
            "missing-alias",
            "wrong-alias-root",
            "unsupported-version",
        ] {
            let mut config = fixture.valid_package_config();
            match case {
                "duplicate-name" => {
                    config["packages"][1]["name"] = "subject".into();
                }
                "duplicate-root" => {
                    config["packages"][1]["rootUri"] = "../".into();
                }
                "missing-alias" => config["packages"][0]["name"] = "other".into(),
                "wrong-alias-root" => {
                    config["packages"] = json!([
                        {"name":"subject", "rootUri":"../../test-package", "packageUri":"lib/"}
                    ]);
                }
                "unsupported-version" => config["configVersion"] = 1.into(),
                "malformed" | "missing" => (),
                _ => unreachable!(),
            }
            fixture.write_package_config(&config);
            match case {
                "malformed" => fs::write(&config_path, b"{").unwrap(),
                "missing" => fs::remove_file(&config_path).unwrap(),
                _ => (),
            }
            let capture = fixture.capture(&format!("reject-{flutter}-{case}"));
            let attempt = execute(
                &fixture.config(),
                &fixture.project(flutter),
                &fixture.copied(),
                &capture,
                &context,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                attempt.preflight_rejection,
                Some("invalid_package_configuration"),
                "{case}: {}",
                attempt.diagnostics
            );
            assert_eq!(attempt.exit_code, 125, "{case}");
            assert!(!attempt.native_ok, "{case}");
            assert!(attempt.result.is_none(), "{case}");
            assert!(attempt.evidence.is_none(), "{case}");
            assert!(
                attempt
                    .diagnostics
                    .contains("invalid test package configuration"),
                "{case}"
            );
            assert!(!fixture.copied().join("started").exists(), "{case}");
            assert!(!capture.exists(), "{case}");
        }
    }

    let cancel = Cancellation::default();
    cancel.request(libc::SIGINT);
    fs::write(&config_path, b"{").unwrap();
    assert!(matches!(
        execute(
            &fixture.config(),
            &fixture.project(false),
            &fixture.copied(),
            &fixture.capture("cancelled-invalid-config"),
            &context,
            &cancel,
        )
        .await,
        Err(NamespaceError::Cancelled(libc::SIGINT))
    ));
    assert!(!fixture.copied().join("started").exists());
    assert!(!fixture.capture("cancelled-invalid-config").exists());
}

#[test]
fn cancellation_during_configuration_validation_wins_without_spawning() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        for valid in [true, false] {
            let fixture = Fixture::new();
            fixture.script("echo started > started; exit 0");
            if !valid {
                fs::write(
                    fixture.copied().join(".dart_tool/package_config.json"),
                    b"{",
                )
                .unwrap();
            }
            // Occupy the only blocking thread so validation deterministically
            // awaits its read while the cancellation request arrives.
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            ready.await.unwrap();
            let config = fixture.config();
            let project = fixture.project(false);
            let copied = fixture.copied();
            let capture = fixture.capture("cancelled-validation");
            let context = SelectionContext {
                alias: "subject".into(),
                ..Default::default()
            };
            let cancel = Cancellation::default();
            let future = execute(&config, &project, &copied, &capture, &context, &cancel);
            tokio::pin!(future);
            tokio::select! {
                biased;
                result = &mut future => panic!("validation did not wait: {result:?}"),
                () = tokio::task::yield_now() => (),
            }
            cancel.request(libc::SIGTERM);
            release.send(()).unwrap();
            assert!(matches!(
                future.await,
                Err(NamespaceError::Cancelled(libc::SIGTERM))
            ));
            blocker.await.unwrap();
            assert!(!fixture.copied().join("started").exists());
            assert!(!capture.exists());
        }
    });
}
