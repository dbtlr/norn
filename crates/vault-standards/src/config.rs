use std::collections::HashMap;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::path_match::{PathPattern, PathPatternError};

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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    #[serde(default = "default_schema_version")]
    pub version: u32,
    #[serde(default)]
    pub files: FilesConfig,
    #[serde(default)]
    pub links: LinksConfig,
    #[serde(default)]
    pub validate: ValidateConfig,
    #[serde(default)]
    pub repair: RepairConfig,
    // Capture the deprecated v0.16 key so post_validate can emit a clear error.
    #[serde(default, rename = "graph")]
    _deprecated_graph: Option<serde_yaml::Value>,
}

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            version: CURRENT_SCHEMA_VERSION,
            files: FilesConfig::default(),
            links: LinksConfig::default(),
            validate: ValidateConfig::default(),
            repair: RepairConfig::default(),
            _deprecated_graph: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinksConfig {
    #[serde(default)]
    pub alias_field: Option<String>,
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
    #[serde(default)]
    pub add_frontmatter: Option<AddFrontmatterAction>,
    #[serde(default)]
    pub move_document: Option<MoveDocumentAction>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AddFrontmatterAction {
    pub field: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoveDocumentAction {
    #[serde(default)]
    pub to_directory: Option<String>,
    #[serde(default)]
    pub to_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DestinationSpec {
    Directory { to_directory: String },
    Path { to_path: String },
}

impl DestinationSpec {
    pub fn raw(&self) -> &str {
        match self {
            DestinationSpec::Directory { to_directory } => to_directory,
            DestinationSpec::Path { to_path } => to_path,
        }
    }
}

/// Repair rule action — derived from RepairRule by `action(...)` after
/// post_validate ensures exactly one action field is set. The existing engine
/// code consumes this via the `action` accessor.
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
    AddFrontmatter {
        field: String,
        value: serde_json::Value,
    },
    MoveDocument {
        destination: DestinationSpec,
    },
}

impl RepairRule {
    /// Returns the rule's action after post_validate has guaranteed exactly one is set.
    /// Panics if post_validate didn't run or didn't catch the invariant violation.
    pub fn action(&self) -> RepairAction {
        match (
            &self.set_frontmatter,
            &self.remove_frontmatter,
            &self.add_frontmatter,
            &self.move_document,
        ) {
            (Some(set), None, None, None) => RepairAction::SetFrontmatter {
                field: set.field.clone(),
                value: set.value.clone(),
            },
            (None, Some(remove), None, None) => RepairAction::RemoveFrontmatter {
                field: remove.field.clone(),
            },
            (None, None, Some(add), None) => RepairAction::AddFrontmatter {
                field: add.field.clone(),
                value: add.value.clone(),
            },
            (None, None, None, Some(mv)) => RepairAction::MoveDocument {
                destination: match (&mv.to_directory, &mv.to_path) {
                    (Some(dir), None) => DestinationSpec::Directory {
                        to_directory: dir.clone(),
                    },
                    (None, Some(path)) => DestinationSpec::Path {
                        to_path: path.clone(),
                    },
                    _ => unreachable!("post_validate ensures exactly one destination"),
                },
            },
            _ => unreachable!("post_validate ensures exactly one repair action"),
        }
    }
}

/// Pre-compiled path patterns for a single validate rule. Index-matched with
/// `validate.rules[i]` — `compiled_rules[i]` corresponds to `validate.rules[i]`.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    pub path: Option<PathPattern>,
    pub path_not: Option<PathPattern>,
    pub exclude_path: Option<PathPattern>,
    pub allowed_paths: Vec<PathPattern>,
}

/// Pre-compiled path patterns for `files.ignore` and `validate.ignore`.
/// Each entry in the vec corresponds to the pattern string at the same index
/// in the source `Vec<String>`.
#[derive(Debug, Clone, Default)]
pub struct CompiledConfig {
    pub files_ignore: Vec<PathPattern>,
    pub validate_ignore: Vec<PathPattern>,
    pub rules: Vec<CompiledRule>,
}

fn compile_pattern(
    pattern: &str,
    label: &str,
    source_path: &Utf8Path,
) -> Result<PathPattern, ConfigError> {
    PathPattern::parse(pattern).map_err(|e: PathPatternError| ConfigError::Invalid {
        source_path: source_path.to_owned(),
        message: format!("{label}: invalid path pattern `{pattern}`: {e}"),
    })
}

fn compile_optional(
    opt: &Option<String>,
    label: &str,
    source_path: &Utf8Path,
) -> Result<Option<PathPattern>, ConfigError> {
    opt.as_deref()
        .map(|s| compile_pattern(s, label, source_path))
        .transpose()
}

fn compile_vec(
    patterns: &[String],
    label: &str,
    source_path: &Utf8Path,
) -> Result<Vec<PathPattern>, ConfigError> {
    patterns
        .iter()
        .map(|s| compile_pattern(s, label, source_path))
        .collect()
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

/// Parse and compile all path patterns in the config. Returns both the raw
/// deserialized config and a `CompiledConfig` with pre-built `PathPattern`
/// values. Call this instead of `parse_config` when you need hot-path
/// matching (e.g., the validate engine).
pub fn parse_config_compiled(
    yaml: &str,
    source_path: &Utf8Path,
) -> Result<(VaultConfig, CompiledConfig), ConfigError> {
    let cfg = parse_config(yaml, source_path)?;

    let files_ignore = compile_vec(&cfg.files.ignore, "files.ignore", source_path)?;
    let validate_ignore = compile_vec(&cfg.validate.ignore, "validate.ignore", source_path)?;

    let mut compiled_rules = Vec::with_capacity(cfg.validate.rules.len());
    for rule in &cfg.validate.rules {
        let rule_label = rule.name.as_deref().unwrap_or("unnamed validate rule");
        let path = compile_optional(
            &rule.r#match.path,
            &format!("rule {rule_label}: match.path"),
            source_path,
        )?;
        let path_not = compile_optional(
            &rule.r#match.path_not,
            &format!("rule {rule_label}: match.path_not"),
            source_path,
        )?;
        let exclude_path = compile_optional(
            &rule.exclude.path,
            &format!("rule {rule_label}: exclude.path"),
            source_path,
        )?;
        let allowed_paths = compile_vec(
            &rule.allowed_paths,
            &format!("rule {rule_label}: allowed_paths"),
            source_path,
        )?;
        compiled_rules.push(CompiledRule {
            path,
            path_not,
            exclude_path,
            allowed_paths,
        });
    }

    Ok((
        cfg,
        CompiledConfig {
            files_ignore,
            validate_ignore,
            rules: compiled_rules,
        },
    ))
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

    // Repair rules: exactly one of the four action fields.
    for rule in &cfg.repair.rules {
        let rule_label = rule
            .name
            .clone()
            .unwrap_or_else(|| "unnamed repair rule".into());
        let action_count = [
            rule.set_frontmatter.is_some(),
            rule.remove_frontmatter.is_some(),
            rule.add_frontmatter.is_some(),
            rule.move_document.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();
        if action_count > 1 {
            return Err(ConfigError::Invalid {
                source_path: source_path.to_owned(),
                message: format!(
                    "repair rule {rule_label} declares multiple actions; pick one of set_frontmatter, remove_frontmatter, add_frontmatter, move_document"
                ),
            });
        }
        if action_count == 0 {
            return Err(ConfigError::Invalid {
                source_path: source_path.to_owned(),
                message: format!(
                    "repair rule {rule_label} declares no action (need set_frontmatter, remove_frontmatter, add_frontmatter, or move_document)"
                ),
            });
        }
        if let Some(mv) = &rule.move_document {
            match (&mv.to_directory, &mv.to_path) {
                (Some(_), Some(_)) => {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "repair rule {rule_label} move_document declares both to_directory and to_path; pick exactly one"
                        ),
                    });
                }
                (None, None) => {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "repair rule {rule_label} move_document declares neither to_directory nor to_path"
                        ),
                    });
                }
                _ => {}
            }
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
        assert!(msg.contains("declares multiple actions"), "got: {msg}");
    }

    #[test]
    fn repair_rule_with_no_action_is_rejected() {
        let err =
            parse("repair:\n  rules:\n    - name: r\n      match:\n        code: x\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("declares no action"), "got: {msg}");
    }

    #[test]
    fn add_frontmatter_action_parses() {
        let yaml = r#"
repair:
  rules:
    - name: ensure-kind
      match:
        code: frontmatter-required-field-missing
        field: kind
      add_frontmatter:
        field: kind
        value: research
"#;
        let cfg = parse_config(yaml, Utf8Path::new("/test/.vault/config.yaml")).unwrap();
        assert_eq!(cfg.repair.rules.len(), 1);
        let action = cfg.repair.rules[0].action();
        match action {
            RepairAction::AddFrontmatter { field, value } => {
                assert_eq!(field, "kind");
                assert_eq!(value, serde_json::json!("research"));
            }
            _ => panic!("expected AddFrontmatter"),
        }
    }

    #[test]
    fn move_document_with_to_directory_parses() {
        let yaml = r#"
repair:
  rules:
    - name: route-tasks
      match:
        code: document-misrouted
      move_document:
        to_directory: "Workspaces/demo/tasks/"
"#;
        let cfg = parse_config(yaml, Utf8Path::new("/test/.vault/config.yaml")).unwrap();
        let action = cfg.repair.rules[0].action();
        match action {
            RepairAction::MoveDocument { destination } => match destination {
                DestinationSpec::Directory { to_directory } => {
                    assert_eq!(to_directory, "Workspaces/demo/tasks/");
                }
                _ => panic!("expected DestinationSpec::Directory"),
            },
            _ => panic!("expected MoveDocument"),
        }
    }

    #[test]
    fn move_document_with_to_path_parses() {
        let yaml = r#"
repair:
  rules:
    - name: route-tasks
      match:
        code: document-misrouted
      move_document:
        to_path: "Workspaces/demo/tasks/{stem}.md"
"#;
        let cfg = parse_config(yaml, Utf8Path::new("/test/.vault/config.yaml")).unwrap();
        let action = cfg.repair.rules[0].action();
        match action {
            RepairAction::MoveDocument { destination } => match destination {
                DestinationSpec::Path { to_path } => {
                    assert_eq!(to_path, "Workspaces/demo/tasks/{stem}.md");
                }
                _ => panic!("expected DestinationSpec::Path"),
            },
            _ => panic!("expected MoveDocument"),
        }
    }

    #[test]
    fn move_document_with_both_to_directory_and_to_path_rejects() {
        let yaml = r#"
repair:
  rules:
    - name: bad
      match:
        code: document-misrouted
      move_document:
        to_directory: "x/"
        to_path: "y/{stem}.md"
"#;
        let err = parse_config(yaml, Utf8Path::new("/test/.vault/config.yaml")).unwrap_err();
        assert!(format!("{err}").contains("exactly one"), "got: {err}");
    }

    #[test]
    fn repair_rule_with_multiple_actions_rejects() {
        let yaml = r#"
repair:
  rules:
    - name: bad
      match:
        code: x
      set_frontmatter:
        field: a
        value: 1
      add_frontmatter:
        field: a
        value: 2
"#;
        let err = parse_config(yaml, Utf8Path::new("/test/.vault/config.yaml")).unwrap_err();
        assert!(format!("{err}").contains("declares") && format!("{err}").contains("pick one"));
    }

    #[test]
    fn config_without_version_defaults_to_v1() {
        let yaml = "files:\n  ignore: []\n";
        let cfg: VaultConfig = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn config_with_explicit_version_1_parses() {
        let yaml = "version: 1\nfiles:\n  ignore: []\n";
        let cfg: VaultConfig = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(cfg.version, 1);
    }

    #[test]
    fn config_with_unknown_version_parses_but_value_preserved() {
        // We intentionally accept unknown versions at parse-time so
        // `vault config validate` can surface them as findings rather
        // than hard parse errors. Reject-at-validate keeps the
        // diagnostic surface uniform.
        let yaml = "version: 99\n";
        let cfg: VaultConfig = serde_yaml::from_str(yaml).expect("parses");
        assert_eq!(cfg.version, 99);
    }

    #[test]
    fn links_alias_field_parses() {
        let yaml = "links:\n  alias_field: aliases\n";
        let cfg = parse(yaml).unwrap();
        assert_eq!(cfg.links.alias_field.as_deref(), Some("aliases"));
    }

    #[test]
    fn links_section_absent_defaults_to_none() {
        let yaml = "files:\n  ignore: []\n";
        let cfg = parse(yaml).unwrap();
        assert!(cfg.links.alias_field.is_none());
    }

    #[test]
    fn links_alias_field_as_list_is_rejected() {
        let err = parse("links:\n  alias_field:\n    - aliases\n").unwrap_err();
        assert!(err.to_string().contains("invalid"), "got: {err}");
    }

    #[test]
    fn links_unknown_field_is_rejected() {
        let err = parse("links:\n  notakey: x\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "got: {err}");
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

    #[test]
    fn config_load_rejects_invalid_path_pattern() {
        let yaml = r#"
validate:
  rules:
    - name: bad
      match:
        path: "Workspaces/{{unclosed/foo.md"
"#;
        let err = parse_config_compiled(yaml, Utf8Path::new(".vault/config.yaml")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid path pattern"), "got: {msg}");
        assert!(msg.contains("bad"), "got: {msg}");
    }
}
