//! Typed effective settings and fresh JSON projections for execution manifests.

use crate::catalogue::{FAMILIES, operators, valid_operator_id};
use crate::{ConfigError, MAX_RULE_VALUES};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignRules {
    pub operators: Option<Vec<String>>,
    pub include: Option<Vec<String>>,
    pub exclude: Vec<String>,
    pub classes: Option<Vec<String>>,
    pub methods: Option<Vec<String>>,
    pub components: Vec<CampaignComponent>,
}

impl Default for CampaignRules {
    fn default() -> Self {
        Self {
            operators: None,
            include: Some(vec!["lib/**.dart".into()]),
            exclude: vec![],
            classes: None,
            methods: None,
            components: vec![],
        }
    }
}

impl CampaignRules {
    pub fn to_value(&self) -> Value {
        let mut value = Map::new();
        value.insert("exclude".into(), json!(self.exclude));
        for (name, items) in [
            ("operators", &self.operators),
            ("include", &self.include),
            ("classes", &self.classes),
            ("methods", &self.methods),
        ] {
            if let Some(items) = items {
                value.insert(name.into(), json!(items));
            }
        }
        if !self.components.is_empty() {
            value.insert(
                "components".into(),
                Value::Array(
                    self.components
                        .iter()
                        .map(CampaignComponent::to_value)
                        .collect(),
                ),
            );
        }
        Value::Object(value)
    }

    pub fn to_adapter_rules(&self) -> Value {
        let Value::Object(rules) = self.to_value() else {
            unreachable!()
        };
        let mut value: Map<String, Value> = rules
            .into_iter()
            .map(|(key, value)| {
                let key = match key.as_str() {
                    "include" => "includePaths",
                    "exclude" => "excludePaths",
                    _ => &key,
                };
                (key.to_owned(), value)
            })
            .collect();
        if !self.components.is_empty() {
            value.insert(
                "components".into(),
                Value::Array(
                    self.components
                        .iter()
                        .map(|component| {
                            let Value::Object(mut value) = component.rules.to_adapter_rules()
                            else {
                                unreachable!()
                            };
                            value.insert("name".into(), json!(component.name));
                            Value::Object(value)
                        })
                        .collect(),
                ),
            );
        }
        Value::Object(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignComponent {
    pub name: String,
    pub rules: CampaignRules,
}

impl CampaignComponent {
    pub fn to_value(&self) -> Value {
        let Value::Object(mut value) = self.rules.to_value() else {
            unreachable!()
        };
        value.insert("name".into(), json!(self.name));
        Value::Object(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignConfig {
    pub project: PathBuf,
    pub family: String,
    pub workers: u32,
    pub test_timeout: String,
    pub command_timeout: String,
    pub rules: CampaignRules,
    pub max_mutants: u32,
    pub dart_sdk: PathBuf,
    pub flutter_sdk: Option<PathBuf>,
    pub adapter_root: PathBuf,
    pub output: Option<PathBuf>,
    pub language: Option<String>,
}

impl CampaignConfig {
    pub fn to_value(&self) -> Value {
        let mut value = json!({"project": self.project.to_string_lossy(), "family":self.family,
            "workers":self.workers, "timeouts":{"test":self.test_timeout,"command":self.command_timeout},
            "rules":self.rules.to_value(), "limits":{"max_mutants":self.max_mutants},
            "dart_sdk":self.dart_sdk.to_string_lossy(),
            "flutter_sdk":self.flutter_sdk.as_ref().map(|path| path.to_string_lossy()),
            "adapter_root":self.adapter_root.to_string_lossy(),
            "output":self.output.as_ref().map(|path| path.to_string_lossy())});
        if let Some(language) = &self.language {
            value["language"] = json!(language);
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationSelection {
    pub family: String,
    pub enabled: bool,
    pub operators: Vec<(String, bool)>,
    pub language: Option<String>,
    pub present: bool,
}

impl MutationSelection {
    pub fn new(
        family: String,
        enabled: bool,
        switches: Vec<(String, bool)>,
        language: Option<String>,
        present: bool,
    ) -> Result<Self, ConfigError> {
        let value = Self {
            family,
            enabled,
            operators: switches,
            language,
            present,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let known = operators(&self.family)
            .ok_or_else(|| ConfigError::new("mutation selection requires a supported family"))?;
        let mut seen = HashSet::new();
        for (operator, _) in &self.operators {
            if !known.contains(&operator.as_str()) || !seen.insert(operator) {
                return Err(ConfigError::new(
                    "mutation selection requires unique known family operator IDs",
                ));
            }
        }
        if !self.present {
            if self.enabled || !self.operators.is_empty() || self.language.is_some() {
                return Err(ConfigError::new(
                    "an absent mutation selection cannot contain settings",
                ));
            }
        } else if self.family == "language_specific" {
            if self.language.as_deref() != Some("dart") {
                return Err(ConfigError::new(
                    "mutations.language_specific.language must explicitly be dart",
                ));
            }
        } else if self.language.is_some() {
            return Err(ConfigError::new(
                "only language_specific selections can specify a language",
            ));
        }
        Ok(())
    }

    pub fn active_operators(&self) -> Vec<String> {
        if !self.enabled {
            return vec![];
        }
        operators(&self.family)
            .unwrap_or_default()
            .iter()
            .filter(|operator| {
                self.operators
                    .iter()
                    .any(|(id, enabled)| id == **operator && *enabled)
            })
            .map(|operator| (*operator).to_owned())
            .collect()
    }

    pub fn to_value(&self) -> Value {
        let switches: Map<String, Value> = self
            .operators
            .iter()
            .map(|(id, enabled)| (id.clone(), json!(enabled)))
            .collect();
        let mut value = json!({"enabled":self.enabled,"operators":switches});
        if let Some(language) = &self.language {
            value["language"] = json!(language);
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileConfig {
    pub campaigns: Vec<CampaignConfig>,
    pub mutations: Vec<MutationSelection>,
}

impl ProfileConfig {
    pub fn new(
        campaigns: Vec<CampaignConfig>,
        mutations: Vec<MutationSelection>,
    ) -> Result<Self, ConfigError> {
        let value = Self {
            campaigns,
            mutations,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.campaigns.is_empty() {
            return Err(ConfigError::new("a profile requires active operators"));
        }
        let mut selections = HashMap::new();
        for selection in &self.mutations {
            selection.validate()?;
            if selections
                .insert(selection.family.as_str(), selection)
                .is_some()
            {
                return Err(ConfigError::new(
                    "profile mutation selections must have distinct families",
                ));
            }
        }
        let expected: Vec<_> = FAMILIES
            .iter()
            .copied()
            .filter(|family| {
                selections
                    .get(family)
                    .is_some_and(|selection| !selection.active_operators().is_empty())
            })
            .collect();
        if self
            .campaigns
            .iter()
            .map(|campaign| campaign.family.as_str())
            .collect::<Vec<_>>()
            != expected
        {
            return Err(ConfigError::new(
                "profile campaigns must match active mutation selections in family order",
            ));
        }
        for campaign in &self.campaigns {
            let selection = selections[campaign.family.as_str()];
            if campaign.language != selection.language {
                return Err(ConfigError::new(
                    "profile campaign language must match its explicit mutation selection",
                ));
            }
            if !campaign.rules.components.is_empty() {
                return Err(ConfigError::new(
                    "profile rules.components cannot override explicit mutation selections",
                ));
            }
            for (key, values) in [
                ("operators", &campaign.rules.operators),
                ("include", &campaign.rules.include),
                ("classes", &campaign.rules.classes),
                ("methods", &campaign.rules.methods),
            ] {
                if let Some(values) = values {
                    validate_strings(values, &format!("profile rules.{key}"), key == "operators")?;
                }
            }
            validate_strings(&campaign.rules.exclude, "profile rules.exclude", false)?;
            if campaign.rules.operators.as_ref() != Some(&selection.active_operators()) {
                return Err(ConfigError::new(
                    "profile campaign operators must match its explicit mutation selection",
                ));
            }
            for (name, path) in [
                ("project", Some(campaign.project.as_path())),
                ("dart_sdk", Some(campaign.dart_sdk.as_path())),
                ("flutter_sdk", campaign.flutter_sdk.as_deref()),
                ("adapter_root", Some(campaign.adapter_root.as_path())),
                ("output", campaign.output.as_deref()),
            ] {
                if path.is_some_and(|path| path.to_str().is_none()) {
                    return Err(ConfigError::new(format!(
                        "profile {name} must contain valid Unicode"
                    )));
                }
            }
        }
        let common = &self.campaigns[0];
        for campaign in &self.campaigns[1..] {
            let mut comparable = campaign.clone();
            comparable.family.clone_from(&common.family);
            comparable.language.clone_from(&common.language);
            comparable
                .rules
                .operators
                .clone_from(&common.rules.operators);
            if comparable != *common {
                return Err(ConfigError::new(
                    "profile families must share execution settings and filters",
                ));
            }
        }
        Ok(())
    }

    pub fn project(&self) -> &Path {
        &self.campaigns[0].project
    }
    pub fn output(&self) -> Option<&Path> {
        self.campaigns[0].output.as_deref()
    }
    pub fn max_mutants(&self) -> u32 {
        self.campaigns[0].max_mutants
    }

    pub fn to_value(&self) -> Value {
        let Value::Object(mut value) = self.campaigns[0].to_value() else {
            unreachable!()
        };
        value.insert("kind".into(), json!("profile"));
        value.remove("family");
        value.remove("language");
        if let Some(Value::Object(rules)) = value.get_mut("rules") {
            rules.remove("operators");
        }
        value.insert(
            "mutations".into(),
            Value::Object(
                self.mutations
                    .iter()
                    .filter(|selection| selection.present)
                    .map(|selection| (selection.family.clone(), selection.to_value()))
                    .collect(),
            ),
        );
        Value::Object(value)
    }
}

pub(crate) fn validate_strings(
    values: &[String],
    name: &str,
    operator_ids: bool,
) -> Result<(), ConfigError> {
    if values.len() > MAX_RULE_VALUES {
        return Err(ConfigError::new(format!(
            "{name} must be a bounded list of strings"
        )));
    }
    let mut seen = HashSet::new();
    for item in values {
        if item.is_empty() || item.contains('\0') {
            return Err(ConfigError::new(format!(
                "{name} must contain nonempty strings without NUL"
            )));
        }
        if operator_ids && !valid_operator_id(item) {
            return Err(ConfigError::new(format!(
                "{name} must contain uppercase operator IDs"
            )));
        }
        if !seen.insert(item) {
            return Err(ConfigError::new(format!(
                "{name} contains a duplicate value"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_projection_preserves_omission_empty_and_component_adapter_names() {
        let mut rules = CampaignRules {
            include: None,
            operators: Some(vec![]),
            ..CampaignRules::default()
        };
        assert_eq!(rules.to_value(), json!({"operators":[],"exclude":[]}));
        rules.components.push(CampaignComponent {
            name: "Сервис😀".into(),
            rules: CampaignRules {
                include: None,
                methods: Some(vec!["читать".into()]),
                ..CampaignRules::default()
            },
        });
        assert_eq!(
            rules.to_adapter_rules(),
            json!({"operators":[],"excludePaths":[],
            "components":[{"name":"Сервис😀","excludePaths":[],"methods":["читать"]}]})
        );
        let mut projected = rules.to_value();
        projected["operators"] = json!(["changed"]);
        assert_eq!(rules.operators, Some(vec![]));
    }

    #[test]
    fn mutation_selection_validates_inactive_absent_and_canonical_order() {
        let selection = MutationSelection::new(
            "classic".into(),
            true,
            vec![
                ("BOOLEAN_NEGATION".into(), true),
                ("RELATIONAL_BOUNDARY".into(), false),
                ("BOOLEAN_LITERAL_FLIP".into(), true),
            ],
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            selection.active_operators(),
            ["BOOLEAN_LITERAL_FLIP", "BOOLEAN_NEGATION"]
        );
        for (family, switches, language, present) in [
            ("unknown", vec![], None, true),
            ("classic", vec![("NSM-001".into(), false)], None, true),
            (
                "classic",
                vec![
                    ("BOOLEAN_NEGATION".into(), false),
                    ("BOOLEAN_NEGATION".into(), true),
                ],
                None,
                true,
            ),
            ("classic", vec![], Some("dart".into()), true),
            ("language_specific", vec![], None, true),
            ("language_specific", vec![], Some("dart".into()), false),
            (
                "classic",
                vec![("BOOLEAN_NEGATION".into(), false)],
                None,
                false,
            ),
        ] {
            assert!(
                MutationSelection::new(family.into(), false, switches, language, present).is_err()
            );
        }
        assert!(
            MutationSelection::new("language_specific".into(), false, vec![], None, false).is_ok()
        );
        assert!(MutationSelection::new("classic".into(), true, vec![], None, false).is_err());
    }
}
