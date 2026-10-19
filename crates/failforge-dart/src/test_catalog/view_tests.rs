use super::*;
use serde_json::json;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

fn events() -> Vec<Value> {
    vec![
        json!({"type":"start","time":0,"protocolVersion":"0.1.1","runnerVersion":"1","pid":100}),
        json!({"type":"suite","time":1,"suite":{"id":0,"platform":"vm","path":"test/a.dart"}}),
        json!({"type":"group","time":2,"group":{"id":1,"suiteID":0,"parentID":null,"name":"","testCount":2,"metadata":{"skip":false},"line":null,"column":null,"url":null}}),
        json!({"type":"testStart","time":3,"test":{"id":9,"suiteID":0,"groupIDs":[1],"name":"z author","metadata":{"skip":false},"line":10,"column":3,"url":"file:///work/test/a.dart"}}),
        json!({"type":"testStart","time":4,"test":{"id":3,"suiteID":0,"groupIDs":[1],"name":"a author","metadata":{"skip":false},"line":20,"column":3,"url":"test/a.dart"}}),
        json!({"type":"testDone","time":5,"testID":3,"result":"success","hidden":false,"skipped":false}),
        json!({"type":"testDone","time":6,"testID":9,"result":"success","hidden":false,"skipped":false}),
        json!({"type":"allSuites","time":7,"count":1}),
        json!({"type":"done","time":8,"success":true}),
    ]
}

fn raw(events: &[Value]) -> Vec<u8> {
    events
        .iter()
        .flat_map(|event| {
            let mut line = serde_json::to_vec(event).unwrap();
            line.push(b'\n');
            line
        })
        .collect()
}

fn snapshot(events: &[Value]) -> CatalogSnapshot {
    analyze_catalogue(&raw(events), Path::new("/work"), ReporterMode::Dart).unwrap()
}

fn row<'a>(events: &'a mut [Value], kind: &str) -> &'a mut Value {
    events
        .iter_mut()
        .find(|event| event["type"] == kind)
        .unwrap()
}

fn retime(events: &mut [Value]) {
    for (index, event) in events.iter_mut().enumerate() {
        event["time"] = json!(index);
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ff-catalog-view-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn metadata_keys_are_canonical_recursively() {
    let value = serde_json::json!({"z":1,"a":{"y":true,"b":false}});
    assert_eq!(
        canonical_metadata(&value),
        r#"{"a":{"b":false,"y":true},"z":1}"#
    );
}

#[test]
fn catalogue_preserves_duplicate_multiplicity_and_reporter_order() {
    let original = events();
    let capture = raw(&original);
    let first = snapshot(&original);
    assert_eq!(first.raw_sha256, format!("{:x}", Sha256::digest(&capture)));
    assert_eq!(first.cwd, "/work");
    assert_eq!(
        first.identities,
        [["test/a.dart", "a author"], ["test/a.dart", "z author"]]
    );
    let view = first.view.unwrap();
    assert!(view.success);
    assert_eq!(view.failed, 0);
    assert_eq!(view.suites, [SuiteKey("test/a.dart".into(), "vm".into())]);
    assert_eq!(
        view.tests
            .iter()
            .map(|test| test.done.test_id)
            .collect::<Vec<_>>(),
        [9, 3]
    );
    assert_eq!(
        view.tests[0].location,
        Location(Some(10), Some(3), Some("test/a.dart".into()))
    );
    assert_eq!(view.tests[0].key.chain, view.groups[0].key.chain);

    let mut duplicate = events();
    duplicate[4]["test"]["name"] = json!("z author");
    let snapshot = snapshot(&duplicate);
    assert_eq!(
        snapshot.identities,
        [["test/a.dart", "z author"], ["test/a.dart", "z author"]]
    );
    assert_eq!(snapshot.view.unwrap().tests.len(), 2);
}

#[test]
fn complete_loading_failure_retains_observed_authors_without_strict_view() {
    let mut events = events();
    let loader = vec![
        json!({"type":"testStart","test":{"id":10,"suiteID":0,"groupIDs":[],"name":"loading test/a.dart","metadata":{"skip":false}}}),
        json!({"type":"error","testID":10,"error":"Failed to load test/a.dart: StateError: top level","stackTrace":"test/a.dart","isFailure":false}),
        json!({"type":"testDone","testID":10,"result":"error","hidden":true,"skipped":false}),
    ];
    events.splice(7..7, loader);
    events.last_mut().unwrap()["success"] = json!(false);
    retime(&mut events);
    let snapshot = snapshot(&events);
    assert_eq!(snapshot.identities.len(), 2);
    assert!(snapshot.view.is_none());
}

#[test]
fn diagnostic_timeout_blocks_strict_view_but_keeps_actual_names() {
    let mut events = events();
    events.insert(5,json!({"type":"error","testID":3,"error":"TimeoutException after 0:00:01.000000: Test timed out after 1 seconds.","stackTrace":"dart:isolate _RawReceivePort._handleMessage\n","isFailure":false}));
    events[6]["result"] = json!("error");
    events.last_mut().unwrap()["success"] = json!(false);
    retime(&mut events);
    for event in &mut events[5..] {
        event["time"] = json!(event["time"].as_u64().unwrap() + 1000);
    }
    let snapshot = snapshot(&events);
    assert_eq!(snapshot.identities.len(), 2);
    assert!(snapshot.view.is_none());
}

#[test]
fn malformed_or_incomplete_protocol_never_supplies_identities() {
    let mut truncated = events();
    truncated.pop();
    assert_eq!(
        analyze_catalogue(&raw(&truncated), Path::new("/work"), ReporterMode::Dart)
            .unwrap_err()
            .0,
        "missing_done_event"
    );
    let mut bad = events();
    bad[4]["test"]["id"] = json!(9);
    assert_eq!(
        analyze_catalogue(&raw(&bad), Path::new("/work"), ReporterMode::Dart)
            .unwrap_err()
            .0,
        "duplicate_event_id"
    );
    for raw in [
        b"not JSON".as_slice(),
        br#"{"type":"start","type":"start"}"#,
        b"\xff",
    ] {
        assert!(analyze_catalogue(raw, Path::new("/work"), ReporterMode::Dart).is_err());
    }
    let blanks = vec![b'\n'; MAX_EVENTS];
    assert_eq!(
        analyze_catalogue(&blanks, Path::new("/work"), ReporterMode::Dart)
            .unwrap_err()
            .0,
        "event_limit_exceeded"
    );
}

#[test]
fn flutter_tool_events_and_unicode_line_separators_do_not_add_identities() {
    let mut events = events();
    events[0]["runnerVersion"] = Value::Null;
    events[3]["test"]["name"] = json!("before\u{2028}middle\u{2029}after");
    events.insert(
        1,
        json!([{"event":"test.startedProcess","params":{"vmServiceUri":null}}]),
    );
    let bytes = raw(&events);
    let snapshot = analyze_catalogue(&bytes, Path::new("/work"), ReporterMode::Flutter).unwrap();
    assert_eq!(snapshot.identities.len(), 2);
    assert!(snapshot.view.is_some());
    assert_eq!(
        snapshot.identities[1][1],
        "before\u{2028}middle\u{2029}after"
    );
    assert!(analyze_catalogue(&bytes, Path::new("/work"), ReporterMode::Dart).is_err());
}

#[test]
fn broad_identity_path_normalization_matches_legacy_lexical_paths() {
    for (path, expected) in [
        ("/work/test/../test/a.dart", "test/a.dart"),
        ("/other/test/a.dart", "../other/test/a.dart"),
        ("./test//a.dart", "test/a.dart"),
        ("test/../test/a.dart", "test/../test/a.dart"),
        ("file:///work/test/a.dart", "file:/work/test/a.dart"),
        ("", "."),
    ] {
        let mut events = events();
        row(&mut events, "suite")["suite"]["path"] = json!(path);
        let snapshot = snapshot(&events);
        assert_eq!(snapshot.identities[0][0], expected, "{path}");
    }
}

#[test]
fn strict_local_uri_decodes_unicode_spaces_and_escaped_separators() {
    let cwd = Path::new("/work");
    for (input, expected) in [
        ("test/a.dart", "test/a.dart"),
        ("/work/test/a.dart", "test/a.dart"),
        ("file:///work/test/a.dart", "test/a.dart"),
        ("file://localhost/work/test/a%20b.dart", "test/a b.dart"),
        ("file:test/a.dart", "test/a.dart"),
        (
            "file:///work/test/%D1%82%D0%B5%D1%81%D1%82.dart",
            "test/тест.dart",
        ),
        ("file:///work/test%2Fa.dart?ignored#fragment", "test/a.dart"),
    ] {
        assert_eq!(relative(input, cwd).unwrap(), expected, "{input}");
    }
    assert_eq!(
        path_uri(Path::new("/work/test/тест a#b.dart")).unwrap(),
        "file:///work/test/%D1%82%D0%B5%D1%81%D1%82%20a%23b.dart"
    );
    assert!(path_uri(Path::new("relative/a.dart")).is_err());
}

#[test]
fn source_paths_reject_nonlocal_uris_escape_invalid_encoding_and_non_authors() {
    for input in [
        "",
        "test/a\0.dart",
        "../test/a.dart",
        "/other/test/a.dart",
        "lib/a.dart",
        "test/a.txt",
        "test/.dart",
        "file://host/work/test/a.dart",
        "file://LOCALHOST/work/test/a.dart",
        "file://user@localhost/work/test/a.dart",
        "file:///work/test/%00.dart",
        "file:///work/test/%FF.dart",
        "file:///work/test/%zz.dart",
        "file:///work/test/%",
        "file:///work/test/%2E%2E/lib/a.dart",
    ] {
        assert!(relative(input, Path::new("/work")).is_err(), "{input:?}");
    }
}

#[test]
fn existing_symlink_prefixes_are_resolved_even_when_suffix_or_baseline_is_deleted() {
    let fixture = Fixture::new();
    let project = fixture.0.join("project");
    let outside = fixture.0.join("outside");
    std::fs::create_dir_all(project.join("test")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, project.join("test/escape")).unwrap();
    assert!(relative("test/escape/missing.dart", &project).is_err());
    symlink("test", project.join("alias")).unwrap();
    assert_eq!(
        relative("alias/missing.dart", &project).unwrap(),
        "test/missing.dart"
    );
    symlink("loop", project.join("loop")).unwrap();
    assert!(resolve_existing(&project.join("loop/a.dart")).is_err());
    let deleted = project.join("missing/../test/a.dart");
    assert_eq!(
        resolve_existing(&deleted).unwrap(),
        project.join("test/a.dart")
    );
    std::fs::remove_dir_all(&project).unwrap();
    assert_eq!(relative("test/a.dart", &project).unwrap(), "test/a.dart");
    assert_eq!(resolve_existing(&project).unwrap(), project);
}

#[test]
fn absolute_cwd_preserves_logical_alias_for_stack_matching_and_rejects_nul() {
    let fixture = Fixture::new();
    let real = fixture.0.join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = fixture.0.join("alias");
    symlink(&real, &alias).unwrap();
    let events = events();
    let snapshot = analyze_catalogue(&raw(&events), &alias, ReporterMode::Dart).unwrap();
    assert_eq!(snapshot.cwd, alias.to_str().unwrap());
    // Fixture source locations deliberately point at /work, so only the broad
    // identities remain usable for this alternate cwd.
    assert!(snapshot.view.is_none());
    assert!(analyze_catalogue(&raw(&events), Path::new("/work\0"), ReporterMode::Dart).is_err());
}

#[test]
fn ambiguous_suites_groups_and_noncanonical_test_chains_withhold_strict_view() {
    let mut duplicate_suite = events();
    duplicate_suite.insert(
        2,
        json!({"type":"suite","suite":{"id":20,"platform":"vm","path":"./test/a.dart"}}),
    );
    row(&mut duplicate_suite, "allSuites")["count"] = json!(2);
    retime(&mut duplicate_suite);
    assert!(snapshot(&duplicate_suite).view.is_none());
    let mut duplicate_group = events();
    let mut group = duplicate_group[2].clone();
    group["group"]["id"] = json!(20);
    duplicate_group.insert(3, group);
    retime(&mut duplicate_group);
    assert!(snapshot(&duplicate_group).view.is_none());
    let mut chain = events();
    let mut group = chain[2].clone();
    group["group"]["id"] = json!(20);
    group["group"]["parentID"] = json!(1);
    group["group"]["name"] = json!("nested");
    chain.insert(3, group);
    chain[4]["test"]["groupIDs"] = json!([20]);
    retime(&mut chain);
    assert!(snapshot(&chain).view.is_none());
    chain[4]["test"]["groupIDs"] = json!([1, 20]);
    assert_eq!(snapshot(&chain).view.unwrap().tests[0].chain.len(), 2);
}

#[test]
fn absent_or_invalid_group_metadata_and_source_locations_withhold_strict_view() {
    for (field, value) in [("metadata", Value::Null), ("parentID", json!(false))] {
        let mut changed = events();
        changed[2]["group"][field] = value;
        if field == "parentID" {
            assert!(
                analyze_catalogue(&raw(&changed), Path::new("/work"), ReporterMode::Dart).is_err()
            );
        } else {
            assert!(snapshot(&changed).view.is_none());
        }
    }
    for (field, value) in [
        ("line", json!(0)),
        ("line", json!(true)),
        ("line", json!(1.0)),
        ("column", Value::Null),
        ("url", json!("test/b.dart")),
        ("url", Value::Null),
    ] {
        let mut changed = events();
        changed[3]["test"][field] = value;
        assert!(snapshot(&changed).view.is_none(), "{field}");
    }
    let mut missing = events();
    missing[2]["group"]
        .as_object_mut()
        .unwrap()
        .remove("parentID");
    assert!(snapshot(&missing).view.is_none());
}

#[test]
fn snapshots_roundtrip_and_reject_unknown_fields() {
    let original = snapshot(&events());
    let encoded = serde_json::to_value(&original).unwrap();
    let decoded: CatalogSnapshot = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded.identities, original.identities);
    assert_eq!(
        decoded.view.unwrap().tests[0].key,
        original.view.unwrap().tests[0].key
    );
    let mut malformed = encoded;
    malformed["view"]["tests"][0]["done"]["injected"] = json!(true);
    assert!(serde_json::from_value::<CatalogSnapshot>(malformed).is_err());
}

#[test]
fn budget_rejects_graph_growth_before_expanding_shared_descriptors() {
    let mut budget = Budget::new();
    budget.consume(MAX_GRAPH_BYTES).unwrap();
    assert!(budget.consume(1).is_err());
    let mut budget = Budget::new();
    assert!(budget.consume(usize::MAX).is_err());
    // A single moderately sized suite name reused across many authored tests
    // must be bounded even though the reporter's input remains much smaller.
    let long_path = "x".repeat(1024 * 1024);
    let mut rows = Rows::new();
    rows.suites
        .push((0, json!({"path":long_path}).as_object().unwrap().clone()));
    for id in 0..33 {
        rows.tests.push((
            id,
            json!({"suiteID":0,"groupIDs":[1],"name":"author"})
                .as_object()
                .unwrap()
                .clone(),
        ));
    }
    assert_eq!(
        identities(&rows, Path::new("/work")).unwrap_err().0,
        "catalogue_graph_limit_exceeded"
    );
}

#[test]
fn deep_group_ancestry_is_bounded_without_discarding_observed_identities() {
    let mut capture = events();
    let root = capture[2].clone();
    let mut nested = Vec::new();
    for id in 20..1020 {
        let mut group = root.clone();
        group["group"]["id"] = json!(id);
        group["group"]["parentID"] = json!(if id == 20 { 1 } else { id - 1 });
        group["group"]["name"] = json!(format!("nested-{id}"));
        nested.push(group);
    }
    capture.splice(3..3, nested);
    retime(&mut capture);
    let bytes = raw(&capture);
    assert!(bytes.len() < 1024 * 1024);
    let snapshot = analyze_catalogue(&bytes, Path::new("/work"), ReporterMode::Dart).unwrap();
    assert_eq!(snapshot.identities.len(), 2);
    assert!(snapshot.view.is_none());
}

#[test]
fn repeated_large_metadata_is_bounded_before_cloning_per_test() {
    let mut capture = events();
    capture[2]["group"]["metadata"]["extra"] = json!("x".repeat(1024 * 1024));
    let start = capture[3].clone();
    let done = capture[6].clone();
    let mut authors = Vec::new();
    for id in 20..40 {
        let mut start = start.clone();
        start["test"]["id"] = json!(id);
        authors.push(start);
        let mut done = done.clone();
        done["testID"] = json!(id);
        authors.push(done);
    }
    capture.splice(7..7, authors);
    retime(&mut capture);
    let bytes = raw(&capture);
    assert!(bytes.len() < 2 * 1024 * 1024);
    let snapshot = analyze_catalogue(&bytes, Path::new("/work"), ReporterMode::Dart).unwrap();
    assert_eq!(snapshot.identities.len(), 22);
    assert!(snapshot.view.is_none());
}

#[test]
fn strict_context_json_shares_duplicate_numeric_and_size_checks() {
    use crate::test_reporter::parse_json;
    assert_eq!(parse_json(b"{}", 2).unwrap(), json!({}));
    assert_eq!(parse_json(b"{}", 1).unwrap_err().0, "event_limit_exceeded");
    assert_eq!(
        parse_json(br#"{"ignored":{"x":1,"x":2}}"#, 100)
            .unwrap_err()
            .0,
        "duplicate_json_key"
    );
    assert_eq!(
        parse_json(br#"{"ignored":1e400}"#, 100).unwrap_err().0,
        "non_json_number"
    );
}

#[test]
fn ordinary_failure_witness_accepts_frames_using_logical_symlink_cwd() {
    use crate::test_catalog::{SelectionContext, apply_selection};
    let fixture = Fixture::new();
    let real = fixture.0.join("real");
    std::fs::create_dir(&real).unwrap();
    let logical = fixture.0.join("logical");
    symlink(&real, &logical).unwrap();
    let captures = |failed: bool| {
        let mut rows =
            vec![json!({"type":"start","protocolVersion":"0.1.1","runnerVersion":"1","pid":1})];
        for (suite, group, path, count) in [(0, 2, "test/a.dart", 1), (1, 4, "test/b.dart", 1)] {
            rows.push(json!({"type":"suite","suite":{"id":suite,"platform":"vm","path":path}}));
            rows.push(json!({"type":"group","group":{"id":group,"suiteID":suite,"parentID":null,"name":"","testCount":count,"metadata":{"skip":false}}}));
        }
        for (id, name, suite, group, path, hook) in [
            (10, "(setUpAll)", 0, 2, "test/a.dart", true),
            (11, "missing body", 0, 2, "test/a.dart", false),
            (12, "outside", 1, 4, "test/b.dart", false),
        ] {
            if failed && id == 11 {
                continue;
            }
            rows.push(json!({"type":"testStart","test":{"id":id,"suiteID":suite,"groupIDs":[group],"name":name,"metadata":{"skip":false},"line":10,"column":1,"url":path_uri(&logical.join(path)).unwrap()}}));
            if failed {
                rows.push(json!({"type":"error","testID":id,"error":"StateError: subject failed","stackTrace":format!("package:subject/a.dart 1:1 method\n{} 11:1 author\n",logical.join(path).display()),"isFailure":false}));
            }
            rows.push(json!({"type":"testDone","testID":id,"result":if failed {"error"} else {"success"},"hidden":hook&&!failed,"skipped":false}));
        }
        rows.push(json!({"type":"allSuites","count":2}));
        rows.push(json!({"type":"done","success":!failed}));
        retime(&mut rows);
        raw(&rows)
    };
    let baseline = analyze_catalogue(&captures(false), &logical, ReporterMode::Dart).unwrap();
    let current = analyze_catalogue(&captures(true), &logical, ReporterMode::Dart).unwrap();
    let context = SelectionContext {
        expected: Some(baseline.identities.clone()),
        baseline: Some(baseline),
        mutant: Some(json!({"file":"lib/a.dart"})),
        alias: "subject".into(),
    };
    let mut result = json!({"outcome":"KILLED","reason":"user_test_failure"});
    apply_selection(&mut result, &current, &context).unwrap();
    assert_eq!(result["outcome"], "KILLED", "{result}");
    assert_eq!(
        result["reason"],
        "ordinary_author_failure_with_setup_suppression"
    );
}
