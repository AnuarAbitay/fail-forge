//! Pure normalization shared by explicit command settings and YAML profiles.

use crate::catalogue::{DART_OPERATOR_GROUPS, DART_OPERATORS, FAMILIES, operators};
use crate::duration::resolve_timeout_policy;
use crate::models::{
    CampaignComponent, CampaignConfig, CampaignRules, MutationSelection, ProfileConfig,
    validate_strings,
};
use crate::paths::resolve_path;
use crate::{ConfigError, Context, MAX_RULE_VALUES};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const ROOT_KEYS: &[&str] = &[
    "project",
    "family",
    "workers",
    "timeouts",
    "rules",
    "limits",
    "dart_sdk",
    "flutter_sdk",
    "adapter_root",
    "output",
    "language",
    "mutations",
];
const PROFILE_KEYS: &[&str] = &[
    "workers",
    "timeouts",
    "rules",
    "limits",
    "dart_sdk",
    "flutter_sdk",
    "adapter_root",
    "output",
    "mutations",
];
const RULE_KEYS: &[&str] = &[
    "operators",
    "include",
    "exclude",
    "classes",
    "methods",
    "components",
];

fn object<'a>(
    value: &'a Value,
    name: &str,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>, ConfigError> {
    let values = value
        .as_object()
        .ok_or_else(|| ConfigError::new(format!("{name} must be a mapping")))?;
    let mut unknown: Vec<_> = values
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(ConfigError::new(format!(
            "{name} has unknown settings: {}",
            unknown.join(", ")
        )));
    }
    Ok(values)
}

fn path_value(path: &Path, name: &str) -> Result<Value, ConfigError> {
    path.to_str()
        .map(|value| json!(value))
        .ok_or_else(|| ConfigError::new(format!("{name} must contain valid Unicode")))
}

fn context_paths(
    context: &Context,
    base_dir: Option<&Path>,
) -> Result<(PathBuf, PathBuf, PathBuf), ConfigError> {
    let process_cwd =
        std::env::current_dir().map_err(|_| ConfigError::new("cwd cannot be resolved"))?;
    let current = resolve_path(&path_value(&context.cwd, "cwd")?, "cwd", &process_cwd)?;
    let installed = resolve_path(
        &path_value(&context.installation, "installation")?,
        "installation",
        &current,
    )?;
    let base = if let Some(base) = base_dir {
        resolve_path(
            &path_value(base, "configuration directory")?,
            "configuration directory",
            &current,
        )?
    } else {
        current.clone()
    };
    Ok((current, installed, base))
}

fn integer(value: &Value, name: &str, minimum: u32, maximum: u32) -> Result<u32, ConfigError> {
    let number = value
        .as_u64()
        .filter(|number| *number >= u64::from(minimum) && *number <= u64::from(maximum));
    number.map(|number| number as u32).ok_or_else(|| {
        ConfigError::new(format!(
            "{name} must be an integer between {minimum} and {maximum}"
        ))
    })
}

fn boolean(value: &Value, name: &str) -> Result<bool, ConfigError> {
    value
        .as_bool()
        .ok_or_else(|| ConfigError::new(format!("{name} must be a boolean true or false")))
}

fn strings(value: &Value, name: &str, operator_ids: bool) -> Result<Vec<String>, ConfigError> {
    let values = value
        .as_array()
        .filter(|values| values.len() <= MAX_RULE_VALUES)
        .ok_or_else(|| ConfigError::new(format!("{name} must be a bounded list of strings")))?;
    let values: Vec<String> = values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                ConfigError::new(format!("{name} must contain nonempty strings without NUL"))
            })
        })
        .collect::<Result<_, _>>()?;
    validate_strings(&values, name, operator_ids)?;
    Ok(values)
}

fn rules(values: &Map<String, Value>, component: bool) -> Result<CampaignRules, ConfigError> {
    let mut components = vec![];
    if !component {
        if let Some(entries) = values.get("components") {
            let entries = entries
                .as_array()
                .filter(|entries| entries.len() <= MAX_RULE_VALUES)
                .ok_or_else(|| ConfigError::new("rules.components must be a bounded list"))?;
            let mut names = HashSet::new();
            for entry in entries {
                let entry = object(
                    entry,
                    "component",
                    &[
                        "operators",
                        "include",
                        "exclude",
                        "classes",
                        "methods",
                        "name",
                    ],
                )?;
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty() && !name.contains('\0') && name.len() <= 512)
                    .ok_or_else(|| {
                        ConfigError::new("component names must be nonempty, unique and bounded")
                    })?;
                if !names.insert(name) {
                    return Err(ConfigError::new(
                        "component names must be nonempty, unique and bounded",
                    ));
                }
                components.push(CampaignComponent {
                    name: name.into(),
                    rules: rules(entry, true)?,
                });
            }
        }
    }
    let optional = |key: &str, operator_ids| {
        values
            .get(key)
            .map(|value| strings(value, &format!("rules.{key}"), operator_ids))
            .transpose()
    };
    Ok(CampaignRules {
        operators: optional("operators", true)?,
        include: if values.contains_key("include") {
            optional("include", false)?
        } else if component {
            None
        } else {
            Some(vec!["lib/**.dart".into()])
        },
        exclude: values
            .get("exclude")
            .map(|value| strings(value, "rules.exclude", false))
            .transpose()?
            .unwrap_or_default(),
        classes: optional("classes", false)?,
        methods: optional("methods", false)?,
        components,
    })
}

fn dart_rules(mut rules: CampaignRules, mutations: &Value) -> Result<CampaignRules, ConfigError> {
    let groups = object(mutations, "mutations", &["dart_semantic"])?;
    let empty = json!({});
    let settings = object(
        groups.get("dart_semantic").unwrap_or(&empty),
        "mutations.dart_semantic",
        &[
            "enabled",
            "exclude",
            "null_safety",
            "parameters",
            "switch",
            "patterns",
            "collections",
            "enums",
        ],
    )?;
    for key in std::iter::once("enabled").chain(DART_OPERATOR_GROUPS.iter().map(|(key, _)| *key)) {
        if settings.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(ConfigError::new(format!(
                "mutations.dart_semantic.{key} must be a boolean"
            )));
        }
    }
    for selected in std::iter::once(&rules.operators).chain(
        rules
            .components
            .iter()
            .map(|component| &component.rules.operators),
    ) {
        if selected
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| !DART_OPERATORS.contains(&id.as_str())))
        {
            return Err(ConfigError::new(
                "language_specific/dart requires Dart semantic operator IDs",
            ));
        }
    }
    let mut enabled = HashSet::new();
    if settings
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        for (group, ids) in DART_OPERATOR_GROUPS {
            if settings
                .get(*group)
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                enabled.extend(ids.iter().copied());
            }
        }
    }
    let selected = rules
        .operators
        .take()
        .unwrap_or_else(|| DART_OPERATORS.iter().map(|id| (*id).to_owned()).collect());
    rules.operators = Some(
        selected
            .into_iter()
            .filter(|id| enabled.contains(id.as_str()))
            .collect(),
    );
    let exclusions = settings
        .get("exclude")
        .map(|value| strings(value, "mutations.dart_semantic.exclude", false))
        .transpose()?
        .unwrap_or_default();
    let mut seen = HashSet::new();
    rules.exclude = rules
        .exclude
        .into_iter()
        .chain(exclusions)
        .filter(|value| seen.insert(value.clone()))
        .collect();
    Ok(rules)
}

pub fn from_mapping(
    value: &Value,
    context: &Context,
    base_dir: Option<&Path>,
) -> Result<CampaignConfig, ConfigError> {
    let (_, installed, base) = context_paths(context, base_dir)?;
    let values = object(value, "campaign", ROOT_KEYS)?;
    let family = values
        .get("family")
        .and_then(Value::as_str)
        .filter(|family| FAMILIES.contains(family))
        .ok_or_else(|| {
            ConfigError::new(
                "family is required and must be classic, failure_mode or language_specific",
            )
        })?;
    let language = if family == "language_specific" {
        let language = values.get("language").unwrap_or(&Value::Null);
        if values.contains_key("language") && language.as_str() != Some("dart") {
            return Err(ConfigError::new(
                "language_specific currently supports language dart only",
            ));
        }
        Some("dart".to_owned())
    } else {
        if values.contains_key("language") || values.contains_key("mutations") {
            return Err(ConfigError::new(
                "language and mutations settings require family language_specific",
            ));
        }
        None
    };
    let project = match values.get("project") {
        Some(project) => resolve_path(project, "project", &base)?,
        None => resolve_path(&path_value(&base, "project")?, "project", &base)?,
    };
    let workers = integer(values.get("workers").unwrap_or(&json!(1)), "workers", 1, 4)?;
    let empty = json!({});
    let timeouts = object(
        values.get("timeouts").unwrap_or(&empty),
        "timeouts",
        &["test", "command"],
    )?;
    let policy = resolve_timeout_policy(
        timeouts.get("command").unwrap_or(&json!("120s")),
        timeouts.get("test").unwrap_or(&json!("30s")),
    )?;
    let limits = object(
        values.get("limits").unwrap_or(&empty),
        "limits",
        &["max_mutants"],
    )?;
    let max_mutants = integer(
        limits.get("max_mutants").unwrap_or(&json!(1000)),
        "max_mutants",
        0,
        10000,
    )?;
    let rule_values = object(values.get("rules").unwrap_or(&empty), "rules", RULE_KEYS)?;
    let mut rules = rules(rule_values, false)?;
    if family == "language_specific" {
        rules = dart_rules(rules, values.get("mutations").unwrap_or(&empty))?;
    }
    let dart_value = values.get("dart_sdk").filter(|value| !value.is_null());
    let flutter_value = values.get("flutter_sdk").filter(|value| !value.is_null());
    if dart_value.is_some() && flutter_value.is_some() {
        return Err(ConfigError::new(
            "dart_sdk and flutter_sdk are mutually exclusive inputs",
        ));
    }
    let flutter_sdk = flutter_value
        .map(|value| resolve_path(value, "flutter_sdk", &base))
        .transpose()?;
    let dart_sdk = if let Some(value) = dart_value {
        resolve_path(value, "dart_sdk", &base)?
    } else if let Some(flutter) = &flutter_sdk {
        flutter.join("bin/cache/dart-sdk")
    } else {
        installed.join(".toolchain/flutter/bin/cache/dart-sdk")
    };
    let adapter_default = path_value(&installed.join("adapters/dart"), "adapter_root")?;
    let adapter_root = resolve_path(
        values.get("adapter_root").unwrap_or(&adapter_default),
        "adapter_root",
        &base,
    )?;
    let output = values
        .get("output")
        .filter(|value| !value.is_null())
        .map(|value| resolve_path(value, "output", &base))
        .transpose()?;
    Ok(CampaignConfig {
        project,
        family: family.into(),
        workers,
        test_timeout: policy.test_timeout(),
        command_timeout: policy.runner_timeout(),
        rules,
        max_mutants,
        dart_sdk,
        flutter_sdk,
        adapter_root,
        output,
        language,
    })
}

pub fn from_profile_mapping(
    value: &Value,
    project: &Path,
    context: &Context,
    base_dir: Option<&Path>,
) -> Result<ProfileConfig, ConfigError> {
    let (current, installed, base) = context_paths(context, base_dir)?;
    let project = resolve_path(&path_value(project, "project")?, "project", &base)?;
    let values = object(value, "profile", PROFILE_KEYS)?;
    let empty = json!({});
    object(
        values.get("rules").unwrap_or(&empty),
        "rules",
        &["include", "exclude", "classes", "methods"],
    )?;
    let mutations = object(
        values.get("mutations").unwrap_or(&empty),
        "mutations",
        FAMILIES,
    )?;
    let mut selections = vec![];
    for family in FAMILIES {
        let present = mutations.contains_key(*family);
        let name = format!("mutations.{family}");
        let allowed: &[&str] = if *family == "language_specific" {
            &["enabled", "operators", "language"]
        } else {
            &["enabled", "operators"]
        };
        let settings = object(mutations.get(*family).unwrap_or(&empty), &name, allowed)?;
        let enabled = boolean(
            settings.get("enabled").unwrap_or(&Value::Bool(false)),
            &format!("{name}.enabled"),
        )?;
        let catalogue = operators(family).expect("family comes from the static catalogue");
        let switches = object(
            settings.get("operators").unwrap_or(&empty),
            &format!("{name}.operators"),
            catalogue,
        )?;
        let switches = catalogue
            .iter()
            .filter_map(|id| {
                switches.get(*id).map(|value| {
                    boolean(value, &format!("{name}.operators.{id}"))
                        .map(|enabled| ((*id).to_owned(), enabled))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let language = if *family == "language_specific" && present {
            if settings.get("language").and_then(Value::as_str) != Some("dart") {
                return Err(ConfigError::new(
                    "mutations.language_specific.language must explicitly be dart",
                ));
            }
            Some("dart".to_owned())
        } else {
            None
        };
        selections.push(MutationSelection::new(
            (*family).into(),
            enabled,
            switches,
            language,
            present,
        )?);
    }
    if !selections
        .iter()
        .any(|selection| !selection.active_operators().is_empty())
    {
        return Err(ConfigError::new(
            "profile has no enabled mutation operators",
        ));
    }
    let mut common: Map<_, _> = values
        .iter()
        .filter(|(key, _)| key.as_str() != "mutations")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    common.insert("project".into(), path_value(&project, "project")?);
    common.insert("family".into(), json!("classic"));
    let common = from_mapping(
        &Value::Object(common),
        &Context {
            cwd: current,
            installation: installed,
        },
        Some(&base),
    )?;
    let campaigns = selections
        .iter()
        .filter_map(|selection| {
            let active = selection.active_operators();
            if active.is_empty() {
                return None;
            }
            let mut campaign = common.clone();
            campaign.family.clone_from(&selection.family);
            campaign.language.clone_from(&selection.language);
            campaign.rules.operators = Some(active);
            Some(campaign)
        })
        .collect();
    ProfileConfig::new(campaigns, selections)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        let cwd = std::env::current_dir().unwrap();
        Context {
            installation: cwd.join("installation-does-not-need-python"),
            cwd,
        }
    }
    fn direct(value: Value) -> Result<CampaignConfig, ConfigError> {
        from_mapping(&value, &context(), None)
    }
    fn active() -> Value {
        json!({"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}}})
    }
    fn profile(mut values: Value) -> Result<ProfileConfig, ConfigError> {
        if values.get("mutations").is_none() {
            values["mutations"] = active();
        }
        from_profile_mapping(&values, &context().cwd, &context(), None)
    }

    #[test]
    fn direct_defaults_have_exact_effective_manifest_fields_and_no_python_dependency() {
        let config = direct(json!({"family":"classic"})).unwrap();
        let context = context();
        assert_eq!(config.project, context.cwd);
        assert_eq!(
            config.to_value(),
            json!({"project":context.cwd.to_string_lossy(),"family":"classic","workers":1,
            "timeouts":{"test":"30000ms","command":"120000ms"},"rules":{"include":["lib/**.dart"],"exclude":[]},
            "limits":{"max_mutants":1000},"dart_sdk":context.installation.join(".toolchain/flutter/bin/cache/dart-sdk").to_string_lossy(),
            "flutter_sdk":null,"adapter_root":context.installation.join("adapters/dart").to_string_lossy(),"output":null})
        );
        let mut value = config.to_value();
        value["rules"]["include"] = json!([]);
        assert_eq!(config.rules.include, Some(vec!["lib/**.dart".into()]));
    }

    #[test]
    fn strict_root_shapes_keys_family_integer_ranges_and_nested_maps() {
        for bad in [
            Value::Null,
            json!([]),
            json!(true),
            json!({}),
            json!({"family":"Classic"}),
            json!({"family":true}),
            json!({"family":"classic","version":2}),
            json!({"family":"classic","unknown":0}),
            json!({"family":"classic","timeouts":null}),
            json!({"family":"classic","rules":[]}),
            json!({"family":"classic","limits":{"unknown":1}}),
        ] {
            assert!(direct(bad.clone()).is_err(), "{bad}");
        }
        for bad in [json!(0), json!(5), json!(true), json!(1.0), json!("2")] {
            assert!(direct(json!({"family":"classic","workers":bad})).is_err());
        }
        for bad in [
            json!(-1),
            json!(10001),
            json!(false),
            json!(1.0),
            json!("0"),
        ] {
            assert!(direct(json!({"family":"classic","limits":{"max_mutants":bad}})).is_err());
        }
        for maximum in [0, 10000] {
            assert_eq!(
                direct(json!({"family":"classic","workers":4,"limits":{"max_mutants":maximum}}))
                    .unwrap()
                    .max_mutants,
                maximum
            );
        }
    }

    #[test]
    fn rules_distinguish_omission_empty_and_component_filter_defaults() {
        let config = direct(json!({"family":"classic","rules":{"operators":[],"include":[],"classes":[],"methods":[],
            "components":[{"name":"network","operators":["FUTURE_OPERATOR"],"methods":["load"]}]}})).unwrap();
        assert_eq!(
            config.rules.to_adapter_rules(),
            json!({"operators":[],"includePaths":[],"excludePaths":[],"classes":[],"methods":[],
            "components":[{"name":"network","operators":["FUTURE_OPERATOR"],"excludePaths":[],"methods":["load"]}]})
        );
        for name in ["operators", "include", "exclude", "classes", "methods"] {
            for bad in [
                Value::Null,
                json!("one"),
                json!([true]),
                json!([""]),
                json!(["a\u{0000}b"]),
                json!(["SAME", "SAME"]),
            ] {
                let mut value = json!({"family":"classic","rules":{}});
                value["rules"][name] = bad;
                assert!(direct(value).is_err());
            }
        }
        assert!(direct(json!({"family":"classic","rules":{"include":vec!["x";10001]}})).is_err());
        for components in [
            json!([{"name":"same"},{"name":"same"}]),
            json!([{"name":""}]),
            json!([{"name":"x","components":[]}]),
            json!([{"name":"😀".repeat(129)}]),
            json!({}),
        ] {
            assert!(direct(json!({"family":"classic","rules":{"components":components}})).is_err());
        }
        assert!(
            direct(json!({"family":"classic","rules":{"components":[{"name":"😀".repeat(128)}]}}))
                .is_ok()
        );
    }

    #[test]
    fn paths_use_explicit_base_and_flutter_derives_dart_without_io_preflight() {
        let context = context();
        let base = context.cwd.join("configuration");
        let config = from_mapping(
            &json!({"family":"failure_mode","project":"subject","flutter_sdk":"../flutter",
            "adapter_root":"adapter","output":"reports"}),
            &context,
            Some(&base),
        )
        .unwrap();
        assert_eq!(config.project, base.join("subject"));
        assert_eq!(config.output, Some(base.join("reports")));
        assert_eq!(config.flutter_sdk, Some(context.cwd.join("flutter")));
        assert_eq!(
            config.dart_sdk,
            context.cwd.join("flutter/bin/cache/dart-sdk")
        );
        assert!(
            direct(json!({"family":"classic","dart_sdk":"dart","flutter_sdk":"flutter"})).is_err()
        );
        for key in [
            "project",
            "dart_sdk",
            "flutter_sdk",
            "adapter_root",
            "output",
        ] {
            for bad in [json!(""), json!("bad\u{0000}path"), json!(false), json!(5)] {
                let mut value = json!({"family":"classic"});
                value[key] = bad;
                assert!(direct(value).is_err());
            }
        }
    }

    #[test]
    fn direct_dart_defaults_groups_and_explicit_empty_intersection_match_legacy() {
        let default = direct(json!({"family":"language_specific"})).unwrap();
        assert_eq!(default.rules.operators.unwrap(), DART_OPERATORS);
        let config=direct(json!({"family":"language_specific","rules":{"operators":["NSM-001","ENM-001"],"exclude":["same"],
            "components":[{"name":"net","operators":["NSM-002"]}]},"mutations":{"dart_semantic":{"null_safety":false,"exclude":["same","extra"]}}})).unwrap();
        assert_eq!(config.rules.operators, Some(vec!["ENM-001".into()]));
        assert_eq!(config.rules.exclude, ["same", "extra"]);
        assert_eq!(
            config.rules.components[0].rules.operators,
            Some(vec!["NSM-002".into()])
        );
        for value in [
            json!({"family":"language_specific","rules":{"operators":[]}}),
            json!({"family":"language_specific","mutations":{"dart_semantic":{"enabled":false}}}),
        ] {
            assert_eq!(direct(value).unwrap().rules.operators, Some(vec![]));
        }
        for bad in [
            json!({"family":"language_specific","language":null}),
            json!({"family":"language_specific","language":"rust"}),
            json!({"family":"language_specific","rules":{"operators":["NSM-999"]}}),
            json!({"family":"language_specific","rules":{"components":[{"name":"x","operators":["BOOLEAN_NEGATION"]}]}}),
            json!({"family":"language_specific","mutations":{"dart_semantic":{"null_safety":1}}}),
            json!({"family":"language_specific","mutations":{"unknown":{}}}),
            json!({"family":"classic","language":"dart"}),
            json!({"family":"failure_mode","mutations":{}}),
        ] {
            assert!(direct(bad.clone()).is_err(), "{bad}");
        }
    }

    #[test]
    fn profile_families_and_operator_switches_follow_registry_order_and_share_settings() {
        let config=profile(json!({"workers":3,"timeouts":{"test":"2s","command":"9s"},"limits":{"max_mutants":7},
            "rules":{"include":["lib/модель😀.dart"],"exclude":["generated"],"classes":["Gateway"],"methods":["read"]},"output":"out",
            "mutations":{"language_specific":{"language":"dart","enabled":true,"operators":{"ENM-001":true,"NSM-001":true}},
                "failure_mode":{"enabled":true,"operators":{"EXCEPTION_AT_ENTRY":true}},
                "classic":{"enabled":true,"operators":{"BOOLEAN_NEGATION":true,"BOOLEAN_LITERAL_FLIP":true,"RELATIONAL_BOUNDARY":false}}}})).unwrap();
        assert_eq!(
            config
                .campaigns
                .iter()
                .map(|campaign| campaign.family.as_str())
                .collect::<Vec<_>>(),
            FAMILIES
        );
        assert_eq!(
            config.campaigns[0].rules.operators,
            Some(vec![
                "BOOLEAN_LITERAL_FLIP".into(),
                "BOOLEAN_NEGATION".into()
            ])
        );
        assert_eq!(
            config.campaigns[2].rules.operators,
            Some(vec!["NSM-001".into(), "ENM-001".into()])
        );
        for campaign in &config.campaigns {
            assert_eq!(campaign.workers, 3);
            assert_eq!(campaign.test_timeout, "2000ms");
            assert_eq!(campaign.command_timeout, "9000ms");
        }
        assert_eq!(config.max_mutants(), 7);
        assert_eq!(config.project(), context().cwd);
        assert_eq!(config.output(), Some(context().cwd.join("out").as_path()));
        let value = config.to_value();
        assert_eq!(value["kind"], "profile");
        assert!(value.get("family").is_none());
        assert!(value.get("language").is_none());
        assert!(value.get("version").is_none());
        assert!(value["rules"].get("operators").is_none());
        assert_eq!(
            value["mutations"]["classic"]["operators"]["RELATIONAL_BOUNDARY"],
            false
        );
    }

    #[test]
    fn omission_and_false_switches_never_activate_a_family_or_infer_a_language() {
        for mutations in [
            json!({}),
            json!({"classic":{}}),
            json!({"classic":{"enabled":true}}),
            json!({"classic":{"operators":{"BOOLEAN_LITERAL_FLIP":true}}}),
            json!({"classic":{"enabled":false,"operators":{"BOOLEAN_LITERAL_FLIP":true}}}),
            json!({"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":false}}}),
        ] {
            assert!(profile(json!({"mutations":mutations})).is_err());
        }
        let profile = profile(json!({})).unwrap();
        assert_eq!(profile.campaigns.len(), 1);
        assert!(!profile.mutations[2].present);
        assert!(profile.mutations[2].language.is_none());
        assert!(
            profile.to_value()["mutations"]
                .get("language_specific")
                .is_none()
        );
    }

    #[test]
    fn inactive_profile_settings_still_validate_language_ids_booleans_and_unknown_keys() {
        for bad in [
            Value::Null,
            json!(0),
            json!(1),
            json!("true"),
            json!([]),
            json!({}),
        ] {
            for family in FAMILIES {
                for switch in [false, true] {
                    let mut value = json!({"mutations":active()});
                    value["mutations"][*family] = json!({"enabled":false,"operators":{}});
                    if *family == "language_specific" {
                        value["mutations"][*family]["language"] = json!("dart");
                    }
                    if switch {
                        value["mutations"][*family]["operators"][operators(family).unwrap()[0]] =
                            bad.clone();
                    } else {
                        value["mutations"][*family]["enabled"] = bad.clone();
                    }
                    assert!(profile(value).is_err());
                }
            }
        }
        for language in [
            Value::Null,
            json!(""),
            json!("Dart"),
            json!("rust"),
            json!(true),
            json!(1),
            json!([]),
        ] {
            let mut value = json!({"mutations":active()});
            value["mutations"]["language_specific"] = json!({"enabled":false,"language":language});
            assert!(profile(value).is_err());
        }
        for bad in [
            json!({"version":2}),
            json!({"project":"x"}),
            json!({"family":"classic"}),
            json!({"language":"dart"}),
            json!({"rules":{"operators":[]}}),
            json!({"rules":{"components":[]}}),
            json!({"mutations":{"classic":{"operators":{"FUTURE_OPERATOR":false}}}}),
            json!({"mutations":{"classic":{"operators":{"NSM-001":false}}}}),
        ] {
            assert!(profile(bad).is_err());
        }
    }

    #[test]
    fn every_known_operator_can_be_selected_alone_including_legacy_conditional() {
        for family in FAMILIES {
            for operator in operators(family).unwrap() {
                let mut value = json!({"mutations":{}});
                value["mutations"][*family] = json!({"enabled":true,"operators":{}});
                value["mutations"][*family]["operators"][*operator] = json!(true);
                if *family == "language_specific" {
                    value["mutations"][*family]["language"] = json!("dart");
                }
                let config = profile(value).unwrap();
                assert_eq!(config.campaigns.len(), 1);
                assert_eq!(
                    config.campaigns[0].rules.operators,
                    Some(vec![(*operator).into()])
                );
            }
        }
    }

    #[test]
    fn programmatic_profile_rejects_mismatched_order_operators_language_and_shared_settings() {
        let profile=profile(json!({"mutations":{"classic":{"enabled":true,"operators":{"BOOLEAN_LITERAL_FLIP":true}},
            "failure_mode":{"enabled":true,"operators":{"EXCEPTION_AT_ENTRY":true}}}})).unwrap();
        for index in 0..5 {
            let mut campaigns = profile.campaigns.clone();
            match index {
                0 => campaigns.reverse(),
                1 => campaigns[1].workers = 4,
                2 => campaigns[0].rules.operators = None,
                3 => campaigns[0].language = Some("dart".into()),
                _ => campaigns[0].rules.components.push(CampaignComponent {
                    name: "extra".into(),
                    rules: CampaignRules::default(),
                }),
            }
            assert!(ProfileConfig::new(campaigns, profile.mutations.clone()).is_err());
        }
        let mut selections = profile.mutations.clone();
        selections[0].enabled = false;
        assert!(ProfileConfig::new(profile.campaigns.clone(), selections).is_err());
        let mut selections = profile.mutations.clone();
        selections.push(selections[0].clone());
        assert!(ProfileConfig::new(profile.campaigns.clone(), selections).is_err());
        assert!(ProfileConfig::new(vec![], vec![]).is_err());
    }
}
