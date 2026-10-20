use super::*;

const DISPLAY: &[u8] = b"FailForge mutation summary\nReport: /report/summary.json\n";

fn complete() -> Value {
    json!({
        "version":1,"family":"classic","output":"/report","status":"completed",
        "records":[{"mutantId":"m","outcome":"KILLED"}],
        "outcomeCounts":{"KILLED":1},"complete":true,
        "expectedMutants":1,"completedMutants":1,
        "originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,
        "workspaceCleanupDeferred":false,
        "mutationScore":{"family":"classic","denominator":1,"killed":1,
            "survived":0,"percent":100.0,"excluded":{}},
        "failureModeScore":null,"languageSpecificScore":null
    })
}

#[test]
fn missing_or_nonnumeric_completed_count_cannot_pass_regression() {
    let mut summary = complete();
    assert!(summary_contract(&summary, DISPLAY).is_ok());
    summary.as_object_mut().unwrap().remove("completedMutants");
    assert!(summary_contract(&summary, DISPLAY).is_err());
    for wrong in [Value::Null, json!("1"), json!(-1), json!(true)] {
        summary["completedMutants"] = wrong;
        assert!(summary_contract(&summary, DISPLAY).is_err());
    }
}

#[test]
fn complete_regression_report_requires_successful_cleanup_evidence() {
    for (key, wrong) in [
        ("originalProjectFilesUnchanged", false),
        ("temporarySnapshotCleaned", false),
        ("workspaceCleanupDeferred", true),
    ] {
        let mut summary = complete();
        summary[key] = wrong.into();
        assert!(summary_contract(&summary, DISPLAY).is_err(), "{key}");
        summary.as_object_mut().unwrap().remove(key);
        assert!(summary_contract(&summary, DISPLAY).is_err(), "{key}");
    }
}

#[test]
fn expected_failed_campaign_does_not_hide_an_additional_cleanup_failure() {
    let mut summary = complete();
    summary["status"] = "failed".into();
    summary["complete"] = false.into();
    summary["mutationScore"] = Value::Null;
    assert!(summary_contract(&summary, DISPLAY).is_ok());
    summary["temporarySnapshotCleaned"] = false.into();
    summary["workspaceCleanupDeferred"] = true.into();
    assert!(
        summary_contract(&summary, DISPLAY).is_err(),
        "expected campaign failure must not hide leaked working copies"
    );
}

#[test]
fn contradictory_or_missing_status_cannot_pass_complete_regression() {
    for status in [
        json!("failed"),
        json!("cancelled"),
        json!("running"),
        Value::Null,
    ] {
        let mut summary = complete();
        summary["status"] = status;
        assert!(summary_contract(&summary, DISPLAY).is_err());
    }
    let mut summary = complete();
    summary.as_object_mut().unwrap().remove("status");
    assert!(summary_contract(&summary, DISPLAY).is_err());
}

fn direct_command(arguments: &[&str]) -> (Value, PathBuf) {
    let temporary = common::Temporary::new("ff-tools-flutter-argv").unwrap();
    let runner = temporary.0.join("target/debug/failforge");
    fs::create_dir_all(runner.parent().unwrap()).unwrap();
    fs::write(&runner, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let project = temporary.0.join("subject");
    fs::create_dir(&project).unwrap();
    let output = temporary.0.join("reports");
    fs::create_dir(&output).unwrap();
    let mut harness = Harness {
        root: temporary.0.clone(),
        binary_dir: temporary.0.join("target/debug"),
        output: output.clone(),
        sdk: temporary.0.join("standalone-dart"),
        flutter: Some(temporary.0.join("flutter")),
        reports: Vec::new(),
        temp_root: None,
    };
    // The finite command records argv and exits. The missing summary is expected;
    // this test isolates command construction from SDK and report generation.
    assert!(harness.direct("flutter", &project, arguments, 0,).is_err());
    let receipt = common::read_json(&output.join("flutter-command/command.json")).unwrap();
    (receipt, temporary.0.clone())
}

#[test]
fn flutter_regression_does_not_supply_conflicting_dart_sdk_flag() {
    let (receipt, _) = direct_command(&[
        "--family",
        "failure_mode",
        "--flutter-sdk",
        "/fixture/flutter",
    ]);
    let args = receipt["command"].as_array().unwrap();
    assert!(args.contains(&json!("--flutter-sdk")));
    assert!(
        !args.contains(&json!("--dart-sdk")),
        "Flutter command contains mutually exclusive SDK flags: {args:?}"
    );
}

#[test]
fn regression_explicitly_selects_requested_installation() {
    let (receipt, root) = direct_command(&["--family", "classic"]);
    let args = receipt["command"].as_array().unwrap();
    let position = args.iter().position(|value| value == "--installation");
    assert!(
        position.is_some(),
        "requested installation was omitted: {args:?}"
    );
    assert_eq!(args[position.unwrap() + 1], json!(root));
}

#[test]
fn native_serialized_float_scores_pass_happy_path_contracts() {
    for (family, score_name) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        let mut summary = complete();
        let mut score = summary["mutationScore"].take();
        score["family"] = family.into();
        summary["family"] = family.into();
        summary[score_name] = score;
        let serialized = serde_json::to_vec(&summary).unwrap();
        let observed: Value = serde_json::from_slice(&serialized).unwrap();
        assert_eq!(observed[score_name]["percent"].as_f64(), Some(100.0));
        assert!(summary_contract(&observed, DISPLAY).is_ok(), "{family}");
        assert!(full_score(&observed[score_name]).is_ok(), "{family}");
    }
}

#[test]
fn regression_uses_explicit_binary_directory_without_changing_installation() {
    let temporary = common::Temporary::new("ff-tools-binary-routing").unwrap();
    let binary_dir = temporary.0.join("frozen-release");
    let root = temporary.0.join("prepared-installation");
    let harness = Harness {
        root: root.clone(),
        binary_dir: binary_dir.clone(),
        output: temporary.0.join("reports"),
        sdk: root.join("dart-sdk"),
        flutter: None,
        reports: Vec::new(),
        temp_root: None,
    };
    let command = harness.command();
    assert_eq!(command.get_program(), binary_dir.join("failforge"));
    assert_eq!(command.get_current_dir(), Some(root.as_path()));
}
