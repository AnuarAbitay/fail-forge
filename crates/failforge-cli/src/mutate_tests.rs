use super::*;
use clap::Parser;

fn configuration() -> (Context, RunConfig) {
    let cwd = std::env::current_dir().unwrap();
    let context = Context::new(&cwd, &cwd).unwrap();
    let config = failforge_config::resolve_config(&json!({"family":"classic"}), &context).unwrap();
    (context, config)
}

#[test]
fn profile_selection_rejects_missing_literals_but_preserves_globs_and_direct_filters() {
    let (context, _) = configuration();
    for (include, valid) in [
        ("Cargo.toml", true),
        ("lib/no-such-file.dart", false),
        ("lib/no-such-directory/*.dart", true),
    ] {
        let profile = failforge_config::from_profile_mapping(
            &json!({"mutations":{"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}}},
                "rules":{"include":[include]}}),
            &context.cwd,
            &context,
            None,
        )
        .unwrap();
        assert_eq!(
            validate_profile_selection(&RunConfig::Profile(profile)).is_ok(),
            valid,
            "{include}"
        );
    }
    let direct = failforge_config::resolve_config(
        &json!({"family":"classic","include":["lib/no-such-file.dart"]}),
        &context,
    )
    .unwrap();
    validate_profile_selection(&direct).unwrap();
}

#[test]
fn direct_plan_keeps_omitted_operators_and_uses_no_python_handoff() {
    let (context, config) = configuration();
    let expected = config.to_value();
    let plan = prepare(
        config,
        &context.installation,
        &context.cwd.join("runner"),
        BTreeMap::new(),
        "a".repeat(32),
    )
    .unwrap();
    assert_eq!(plan.request.mode, crate::campaign_workspace::Mode::Direct);
    assert_eq!(plan.configuration, expected);
    assert_eq!(plan.families["classic"], expected);
    assert!(plan.request.families[0].rules.get("operators").is_none());
    assert_eq!(plan.request.project, context.cwd);
    assert!(plan.request.output.is_none());
    assert!(plan.request.registry.is_none());
}

#[test]
fn profile_plan_retains_canonical_family_order_and_shared_configuration() {
    let (context, _) = configuration();
    let config = failforge_config::from_profile_mapping(
        &json!({"mutations":{
            "language_specific":{"enabled":true,"language":"dart","operators":{"NSM-001":true}},
            "classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}}
        }}),
        &context.cwd,
        &context,
        None,
    )
    .unwrap();
    let expected = config.to_value();
    let plan = prepare(
        RunConfig::Profile(config),
        &context.installation,
        &context.cwd.join("runner"),
        BTreeMap::new(),
        "b".repeat(32),
    )
    .unwrap();
    assert_eq!(plan.request.mode, crate::campaign_workspace::Mode::Profile);
    assert_eq!(plan.configuration, expected);
    assert_eq!(
        plan.request
            .families
            .iter()
            .map(|family| family.family.as_str())
            .collect::<Vec<_>>(),
        ["classic", "language_specific"]
    );
    assert_eq!(plan.families["language_specific"]["language"], "dart");
}

#[test]
fn invalid_identity_is_rejected_before_any_execution() {
    let (context, config) = configuration();
    assert!(
        prepare(
            config,
            &context.installation,
            &context.cwd.join("runner"),
            BTreeMap::new(),
            "bad".into()
        )
        .is_err()
    );
    let one = identity().unwrap();
    let two = identity().unwrap();
    assert_ne!(one, two);
    assert_eq!(one.len(), 32);
    assert!(
        one.bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
}

#[test]
fn environment_preserves_user_cache_and_sets_noninteractive_sdk_flags() {
    let result = environment(
        Path::new("/not-installed"),
        [
            ("PUB_CACHE".into(), "/custom/cache".into()),
            ("CI".into(), "false".into()),
        ],
    );
    assert_eq!(result["PUB_CACHE"], "/custom/cache");
    for key in [
        "CI",
        "FLUTTER_SUPPRESS_ANALYTICS",
        "DART_SUPPRESS_ANALYTICS",
    ] {
        assert_eq!(result[key], "true");
    }
    assert!(!environment(Path::new("/not-installed"), []).contains_key("PUB_CACHE"));
}

#[test]
fn exit_codes_preserve_signal_cancellation_and_non_test_failures() {
    for signum in [None, Some(libc::SIGINT), Some(libc::SIGTERM)] {
        assert_eq!(
            exit_status(&json!({"status":"cancelled"}), signum),
            if signum == Some(libc::SIGTERM) {
                143
            } else {
                130
            }
        );
    }
    for outcome in ["KILLED", "SURVIVED", "TIMEOUT", "INVALID", "CANCELLED"] {
        assert_eq!(
            exit_status(
                &json!({"status":"completed","complete":true,"records":[{"outcome":outcome}]}),
                None
            ),
            0
        );
    }
    for outcome in ["INFRA_ERROR", "UNCLASSIFIED", "NO_TESTS"] {
        assert_eq!(
            exit_status(
                &json!({"status":"completed","complete":true,"records":[{"outcome":outcome}]}),
                None
            ),
            125
        );
    }
    assert_eq!(
        exit_status(
            &json!({"status":"no_mutants","complete":true,"records":[]}),
            None
        ),
        0
    );
    assert_eq!(
        exit_status(
            &json!({"status":"failed","complete":false,"records":[]}),
            None
        ),
        125
    );
}

#[test]
fn obsolete_interpreter_flag_is_rejected() {
    assert!(
        crate::args::Cli::try_parse_from([
            "failforge",
            "mutate",
            "--family",
            "classic",
            "--python",
            "anything"
        ])
        .is_err()
    );
}

#[cfg(windows)]
#[test]
fn explicit_child_environment_omits_hidden_drive_and_private_capability_keys() {
    let values = environment(
        Path::new(r"C:\installation"),
        [
            ("=C:".into(), r"C:\source".into()),
            ("=D:".into(), r"D:\other".into()),
            ("PATH".into(), r"C:\Windows\System32".into()),
            ("PUB_CACHE".into(), r"D:\pub-cache".into()),
            (
                "FAILFORGE_CLI_CANCEL_PIPE".into(),
                r"\\.\pipe\private".into(),
            ),
            (crate::campaign_controller::PARENT_ENV.into(), "123".into()),
        ],
    );
    assert!(!values.keys().any(|key| key.starts_with('=')));
    assert!(!values.contains_key("FAILFORGE_CLI_CANCEL_PIPE"));
    assert!(!values.contains_key(crate::campaign_controller::PARENT_ENV));
    assert_eq!(values["PATH"], r"C:\Windows\System32");
    assert_eq!(values["PUB_CACHE"], r"D:\pub-cache");
    assert_eq!(values["CI"], "true");
}

#[cfg(windows)]
#[test]
fn inherited_mixed_case_overrides_do_not_create_duplicate_child_keys() {
    let installation = std::env::temp_dir().join(format!("ff-env-{}", identity().unwrap()));
    std::fs::create_dir_all(installation.join(".toolchain/pub-cache")).unwrap();
    let values = environment(
        &installation,
        [
            ("ci".into(), "false".into()),
            ("Flutter_Suppress_Analytics".into(), "false".into()),
            ("dart_suppress_analytics".into(), "false".into()),
            ("pub_cache".into(), r"D:\caller-cache".into()),
            ("failforge_cli_cancel_pipe".into(), "private".into()),
        ],
    );
    assert_eq!(values["CI"], "true");
    assert_eq!(values["FLUTTER_SUPPRESS_ANALYTICS"], "true");
    assert_eq!(values["DART_SUPPRESS_ANALYTICS"], "true");
    assert_eq!(values["pub_cache"], r"D:\caller-cache");
    for expected in [
        "CI",
        "FLUTTER_SUPPRESS_ANALYTICS",
        "DART_SUPPRESS_ANALYTICS",
        "PUB_CACHE",
    ] {
        assert_eq!(
            values
                .keys()
                .filter(|key| key.eq_ignore_ascii_case(expected))
                .count(),
            1
        );
    }
    assert!(
        !values
            .keys()
            .any(|name| name.eq_ignore_ascii_case("FAILFORGE_CLI_CANCEL_PIPE"))
    );
    std::fs::remove_dir_all(installation).unwrap();
}
