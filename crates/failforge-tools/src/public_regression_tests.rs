use super::*;

fn sample() -> Value {
    json!({"version":1,"cases":[{
        "name":"clock","group":"canonical","project":"/tmp/prepared",
        "configuration":{"family":"classic","rules":{"operators":["BOOLEAN_LITERAL_FLIP"],"include":["lib/a.dart"]},"timeouts":{"test":"5s","command":"60s"},"limits":{"max_mutants":1000}},
        "variants":["direct"],"expected":"/tmp/golden.json","budgetSeconds":120
    }]})
}
fn parse(value: &Value) -> Result<Manifest> {
    let temp = common::Temporary::new("ff-public-manifest")?;
    let path = temp.0.join("manifest.json");
    common::write_json(&path, value)?;
    read_manifest(&path)
}
#[test]
fn rejects_unknown_commands_unsafe_names_and_empty_or_unbounded_cases() {
    assert!(parse(&sample()).is_ok());
    let mut value = sample();
    value["cases"][0]["command"] = json!(["sh", "-c", "echo bad"]);
    assert!(parse(&value).is_err());
    for name in ["../escaped", "", "same/name", ".", ".."] {
        let mut value = sample();
        value["cases"][0]["name"] = name.into();
        assert!(parse(&value).is_err());
    }
    for budget in [0, 172801] {
        let mut value = sample();
        value["cases"][0]["budgetSeconds"] = budget.into();
        assert!(parse(&value).is_err());
    }
    let mut value = sample();
    value["cases"][0]["expected"] = Value::Null;
    assert!(parse(&value).is_err());
}
#[test]
fn rejects_ambiguous_duplicate_case_variant_and_source_escape() {
    let mut value = sample();
    let case = value["cases"][0].clone();
    value["cases"].as_array_mut().unwrap().push(case);
    assert!(parse(&value).is_err());
    let mut value = sample();
    value["cases"][0]["variants"] = json!(["direct", "direct"]);
    assert!(parse(&value).is_err());
    for file in ["/tmp/a.dart", "../a.dart"] {
        let mut value = sample();
        value["cases"][0]["generation"] = json!({"file":file,"families":["classic"]});
        assert!(parse(&value).is_err());
    }
}
#[test]
fn detects_exact_id_verdict_and_patch_changes_without_losing_other_differences() {
    let expected = json!([
        {"mutantId":"a","outcome":"KILLED","edit":{"replacement":"false"}},
        {"mutantId":"b","outcome":"SURVIVED"},
        {"mutantId":"c","outcome":"INVALID"}
    ]);
    let actual = json!([
        {"mutantId":"a","outcome":"SURVIVED","edit":{"replacement":"true"}},
        {"mutantId":"b","outcome":"SURVIVED"},
        {"mutantId":"d","outcome":"INVALID"}
    ]);
    assert_eq!(differences(&actual, &expected).unwrap().len(), 4);
    assert!(differences(&expected, &expected).unwrap().is_empty());
    assert!(record_map(&json!([{"mutantId":"a"},{"mutantId":"a"}])).is_err());
}
#[test]
fn direct_routing_keeps_each_argument_literal_and_freezes_workers() {
    let mut value = sample();
    value["cases"][0]["configuration"]["rules"]["methods"] =
        json!(["method with spaces; $(touch bad)"]);
    let manifest = parse(&value).unwrap();
    let harness = Harness {
        root: "/repo".into(),
        binary: "/release/failforge".into(),
        sdk: "/sdk".into(),
        output: "/output".into(),
        temp_root: "/tmp/ff-test".into(),
    };
    let command = harness
        .selected_command(
            &manifest.cases[0],
            Variant::Direct,
            Path::new("/subject"),
            Path::new("/report"),
        )
        .unwrap();
    assert_eq!(command.get_program(), "/release/failforge");
    let args: Vec<_> = command.get_args().map(|a| a.to_str().unwrap()).collect();
    assert!(
        args.windows(2)
            .any(|a| a == ["--method", "method with spaces; $(touch bad)"])
    );
    assert!(args.windows(2).any(|a| a == ["--workers", "1"]));
    assert!(args.windows(2).any(|a| a == ["--installation", "/repo"]));
    assert!(!args.contains(&"--python"));
}
#[test]
fn profile_rewrite_only_changes_fresh_copy_and_rejects_existing_reserved_path() {
    let temp = common::Temporary::new("ff-public-profile").unwrap();
    let original = temp.0.join("original.yml");
    let profile = json!({"mutations":{"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}}},"rules":{"include":["lib/a.dart"]},"workers":3,"output":"/old"});
    common::write_json(&original, &profile).unwrap();
    let before = common::hash(&original).unwrap();
    let subject = temp.0.join("subject");
    fs::create_dir(&subject).unwrap();
    let mut value = sample();
    value["cases"][0]["profile"] = json!(original);
    value["cases"][0]["variants"] = json!(["profile"]);
    let manifest = parse(&value).unwrap();
    let harness = Harness {
        root: "/repo".into(),
        binary: "/release/failforge".into(),
        sdk: "/sdk".into(),
        output: "/output".into(),
        temp_root: "/tmp/ff-test".into(),
    };
    harness
        .selected_command(
            &manifest.cases[0],
            Variant::Profile,
            &subject,
            Path::new("/new"),
        )
        .unwrap();
    assert_eq!(common::hash(&original).unwrap(), before);
    let copied =
        common::read_json(&subject.join("profiles/retained-public-regression.json")).unwrap();
    assert_eq!(copied["workers"], 1);
    assert_eq!(copied["output"], "/new");
    assert_eq!(copied["mutations"], profile["mutations"]);
    assert!(
        harness
            .selected_command(
                &manifest.cases[0],
                Variant::Profile,
                &subject,
                Path::new("/new")
            )
            .is_err()
    );
}
#[test]
fn requires_real_baseline_final_control_and_per_attempt_cleanup() {
    let attempt =
        json!({"outcome":"SURVIVED","temporaryCopyCleaned":true,"runner":{"cleanup":"complete"}});
    let summary = json!({"family":"classic","complete":true,"records":[{"family":"classic","outcome":"INVALID","reason":"static_analysis"}],"baseline":attempt,"originalControls":[attempt],"completedMutants":1,"outcomeCounts":{"INVALID":1},"mutationScore":{"family":"classic","killed":0,"survived":0,"denominator":0,"excluded":{"INVALID":1},"percent":null}});
    family_contract(&summary, false).unwrap();
    let mut wrong = summary.clone();
    wrong["originalControls"] = json!([]);
    assert!(family_contract(&wrong, false).is_err());
    let mut wrong = summary.clone();
    wrong["baseline"]["outcome"] = "KILLED".into();
    assert!(family_contract(&wrong, false).is_err());
    let mut wrong = summary;
    wrong["records"][0] =
        json!({"outcome":"KILLED","temporaryCopyCleaned":false,"runner":{"cleanup":"complete"}});
    assert!(family_contract(&wrong, false).is_err());
}
#[test]
fn score_check_excludes_timeout_and_accepts_float_100() {
    let mut summary = json!({"family":"classic","complete":true,"records":[{"family":"classic","outcome":"KILLED"},{"family":"classic","outcome":"TIMEOUT"}],"mutationScore":{"family":"classic","killed":1,"survived":0,"denominator":1,"excluded":{"TIMEOUT":1},"percent":100.0}});
    score_contract(&summary).unwrap();
    summary["mutationScore"]["denominator"] = 2.into();
    assert!(score_contract(&summary).is_err());
}

#[test]
fn complete_selected_family_requires_a_score_and_all_direct_fields_are_represented() {
    let summary = json!({"family":"classic","complete":true,"records":[]});
    assert!(score_contract(&summary).is_err());
    for (container, field) in [
        ("timeouts", "total"),
        ("rules", "default"),
        ("limits", "maximum"),
        ("", "workers"),
    ] {
        let mut value = sample();
        if container.is_empty() {
            value["cases"][0]["configuration"][field] = 4.into();
        } else {
            value["cases"][0]["configuration"][container][field] = json!("unsupported");
        }
        assert!(
            parse(&value).is_err(),
            "silently dropped {container}.{field}"
        );
    }
}

#[test]
fn cancellation_retains_observed_records_and_only_discovered_candidate_metadata() {
    let receipt = json!({"completedAtCancellationBarrier":2});
    assert!(cancellation_barrier_contract(&json!({"completedMutants":0}), &receipt).is_err());
    assert!(cancellation_barrier_contract(&json!({"completedMutants":1}), &receipt).is_err());
    cancellation_barrier_contract(&json!({"completedMutants":3}), &receipt).unwrap();
    let mutant = json!({"id":"a","family":"classic","operatorId":"BOOLEAN_LITERAL_FLIP","file":"lib/a.dart","edit":{"replacement":"false"}});
    let candidate = json!({"mutant":mutant});
    let record = json!({"mutantId":"a","family":"classic","operatorId":"BOOLEAN_LITERAL_FLIP","file":"lib/a.dart","edit":{"replacement":"false"}});
    candidate_membership(&json!([record]), std::slice::from_ref(&candidate), false).unwrap();
    assert!(candidate_membership(&json!([]), std::slice::from_ref(&candidate), true).is_err());
    assert!(
        candidate_membership(
            &json!([{"mutantId":"foreign"}]),
            std::slice::from_ref(&candidate),
            false
        )
        .is_err()
    );
    let mut wrong = record;
    wrong["edit"]["replacement"] = "true".into();
    assert!(candidate_membership(&json!([wrong]), &[candidate], false).is_err());
}

#[test]
fn actual_registry_rejects_campaigns_and_pending_reapers() {
    let temp = common::Temporary::new("ff-public-registry").unwrap();
    let registry = temp.0.join(format!("failforge-workspaces-{}", unsafe {
        libc::geteuid()
    }));
    fs::create_dir(&registry).unwrap();
    fs::write(registry.join(".global.lock"), "").unwrap();
    fs::write(registry.join(".owner"), "").unwrap();
    let harness = Harness {
        root: "/repo".into(),
        binary: "/bin/true".into(),
        sdk: "/sdk".into(),
        output: "/output".into(),
        temp_root: temp.0.clone(),
    };
    harness.registry_clean().unwrap();
    for name in ["campaign-active", "pending-reaper"] {
        fs::create_dir(registry.join(name)).unwrap();
        assert!(harness.registry_clean().is_err());
        fs::remove_dir(registry.join(name)).unwrap();
    }
    let profile_registry = temp
        .0
        .join(format!("failforge-profile-workspaces-{}", unsafe {
            libc::geteuid()
        }));
    fs::create_dir(&profile_registry).unwrap();
    fs::create_dir(profile_registry.join("campaign-leaked-generation")).unwrap();
    assert!(harness.registry_clean().is_err());
}

#[test]
fn interrupted_binary_verification_is_unknown_not_a_detected_mismatch() {
    assert_eq!(
        binary_verification(Ok("same".to_owned()), "same"),
        (Some(true), None)
    );
    assert_eq!(
        binary_verification(Ok("other".to_owned()), "same"),
        (Some(false), None)
    );
    let interrupted = Err(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "hashing interrupted by SIGINT",
    )
    .into());
    let (unchanged, error) = binary_verification(interrupted, "same");
    assert_eq!(unchanged, None);
    assert!(error.unwrap().contains("interrupted"));
}
