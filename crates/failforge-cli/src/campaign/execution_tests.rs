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
        crate::family::execution::EmptyPolicy::Skip
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
        crate::family::execution::EmptyPolicy::Controls
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
            ..Default::default()
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
        ..Default::default()
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
fn retained_mutant_infrastructure_error_does_not_make_a_complete_campaign_fail() {
    #[derive(Default)]
    struct Events(Vec<Value>);
    impl CampaignEvents for Events {
        fn emit_campaign(&mut self, event: Value) -> io::Result<()> {
            self.0.push(event);
            Ok(())
        }

        fn finish_campaign(
            &mut self,
            _status: &str,
            _error: Option<&str>,
            _completed_families: usize,
            _expected_families: usize,
            _completed_mutants: usize,
            _expected_mutants: Option<usize>,
        ) -> io::Result<()> {
            panic!("this test finalizes the family through its campaign sink")
        }
    }

    let cancel = Cancellation::default();
    let mut events = Events::default();
    let mut ledger = Ledger {
        expected: Some(1),
        ..Default::default()
    };
    {
        let mut sink = FamilySink {
            stream: &mut events,
            ledger: &mut ledger,
            family: "classic",
            started: Instant::now(),
            terminal: None,
            killed: 0,
            survived: 0,
        };
        sink.emit(json!({"kind":"mutant","record":{"outcome":"INFRA_ERROR"}}))
            .unwrap();
        sink.finish("completed", None, 1, 1).unwrap();
    }
    assert_eq!(ledger.mutants, 1);
    assert_eq!(ledger.families, 1);
    assert_eq!(events.0[0]["event"]["record"]["outcome"], "INFRA_ERROR");
    assert_eq!(
        terminal_state(Ok(()), &cancel, &ledger, 1),
        ("completed", None)
    );
    let failure = terminal_state(Err(invalid("original source changed")), &cancel, &ledger, 1);
    assert_eq!(failure.0, "failed");
    assert!(failure.1.unwrap().contains("original source changed"));
    cancel.request(libc::SIGTERM);
    assert_eq!(
        terminal_state(Ok(()), &cancel, &ledger, 1),
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
        ..Default::default()
    };
    let result = terminal_state(
        Err(invalid("original source changed")),
        &cancel,
        &partial,
        2,
    );
    assert_eq!(result.0, "failed");
    assert!(result.1.unwrap().contains("original source changed"));
    assert_eq!(
        terminal_state(
            Err(NamespaceError::Cancelled(libc::SIGINT)),
            &cancel,
            &partial,
            2
        )
        .0,
        "cancelled"
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

#[test]
fn quality_policy_is_threaded_to_family_and_terminal_preserves_error_priority() {
    let mut input = value();
    input["families"][1]["qualityGate"] = json!({"minScore":80,"stopWhenUnreachable":true});
    let campaign = request(&input).unwrap();
    let family = family_request(&campaign, &campaign.families[0], Vec::new()).unwrap();
    assert_eq!(family.quality_gate.min_score.unwrap().percent(), 80.0);
    assert!(family.quality_gate.stop_when_unreachable);
    let ledger = Ledger {
        expected: Some(10),
        mutants: 3,
        families: 0,
        quality_stopped: true,
    };
    let cancel = Cancellation::default();
    assert_eq!(
        terminal_state(Ok(()), &cancel, &ledger, 2),
        ("quality_stopped", None)
    );
    assert_eq!(
        terminal_state(Err(invalid("source changed")), &cancel, &ledger, 2).0,
        "failed"
    );
    cancel.request(libc::SIGINT);
    assert_eq!(terminal_state(Ok(()), &cancel, &ledger, 2).0, "cancelled");
}
