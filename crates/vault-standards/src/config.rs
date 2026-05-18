use std::collections::HashMap;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config {source_path}: {message}")]
    Invalid {
        source_path: camino::Utf8PathBuf,
        message: String,
    },
    #[error("invalid config {source_path}: 'graph.ignore' was renamed to 'files.ignore' in v0.16")]
    DeprecatedGraphIgnore { source_path: camino::Utf8PathBuf },
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    #[serde(default)]
    pub files: FilesConfig,
    #[serde(default)]
    pub validate: ValidateConfig,
    #[serde(default)]
    pub repair: RepairConfig,
    // Capture the deprecated v0.16 key so post_validate can emit a clear error.
    #[serde(default, rename = "graph")]
    _deprecated_graph: Option<serde_yaml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub required_frontmatter: Vec<String>,
    #[serde(default)]
    pub rules: Vec<ValidateRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateRule {
    pub name: Option<String>,
    #[serde(default, rename = "match")]
    pub r#match: RuleSelector,
    #[serde(default)]
    pub exclude: RuleExclude,
    #[serde(default)]
    pub required_frontmatter: Vec<String>,
    #[serde(default)]
    pub forbidden_frontmatter: Vec<String>,
    #[serde(default)]
    pub field_types: HashMap<String, String>,
    #[serde(default)]
    pub allowed_values: HashMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSelector {
    pub path: Option<String>,
    pub path_not: Option<String>,
    #[serde(default)]
    pub frontmatter: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleExclude {
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairConfig {
    #[serde(default)]
    pub rules: Vec<RepairRule>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairRule {
    pub name: Option<String>,
    #[serde(default, rename = "match")]
    pub r#match: RepairRuleMatch,
    #[serde(default)]
    pub set_frontmatter: Option<SetFrontmatterAction>,
    #[serde(default)]
    pub remove_frontmatter: Option<RemoveFrontmatterAction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairRuleMatch {
    pub code: Option<String>,
    pub rule: Option<String>,
    pub field: Option<String>,
    pub actual_value: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetFrontmatterAction {
    pub field: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveFrontmatterAction {
    pub field: String,
}

/// Repair rule action — derived from RepairRule by `action(...)` after
/// post_validate ensures exactly one of `set_frontmatter` / `remove_frontmatter`
/// is set. The existing engine code consumes this via the `action` accessor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairAction {
    SetFrontmatter {
        field: String,
        value: serde_json::Value,
    },
    RemoveFrontmatter {
        field: String,
    },
}

impl RepairRule {
    /// Returns the rule's action after post_validate has guaranteed exactly one is set.
    /// Panics if post_validate didn't run or didn't catch the invariant violation.
    pub fn action(&self) -> RepairAction {
        match (&self.set_frontmatter, &self.remove_frontmatter) {
            (Some(set), None) => RepairAction::SetFrontmatter {
                field: set.field.clone(),
                value: set.value.clone(),
            },
            (None, Some(remove)) => RepairAction::RemoveFrontmatter {
                field: remove.field.clone(),
            },
            _ => unreachable!("post_validate ensures exactly one repair action"),
        }
    }
}

/// Parse a YAML config string with full validation. This is the single public entry
/// point — replaces the old split between `serde_yaml::from_str::<VaultConfig>` (in
/// the CLI) and `validate_config_yaml` (in vault-standards).
pub fn parse_config(yaml: &str, source_path: &Utf8Path) -> Result<VaultConfig, ConfigError> {
    let cfg: VaultConfig = serde_yaml::from_str(yaml).map_err(|e| ConfigError::Invalid {
        source_path: source_path.to_owned(),
        message: e.to_string(),
    })?;
    post_validate(&cfg, source_path)?;
    Ok(cfg)
}

fn post_validate(cfg: &VaultConfig, source_path: &Utf8Path) -> Result<(), ConfigError> {
    if cfg._deprecated_graph.is_some() {
        return Err(ConfigError::DeprecatedGraphIgnore {
            source_path: source_path.to_owned(),
        });
    }

    // Validate field_types: each value must be a known type.
    for rule in &cfg.validate.rules {
        let rule_label = rule
            .name
            .clone()
            .unwrap_or_else(|| "unnamed validate rule".into());

        for (field, ty) in &rule.field_types {
            if !is_known_field_type(ty) {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "rule {rule_label}: unknown field_type '{ty}' for field '{field}'; expected one of: datetime, date, list_of_strings, wikilink, wikilink_or_list"
                    ),
                });
            }
        }

        // allowed_values: non-empty, scalar values only.
        for (field, values) in &rule.allowed_values {
            if values.is_empty() {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!("rule {rule_label}: allowed_values for '{field}' is empty"),
                });
            }
            for v in values {
                if !is_scalar_json_value(v) {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: allowed_values for '{field}' contains a non-scalar value"
                        ),
                    });
                }
            }
        }

        // Frontmatter predicate values must be scalar (string/bool/number/null).
        for (field, value) in &rule.r#match.frontmatter {
            if !is_scalar_json_value(value) {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "rule {rule_label}: match.frontmatter.{field} must be a string, boolean, or number"
                    ),
                });
            }
        }
    }

    // Repair rules: exactly one of set_frontmatter / remove_frontmatter.
    for rule in &cfg.repair.rules {
        let rule_label = rule
            .name
            .clone()
            .unwrap_or_else(|| "unnamed repair rule".into());
        match (&rule.set_frontmatter, &rule.remove_frontmatter) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "repair rule {rule_label} declares both set_frontmatter and remove_frontmatter; pick one"
                    ),
                });
            }
            (None, None) => {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "repair rule {rule_label} declares no action (need set_frontmatter or remove_frontmatter)"
                    ),
                });
            }
            _ => {}
        }
    }

    Ok(())
}

fn is_known_field_type(ty: &str) -> bool {
    matches!(
        ty,
        "datetime" | "date" | "list_of_strings" | "wikilink" | "wikilink_or_list"
    )
}

fn is_scalar_json_value(v: &serde_json::Value) -> bool {
    matches!(
        v,
        serde_json::Value::Null
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<VaultConfig, ConfigError> {
        parse_config(yaml, Utf8Path::new("/test/.vault/config.yaml"))
    }

    #[test]
    fn empty_config_parses_to_defaults() {
        let cfg = parse("").unwrap();
        assert!(cfg.files.ignore.is_empty());
        assert!(cfg.validate.rules.is_empty());
        assert!(cfg.repair.rules.is_empty());
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let err = parse("notakey: foo\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field"), "got: {msg}");
    }

    #[test]
    fn deprecated_graph_key_is_rejected_with_v0_16_message() {
        let err = parse("graph:\n  ignore:\n    - foo\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("v0.16"), "got: {msg}");
        assert!(msg.contains("graph.ignore"), "got: {msg}");
        assert!(msg.contains("files.ignore"), "got: {msg}");
    }

    #[test]
    fn unknown_field_type_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        created: bogus\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field_type 'bogus'"), "got: {msg}");
        assert!(msg.contains("datetime"), "got: {msg}");
    }

    #[test]
    fn empty_allowed_values_list_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      allowed_values:\n        status: []\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("allowed_values for 'status' is empty"),
            "got: {msg}"
        );
    }

    #[test]
    fn non_scalar_allowed_value_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      allowed_values:\n        status:\n          - [a, b]\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("non-scalar"), "got: {msg}");
    }

    #[test]
    fn repair_rule_with_both_actions_is_rejected() {
        let err = parse(
            "repair:\n  rules:\n    - name: r\n      match:\n        code: x\n      set_frontmatter:\n        field: a\n        value: 1\n      remove_frontmatter:\n        field: a\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declares both"), "got: {msg}");
    }

    #[test]
    fn repair_rule_with_no_action_is_rejected() {
        let err =
            parse("repair:\n  rules:\n    - name: r\n      match:\n        code: x\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declares no action"), "got: {msg}");
    }

    #[test]
    fn valid_full_config_parses_cleanly() {
        let yaml = r#"
files:
  ignore:
    - "**/*.pyc"
validate:
  ignore:
    - "Archive/**"
  required_frontmatter:
    - title
  rules:
    - name: typed-note
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      required_frontmatter:
        - kind
      field_types:
        created: datetime
      allowed_values:
        kind:
          - research
          - log
repair:
  rules:
    - name: fix-someday
      match:
        code: frontmatter-disallowed-value
        field: status
        actual_value: someday
      set_frontmatter:
        field: status
        value: backlog
"#;
        let cfg = parse(yaml).unwrap();
        assert_eq!(cfg.validate.rules.len(), 1);
        assert_eq!(cfg.repair.rules.len(), 1);
    }
}
