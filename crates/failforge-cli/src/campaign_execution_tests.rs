use super::*;

fn value() -> Value {
    crate::test_support::paths(json!({"operation":"campaign","mode":"profile",
        "project":{"root":"/tmp/snapshot","name":"subject","packages":{"subject":"/tmp/snapshot"},
            "graph":{},"flutter":false,"frameworkCollision":false},
        "original":{"root":"/tmp/original","hashes":{}},"output":"/tmp/reports",
        "runtime":{"installation":"/tmp/tools","runner":"/tmp/tools/failforge",
            "dartSdk":"/tmp/sdk","flutterSdk":null,"adapterRoot":"/tmp/adapter"},
        "workspace":{"root":"/tmp/registry","campaign":"campaign-00000000000000000000000000000000"},
        "frozen":{},"environment":{"CI":"true"},"maxMutants":4,
        "families":[
            {"family":"language_specific","rules":{"operators":["NSM-001"]},"workers":1,
                "config":{"commandTimeout":"6s","testTimeout":"1s"}},
            {"family":"classic","rules":{"operators":["BOOLEAN_LITERAL_FLIP"]},"workers":2,
                "config":{"commandTimeout":"6s","testTimeout":"1s"}}]}))
}
fn request(value: &Value) -> Result<Request, NamespaceError> {
    decode(&serde_json::to_vec(value).unwrap())
}

#[test]
fn canonical_profile_order_and_direct_scope_keep_distinct_empty_and_limit_policies() {
    let mut input = value();
    let profile = request(&input).unwrap();
    assert_eq!(
        profile
            .families
            .iter()
            .map(|f| f.family.as_str())
            .collect::<Vec<_>>(),
        ["classic", "language_specific"]
    );
    let empty = family_request(&profile, &profile.families[0], Vec::new()).unwrap();
    assert_eq!(
        empty.empty_policy,
        crate::family_execution::EmptyPolicy::Skip
    );
    assert_eq!(
        empty.output,
        crate::test_support::absolute("/tmp/reports/families/classic")
    );
    input["mode"] = "direct".into();
    assert!(request(&input).is_err());
    input["families"].as_array_mut().unwrap().truncate(1);
    input["maxMutants"] = 0.into();
    let direct = request(&input).unwrap();
    let empty = family_request(&direct, &direct.families[0], Vec::new()).unwrap();
    assert_eq!(
        empty.empty_policy,
        crate::family_execution::EmptyPolicy::Controls
    );
    assert_eq!(empty.output, crate::test_support::absolute("/tmp/reports"));
}

#[test]
fn malformed_campaign_requests_fail_before_any_owner_can_start() {
    let original = value();
    for (pointer, bad) in [
        ("/operation", json!("family")),
        ("/mode", json!("other")),
        ("/maxMutants", json!(10001)),
        ("/maxMutants", json!(true)),
        ("/families", json!([])),
        ("/original/root", json!("relative")),
        ("/families/0/family", json!("classic")),
        ("/families/0/workers", json!(5)),
        ("/families/0/rules", json!([])),
        ("/families/0/rules/operators", json!("NSM-001")),
        ("/families/0/config/commandTimeout", json!("1s")),
        ("/original/hashes", json!([])),
        ("/environment", json!({"A=B":"x"})),
        ("/project", json!([])),
        ("/frozen", json!([])),
    ] {
        let mut bad_value = original.clone();
        *bad_value.pointer_mut(pointer).unwrap() = bad;
        assert!(request(&bad_value).is_err(), "{pointer}");
    }
    for pointer in [
        "",
        "/original",
        "/runtime",
        "/workspace",
        "/families/0",
        "/families/0/config",
        "/families/0/rules",
    ] {
        let mut bad = original.clone();
        bad.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), true.into());
        assert!(request(&bad).is_err(), "{pointer}");
    }
    assert!(decode(br#"{"operation":"campaign","operation":"campaign"}"#).is_err());
}

#[test]
fn component_selections_are_checked_even_when_discovery_has_zero_candidates() {
    let request = request(&value()).unwrap();
    let mut family = request.families[0].clone();
    let empty = json!({"steps":{"handshake":{"operators":[
        {"id":"BOOLEAN_LITERAL_FLIP","family":"classic"},{"id":"OTHER","family":"failure_mode"}]}},
        "candidates":[]});
    assert!(validate_selection(&empty, &family).is_ok());
    family.rules = json!({"components":[{"name":"group","operators":["OTHER"]}]});
    rules(&family.rules, false).unwrap();
    assert!(validate_selection(&empty, &family).is_err());
    family.rules = json!({"components":[{"operators":[]}]});
    assert!(rules(&family.rules, false).is_err());
    family.rules = json!({"components":[{"name":"group","components":[]}]});
    assert!(rules(&family.rules, false).is_err());
}

#[test]
fn discovery_identity_multiplicity_and_family_binding_are_checked() {
    let request = request(&value()).unwrap();
    let family = &request.families[0];
    let mut found = json!({"steps":{"handshake":{"operators":[{"id":"BOOLEAN_LITERAL_FLIP","family":"classic"}]}},
        "candidates":[{"mutant":{"id":"first","family":"classic"}}]});
    assert!(validate_selection(&found, family).is_ok());
    let duplicate = found["candidates"][0].clone();
    found["candidates"].as_array_mut().unwrap().push(duplicate);
    assert!(validate_selection(&found, family).is_err());
    found["candidates"].as_array_mut().unwrap().truncate(1);
    found["candidates"][0]["mutant"]["family"] = "failure_mode".into();
    assert!(validate_selection(&found, family).is_err());
}

#[test]
fn completion_requires_every_family_and_mutant_even_with_no_task_error() {
    let cancel = Cancellation::default();
    for (expected, mutants, families) in [(None, 0, 0), (Some(3), 2, 2), (Some(3), 3, 1)] {
        let ledger = Ledger {
            expected,
            mutants,
            families,
            infrastructure: false,
        };
        assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 2).0, "failed");
    }
    let empty = Ledger {
        expected: Some(0),
        families: 2,
        ..Default::default()
    };
    assert_eq!(
        terminal_state(Ok(()), &cancel, &empty, 2),
        ("no_mutants", None)
    );
    let complete = Ledger {
        expected: Some(3),
        mutants: 3,
        families: 2,
        infrastructure: false,
    };
    assert_eq!(
        terminal_state(Ok(()), &cancel, &complete, 2),
        ("completed", None)
    );
    cancel.request(libc::SIGTERM);
    assert_eq!(
        terminal_state(Ok(()), &cancel, &complete, 2),
        ("cancelled", None)
    );
}

#[test]
fn integrity_and_aggregate_failures_are_never_reclassified_as_cancellation() {
    let cancel = Cancellation::default();
    cancel.request(libc::SIGINT);
    let partial = Ledger {
        expected: Some(3),
        mutants: 1,
        families: 0,
        infrastructure: false,
    };
    let result = terminal_state(
        Err(invalid("original source changed")),
        &cancel,
        &partial,
        2,
    );
    assert_eq!(result.0, "failed");
    assert!(result.1.unwrap().contains("original source changed"));
    let infrastructure = Ledger {
        infrastructure: true,
        ..partial
    };
    assert_eq!(
        terminal_state(
            Err(NamespaceError::Cancelled(libc::SIGINT)),
            &cancel,
            &infrastructure,
            2
        )
        .0,
        "failed"
    );
    let excessive = Ledger {
        expected: Some(30000),
        ..Default::default()
    };
    let result = terminal_state(
        Err(invalid("profile mutant limit exceeded")),
        &cancel,
        &excessive,
        3,
    );
    assert_eq!(result.0, "failed");
    assert!(result.1.unwrap().contains("limit exceeded"));
    assert_eq!(excessive.expected, Some(30000));
}
