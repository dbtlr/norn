use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct VaultConfig {
    #[serde(default)]
    pub files: FilesConfig,
    #[serde(default)]
    pub validate: ValidateConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct FilesConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ValidateConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub required_frontmatter: Vec<String>,
    #[serde(default)]
    pub rules: Vec<ValidateRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ValidateRule {
    pub name: Option<String>,
    #[serde(default)]
    pub r#match: RuleSelector,
    #[serde(default)]
    pub required_frontmatter: Vec<String>,
    #[serde(default)]
    pub allowed_values: HashMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    pub field_types: HashMap<String, String>,
    #[serde(default)]
    pub forbidden_frontmatter: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub exclude: RuleExclude,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuleSelector {
    pub path: Option<String>,
    pub path_not: Option<String>,
    #[serde(default)]
    pub frontmatter: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuleExclude {
    pub path: Option<String>,
}
