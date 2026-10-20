use super::*;

fn record(family: &str, id: &str, outcome: &str, reason: &str) -> Value {
    json!({"recordKind":"mutation","family":family,"mutantId":id,
        "outcome":outcome,"reason":reason,"opaque":{"capture":"unchanged","n":9007199254740993u64}})
}

fn family(name: &str, records: Vec<Value>) -> Value {
    json!({
        "family":name, "language":if name == "language_specific" {json!("dart")} else {Value::Null},
        "output":format!("/reports/families/{name}"),
        "status":if records.is_empty() {"no_mutants"} else {"completed"},
        "error":null,"expectedMutants":records.len(),"records":records,
        "baseline":{"outcome":"SURVIVED","opaque":{"retained":true}},
        "originalControls":[{"outcome":"SURVIVED","opaque":[1,2,3]}],
        "timings":{"baseline":0.25},"wallSeconds":0.75
    })
}

fn request(mode: &str, families: Vec<Value>) -> Value {
    let total = families
        .iter()
        .map(|family| family["expectedMutants"].as_u64().unwrap())
        .sum::<u64>();
    json!({
        "operation":"campaign-report","mode":mode,"runId":"0123456789abcdef0123456789abcdef",
        "output":"/reports","status":if total == 0 {"no_mutants"} else {"completed"},
        "error":null,"expectedMutants":total,"timings":{"snapshot":0.5},"wallSeconds":1.5,
        "cleanup":{"originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,
            "workspaceCleanupDeferred":false,"workspaceDirectory":"/registry/campaign-original",
            "workspaceRecovery":{"recovered":["campaign-old"],"skipped":[{"name":"foreign","reason":"unknown_name"}]}},
        "families":families
    })
}

fn one() -> Value {
    request(
        "direct",
        vec![family(
            "classic",
            vec![record("classic", "one", "KILLED", "assertion")],
        )],
    )
}

fn build_value(value: &Value) -> Result<Value, NamespaceError> {
    build(&serde_json::to_vec(value).unwrap())
}

#[test]
fn direct_summary_matches_existing_schema_and_preserves_opaque_evidence() {
    let input = one();
    let value = build_value(&input).unwrap();
    assert_eq!(
        value,
        json!({
            "version":1,"runId":"0123456789abcdef0123456789abcdef",
            "output":"/reports/families/classic","family":"classic",
            "status":"completed","complete":true,"error":null,
            "expectedMutants":1,"completedMutants":1,"records":input["families"][0]["records"],
            "baseline":input["families"][0]["baseline"],
            "originalControls":input["families"][0]["originalControls"],
            "timings":{"snapshot":0.5},"wallSeconds":1.5,
            "originalProjectFilesUnchanged":true,"temporarySnapshotCleaned":true,
            "workspaceCleanupDeferred":false,"workspaceDirectory":"/registry/campaign-original",
            "workspaceRecovery":{"recovered":["campaign-old"],"skipped":[{"name":"foreign","reason":"unknown_name"}]},
            "outcomeCounts":{"KILLED":1},"reasonCounts":{"assertion":1},
            "mutationScore":{"family":"classic","killed":1,"survived":0,"denominator":1,
                "percent":100.0,"excluded":{}},
            "failureModeScore":null
        })
    );
    assert!(value.get("languageSpecificScore").is_none());
    assert!(value.get("profileComplete").is_none());
    assert!(value.get("kind").is_none());
}

#[test]
fn all_outcomes_are_counted_but_only_killed_and_survived_are_scored() {
    let mut rows = OUTCOMES
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| record("classic", &format!("{index}"), outcome, "shared"))
        .collect::<Vec<_>>();
    rows.push(record("classic", "last", "KILLED", "different"));
    let value = build_value(&request("direct", vec![family("classic", rows)])).unwrap();
    assert_eq!(value["completedMutants"], 9);
    assert_eq!(value["reasonCounts"], json!({"shared":8,"different":1}));
    assert_eq!(value["outcomeCounts"]["KILLED"], 2);
    let score = &value["mutationScore"];
    assert_eq!(score["denominator"], 3);
    assert_eq!(score["percent"].as_f64(), Some(200.0 / 3.0));
    assert_eq!(
        score["excluded"],
        json!({"INVALID":1,"TIMEOUT":1,"INFRA_ERROR":1,"UNCLASSIFIED":1,"NO_TESTS":1,"CANCELLED":1})
    );
}

#[test]
fn each_family_has_its_existing_score_fields_and_language_metadata() {
    for (name, score_key) in [
        ("classic", "mutationScore"),
        ("failure_mode", "failureModeScore"),
        ("language_specific", "languageSpecificScore"),
    ] {
        let value = build_value(&request(
            "direct",
            vec![family(name, vec![record(name, "id", "SURVIVED", "passed")])],
        ))
        .unwrap();
        assert_eq!(value[score_key]["percent"], 0.0, "{name}");
        for key in ["mutationScore", "failureModeScore"] {
            if key != score_key {
                assert_eq!(value[key], Value::Null);
            }
        }
        assert_eq!(value.get("language").is_some(), name == "language_specific");
        assert_eq!(
            value.get("languageSpecificScore").is_some(),
            name == "language_specific"
        );
        if name == "language_specific" {
            assert_eq!(value["language"], "dart");
        }
    }
}

#[test]
fn profile_orders_families_and_records_and_never_invents_a_combined_score() {
    let names = ["language_specific", "failure_mode", "classic"];
    let input = request(
        "profile",
        names
            .iter()
            .map(|name| {
                family(
                    name,
                    vec![
                        record(name, "z", "KILLED", "killed"),
                        record(name, "a", "SURVIVED", "passed"),
                    ],
                )
            })
            .collect(),
    );
    let value = build_value(&input).unwrap();
    let rows = value["records"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| (
                row["family"].as_str().unwrap(),
                row["mutantId"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("classic", "a"),
            ("classic", "z"),
            ("failure_mode", "a"),
            ("failure_mode", "z"),
            ("language_specific", "a"),
            ("language_specific", "z")
        ]
    );
    assert_eq!(value["kind"], "profile");
    assert_eq!(value["version"], 2);
    assert_eq!(value["output"], "/reports");
    assert_eq!(value["completedMutants"], 6);
    assert_eq!(value["outcomeCounts"], json!({"KILLED":3,"SURVIVED":3}));
    assert_eq!(value["reasonCounts"], json!({"killed":3,"passed":3}));
    for key in ["mutationScore", "failureModeScore", "languageSpecificScore"] {
        assert_eq!(value[key], Value::Null);
    }
    for family in FAMILIES {
        assert_eq!(
            value["families"][family.name()]["summary"]["profileComplete"],
            true
        );
        assert_eq!(value["families"][family.name()]["expectedMutants"], 2);
    }
}

#[test]
fn incomplete_profile_keeps_completed_children_but_withholds_every_score() {
    for status in ["failed", "cancelled"] {
        let mut input = request(
            "profile",
            vec![
                family(
                    "classic",
                    vec![record("classic", "id", "KILLED", "assertion")],
                ),
                family("failure_mode", vec![]),
            ],
        );
        input["status"] = status.into();
        input["error"] = "interrupted".into();
        input["expectedMutants"] = Value::Null;
        input["families"][1]["status"] = "not_started".into();
        input["families"][1]["expectedMutants"] = Value::Null;
        input["families"][1]["baseline"] = Value::Null;
        input["families"][1]["originalControls"] = json!([]);
        let value = build_value(&input).unwrap();
        let completed = &value["families"]["classic"];
        assert_eq!(completed["complete"], true);
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["summary"]["profileComplete"], false);
        assert_eq!(completed["summary"]["mutationScore"], Value::Null);
        assert_eq!(value["complete"], false);
        assert_eq!(value["families"]["failure_mode"]["complete"], false);
        assert_eq!(
            value["families"]["failure_mode"]["summary"]["baseline"],
            Value::Null
        );
        assert_eq!(
            value["families"]["language_specific"],
            json!({"enabled":false,"status":"disabled","complete":true,"summary":null})
        );
    }
}

#[test]
fn direct_global_fields_override_family_state_and_failed_bootstrap_has_no_score() {
    let mut input = one();
    input["status"] = "failed".into();
    input["error"] = "original changed".into();
    input["expectedMutants"] = Value::Null;
    input["cleanup"]["originalProjectFilesUnchanged"] = false.into();
    let value = build_value(&input).unwrap();
    assert_eq!(value["status"], "failed");
    assert_eq!(value["error"], "original changed");
    assert_eq!(value["expectedMutants"], Value::Null);
    assert_eq!(value["complete"], false);
    assert_eq!(value["mutationScore"], Value::Null);
    assert_eq!(value["timings"], input["timings"]);
    assert_eq!(value["wallSeconds"], input["wallSeconds"]);

    input["families"][0]["records"] = json!([]);
    input["families"][0]["expectedMutants"] = Value::Null;
    input["families"][0]["status"] = "not_started".into();
    input["families"][0]["baseline"] = Value::Null;
    input["families"][0]["originalControls"] = json!([]);
    input["cleanup"]["temporarySnapshotCleaned"] = false.into();
    input["cleanup"]["workspaceCleanupDeferred"] = true.into();
    input["cleanup"]["workspaceDirectory"] = Value::Null;
    let value = build_value(&input).unwrap();
    assert_eq!(value["completedMutants"], 0);
    assert_eq!(value["outcomeCounts"], json!({}));
    assert_eq!(value["workspaceDirectory"], Value::Null);
}

#[test]
fn empty_success_and_all_excluded_results_have_no_percentage() {
    for mode in ["direct", "profile"] {
        let value = build_value(&request(mode, vec![family("classic", vec![])])).unwrap();
        assert_eq!(value["status"], "no_mutants");
        assert_eq!(value["completedMutants"], 0);
        let summary = if mode == "direct" {
            &value
        } else {
            &value["families"]["classic"]["summary"]
        };
        assert_eq!(summary["mutationScore"]["denominator"], 0);
        assert_eq!(summary["mutationScore"]["percent"], Value::Null);
    }
    let value = build_value(&request(
        "direct",
        vec![family(
            "classic",
            vec![record("classic", "id", "INVALID", "static_analysis")],
        )],
    ))
    .unwrap();
    assert_eq!(value["mutationScore"]["denominator"], 0);
    assert_eq!(value["mutationScore"]["percent"], Value::Null);
    assert_eq!(value["mutationScore"]["excluded"], json!({"INVALID":1}));
}

#[test]
fn invalid_record_identity_family_kind_outcome_or_reason_fails_closed() {
    for (key, replacement) in [
        ("mutantId", json!("")),
        ("mutantId", json!(1)),
        ("mutantId", Value::Null),
        ("family", json!("failure_mode")),
        ("recordKind", json!("baseline")),
        ("outcome", json!("UNKNOWN")),
        ("outcome", json!(false)),
        ("reason", Value::Null),
        ("reason", json!(["reason"])),
    ] {
        let mut input = one();
        input["families"][0]["records"][0][key] = replacement;
        assert!(build_value(&input).is_err(), "{key}: {input}");
    }
    for value in [json!([]), json!(true), Value::Null, json!("record")] {
        let mut input = one();
        input["families"][0]["records"][0] = value;
        assert!(build_value(&input).is_err());
    }
    let mut duplicate = one();
    duplicate["families"][0]["records"] = json!([
        record("classic", "id", "KILLED", ""),
        record("classic", "id", "SURVIVED", "")
    ]);
    duplicate["families"][0]["expectedMutants"] = 2.into();
    duplicate["expectedMutants"] = 2.into();
    assert!(build_value(&duplicate).is_err());
}

#[test]
fn unicode_ids_reasons_and_unknown_payload_fields_survive_sorting() {
    let input = request(
        "direct",
        vec![family(
            "classic",
            vec![
                record("classic", "💥", "KILLED", "қате"),
                record("classic", "я", "KILLED", "қате"),
                record("classic", "a", "SURVIVED", ""),
            ],
        )],
    );
    let value = build_value(&input).unwrap();
    assert_eq!(value["records"][0]["mutantId"], "a");
    assert_eq!(value["records"][1]["mutantId"], "я");
    assert_eq!(value["records"][2]["mutantId"], "💥");
    assert_eq!(value["reasonCounts"], json!({"қате":2,"":1}));
    assert_eq!(
        value["records"][0]["opaque"]["n"].as_u64(),
        Some(9007199254740993)
    );
}

#[test]
fn strict_named_schema_rejects_unknown_fields_and_omitted_nullable_metadata() {
    for pointer in [
        "",
        "/cleanup",
        "/cleanup/workspaceRecovery",
        "/cleanup/workspaceRecovery/skipped/0",
        "/families/0",
    ] {
        let mut input = one();
        input
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), true.into());
        assert!(build_value(&input).is_err(), "{pointer}");
    }
    for (pointer, key) in [
        ("", "error"),
        ("", "expectedMutants"),
        ("/cleanup", "workspaceDirectory"),
        ("/families/0", "language"),
        ("/families/0", "error"),
        ("/families/0", "expectedMutants"),
        ("/families/0", "baseline"),
    ] {
        let mut input = one();
        input
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(build_value(&input).is_err(), "{pointer}/{key}");
    }
    for pointer in [
        "",
        "/cleanup",
        "/cleanup/workspaceRecovery",
        "/cleanup/workspaceRecovery/skipped/0",
        "/families/0",
    ] {
        let mut input = one();
        *input.pointer_mut(pointer).unwrap() = json!([]);
        assert!(build_value(&input).is_err(), "{pointer}");
    }
    let mut input = one();
    input["cleanup"]["workspaceRecovery"]["skipped"][0] =
        json!({"campaign":"foreign","reason":"unknown_name"});
    assert!(
        build_value(&input).is_err(),
        "legacy public field is name, not campaign"
    );
}

#[test]
fn request_identity_selections_paths_language_and_attempt_shapes_are_validated() {
    for (pointer, replacement) in [
        ("/operation", json!("other")),
        ("/mode", json!("other")),
        ("/status", json!("running")),
        ("/runId", json!("abc")),
        ("/runId", json!("ABCDEF0123456789abcdef0123456789ab")),
        ("/output", json!("relative")),
        ("/output", json!("/bad\0path")),
        ("/cleanup/workspaceDirectory", json!("relative")),
        ("/families", json!([])),
        ("/families/0/family", json!("other")),
        ("/families/0/language", json!("dart")),
        ("/families/0/output", json!("relative")),
        ("/families/0/status", json!("other")),
        ("/families/0/baseline", json!([])),
        ("/families/0/originalControls", json!([null])),
    ] {
        let mut input = one();
        *input.pointer_mut(pointer).unwrap() = replacement;
        assert!(build_value(&input).is_err(), "{pointer}");
    }
    let mut input = request("direct", vec![family("language_specific", vec![])]);
    for language in [Value::Null, json!("rust")] {
        input["families"][0]["language"] = language;
        assert!(build_value(&input).is_err());
    }
    let classic = family("classic", vec![]);
    for mode in ["direct", "profile"] {
        assert!(build_value(&request(mode, vec![classic.clone(), classic.clone()])).is_err());
    }
    assert!(
        build_value(&request(
            "direct",
            vec![classic, family("failure_mode", vec![])]
        ))
        .is_err()
    );
}

#[test]
fn invalid_totals_and_false_success_are_rejected_without_rejecting_partial_counts() {
    for (pointer, replacement) in [
        ("/expectedMutants", json!(2)),
        ("/expectedMutants", Value::Null),
        ("/expectedMutants", json!(-1)),
        ("/expectedMutants", json!(true)),
        ("/expectedMutants", json!(1.0)),
        ("/families/0/expectedMutants", json!(0)),
        ("/families/0/expectedMutants", json!(2)),
        ("/families/0/expectedMutants", Value::Null),
        ("/families/0/error", json!("failed")),
        ("/families/0/status", json!("running")),
        ("/families/0/status", json!("no_mutants")),
        ("/error", json!("failed")),
        ("/status", json!("no_mutants")),
        ("/cleanup/originalProjectFilesUnchanged", json!(false)),
        ("/cleanup/temporarySnapshotCleaned", json!(false)),
        ("/cleanup/workspaceCleanupDeferred", json!(true)),
    ] {
        let mut input = one();
        *input.pointer_mut(pointer).unwrap() = replacement;
        assert!(build_value(&input).is_err(), "{pointer}");
    }
    let mut input = one();
    input["status"] = "failed".into();
    input["error"] = "interrupted".into();
    input["expectedMutants"] = 10.into();
    input["families"][0]["expectedMutants"] = 10.into();
    input["families"][0]["status"] = "failed".into();
    assert_eq!(build_value(&input).unwrap()["completedMutants"], 1);
    input["expectedMutants"] = 11.into();
    assert!(build_value(&input).is_err());

    let mut zero = request("direct", vec![family("classic", vec![])]);
    zero["status"] = "completed".into();
    assert!(build_value(&zero).is_err());
}

#[test]
fn aggregate_count_overflow_is_rejected_even_for_an_incomplete_profile() {
    let mut input = request(
        "profile",
        vec![family("classic", vec![]), family("failure_mode", vec![])],
    );
    input["status"] = "failed".into();
    input["expectedMutants"] = Value::Null;
    for family in input["families"].as_array_mut().unwrap() {
        family["status"] = "not_started".into();
        family["expectedMutants"] = u64::MAX.into();
    }
    assert!(build_value(&input).is_err());
}

#[test]
fn timings_reject_negative_nonfinite_wrong_types_and_typed_bypass() {
    for pointer in [
        "/wallSeconds",
        "/timings/snapshot",
        "/families/0/wallSeconds",
        "/families/0/timings/baseline",
    ] {
        for replacement in [json!(-0.1), json!("1"), json!(false), Value::Null] {
            let mut input = one();
            *input.pointer_mut(pointer).unwrap() = replacement;
            assert!(build_value(&input).is_err(), "{pointer}");
        }
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut input: Request = serde_json::from_value(one()).unwrap();
        input.wall_seconds = value;
        assert!(summarize(input).is_err());
        let mut input: Request = serde_json::from_value(one()).unwrap();
        input.families[0].timings.insert("bad".into(), value);
        assert!(summarize(input).is_err());
    }
    let mut input = one();
    input["wallSeconds"] = 0.into();
    input["timings"] = json!({});
    assert!(build_value(&input).is_ok());
}

#[test]
fn strict_json_rejects_duplicate_keys_invalid_numbers_and_malformed_documents() {
    let raw = serde_json::to_string(&one()).unwrap();
    for invalid in [
        String::new(),
        "null".into(),
        "{}{}".into(),
        raw.replacen(
            "\"operation\":",
            "\"operation\":\"campaign-report\",\"operation\":",
            1,
        ),
        raw.replacen(
            "\"reason\":\"assertion\"",
            "\"reason\":\"first\",\"reason\":\"assertion\"",
            1,
        ),
        raw.replacen("\"wallSeconds\":1.5", "\"wallSeconds\":NaN", 1),
        raw.replacen("\"wallSeconds\":1.5", "\"wallSeconds\":1e309", 1),
        raw.replacen("9007199254740993", "18446744073709551616", 1),
    ] {
        assert!(build(invalid.as_bytes()).is_err(), "{invalid}");
    }
}

#[test]
fn decimal_float_metadata_and_timings_round_trip_without_changing_their_bits() {
    let known = "1.5202790901741876e+169".parse::<f64>().unwrap();
    let mut input = one();
    input["wallSeconds"] = json!(known);
    input["families"][0]["records"][0]["opaque"]["number"] = json!(known);
    let value = build_value(&input).unwrap();
    assert_eq!(
        value["wallSeconds"].as_f64().unwrap().to_bits(),
        known.to_bits()
    );
    assert_eq!(
        value["records"][0]["opaque"]["number"]
            .as_f64()
            .unwrap()
            .to_bits(),
        known.to_bits()
    );
}

#[test]
fn seeded_finite_float_facts_preserve_exact_ieee_values_across_json_transport() {
    // Sample the entire finite binary64 range, including subnormals and signed
    // values, without depending on external RNG state or approximate equality.
    let mut seed = 0x9e3779b97f4a7c15_u64;
    let mut numbers = vec![f64::MIN_POSITIVE, f64::MAX, f64::from_bits(1), -0.0];
    for _ in 0..1024 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let value = f64::from_bits(seed);
        if value.is_finite() {
            numbers.push(value);
        }
    }
    let mut input = one();
    input["families"][0]["records"][0]["opaque"]["numbers"] = json!(numbers);
    input["timings"] = json!(
        numbers
            .iter()
            .enumerate()
            .map(|(index, value)| (format!("step-{index}"), value.abs()))
            .collect::<BTreeMap<_, _>>()
    );
    let value = build_value(&input).unwrap();
    for (index, expected) in numbers.into_iter().enumerate() {
        let actual = value["records"][0]["opaque"]["numbers"][index]
            .as_f64()
            .unwrap();
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "opaque float {index}: {expected}"
        );
        let actual = value["timings"][format!("step-{index}")].as_f64().unwrap();
        assert_eq!(
            actual.to_bits(),
            expected.abs().to_bits(),
            "timing float {index}: {expected}"
        );
    }
}

#[test]
fn profile_duplication_makes_the_response_bound_independent_of_the_request_bound() {
    let mut input = request(
        "profile",
        vec![family(
            "classic",
            vec![record("classic", "id", "KILLED", "assertion")],
        )],
    );
    input["families"][0]["records"][0]["opaque"]["capture"] = "x".repeat(4096).into();
    let input_bytes = serde_json::to_vec(&input).unwrap();
    let summary = build(&input_bytes).unwrap();
    let response =
        crate::private_response::encode(json!({"summary":summary,"terminal":"table"})).unwrap();
    assert!(response.len() > input_bytes.len());
    assert_eq!(response.last(), Some(&b'\n'));
    // The private_response writer, not input size, independently enforces the
    // existing response limit (including its final newline) before publication.
}
