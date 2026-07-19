use std::collections::HashMap;

use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::standards::duration::parse_duration;
use crate::standards::path_match::{effective_match_glob, PathPattern, PathPatternError};

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
    #[serde(default)]
    pub templates: TemplatesConfig,
    /// Mutation-telemetry settings; wired into the applier-path event sink.
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
    /// Cache-lifecycle settings; see CacheConfig.
    #[serde(default)]
    pub cache: Option<CacheConfig>,
    /// Inbox settings for document creation routing.
    #[serde(default)]
    pub inbox: InboxConfig,
    /// Derived frontmatter index settings (the auto-index toggle).
    #[serde(default)]
    pub index: IndexConfig,
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
            templates: TemplatesConfig::default(),
            telemetry: None,
            cache: None,
            inbox: InboxConfig::default(),
            index: IndexConfig::default(),
            _deprecated_graph: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplatesConfig {
    #[serde(default = "default_date_format")]
    pub date_format: String,
    #[serde(default = "default_time_format")]
    pub time_format: String,
}

impl Default for TemplatesConfig {
    fn default() -> Self {
        Self {
            date_format: default_date_format(),
            time_format: default_time_format(),
        }
    }
}

fn default_date_format() -> String {
    "YYYY-MM-DD".into()
}

fn default_time_format() -> String {
    "HH:mm".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub location: Option<String>,
    /// Parsed from a duration string (e.g. "90d"); None when absent or
    /// unparseable (best-effort — a malformed value does not fail config load).
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub retention: Option<std::time::Duration>,
}

/// Default mutation-telemetry retention when unconfigured: 90 days.
pub const DEFAULT_RETENTION: std::time::Duration = std::time::Duration::from_secs(90 * 86_400);

/// Cache-lifecycle settings; wired into `norn cache prune` and the lazy sweep.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Age-eviction window, parsed from a duration string (e.g. "90d");
    /// None when absent or unparseable (best-effort, like telemetry.retention).
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub retention: Option<std::time::Duration>,
    /// "lazy" (default) or "manual". Unknown values fall back to lazy.
    #[serde(default)]
    pub prune: Option<String>,
}

impl CacheConfig {
    /// Whether the per-invocation lazy sweep is enabled. Unknown values
    /// warn (stderr) and default to lazy, per the warn-don't-block posture.
    pub fn lazy_prune_enabled(&self) -> bool {
        match self.prune.as_deref() {
            Some("manual") => false,
            None | Some("lazy") => true,
            Some(other) => {
                eprintln!("warn: unknown cache.prune value '{other}' (expected lazy|manual); defaulting to lazy");
                true
            }
        }
    }
}

/// Default cache age-eviction window when unconfigured: 90 days.
/// Deliberately its own constant — `DEFAULT_RETENTION` stays event-scoped.
pub const DEFAULT_CACHE_RETENTION: std::time::Duration =
    std::time::Duration::from_secs(90 * 86_400);

/// serde adapter for the duration-string fields `TelemetryConfig::retention`
/// and `CacheConfig::retention`. Best-effort: a malformed duration string
/// falls back to `None` rather than failing the whole config load.
fn de_opt_duration<'de, D>(d: D) -> Result<Option<std::time::Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(d)?;
    Ok(opt.and_then(|s| parse_duration(&s)))
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InboxConfig {
    #[serde(default)]
    pub path: Option<String>,
}

/// Derived frontmatter index settings, consumed by the cache writer and
/// query router (Wave 2). `auto` gates automatic indexing of bounded-type
/// fields; see `resolved_index_set`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    #[serde(default = "default_index_auto")]
    pub auto: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self { auto: true }
    }
}

fn default_index_auto() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateConfig {
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub required_frontmatter: Vec<String>,
    #[serde(default)]
    pub rules: Vec<ValidateRule>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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
    pub field_types: HashMap<String, FieldTypeSpec>,
    #[serde(default)]
    pub allowed_values: HashMap<String, Vec<serde_json::Value>>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub frontmatter_defaults: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub field_references: HashMap<String, FieldReferenceConstraint>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// Typed-reference constraint for one frontmatter field: the field's
/// wikilink target(s) must resolve to documents whose `type` is in the
/// allowed set. `target_type` accepts a scalar string or a non-empty list
/// (any-of), mirroring `match.frontmatter` selector values.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldReferenceConstraint {
    pub target_type: serde_json::Value,
}

impl FieldReferenceConstraint {
    /// The allowed target types as a flat list, however they were authored.
    pub fn allowed_types(&self) -> Vec<String> {
        match &self.target_type {
            serde_json::Value::Array(values) => values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect(),
            serde_json::Value::String(value) => vec![value.clone()],
            _ => Vec::new(),
        }
    }
}

/// Default bound applied to a `string` field, or to each element of a
/// `list_of_strings` field, when the rule doesn't declare `max_length`.
pub const DEFAULT_STRING_MAX_LENGTH: u32 = 64;

/// The hard ceiling a declared `max_length` may not exceed.
pub const STRING_MAX_LENGTH_CEILING: u32 = 256;

/// A `field_types` declaration: either the bare type-name string (`created:
/// datetime`) or the extended object form (`project: { type: string,
/// max_length: 32 }`). Both are accepted anywhere a field type is declared.
///
/// `type` is optional in the extended form only for the type-less
/// indexed-only shape (`{ indexed: true }`) — see `post_validate`, which
/// enforces that an extended entry either declares `type` or declares
/// `indexed` and nothing else.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum FieldTypeSpec {
    Bare(String),
    Extended(FieldTypeDecl),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldTypeDecl {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed: Option<bool>,
}

impl FieldTypeSpec {
    /// The declared type name, if any. `None` only for the type-less
    /// indexed-only extended form (`{ indexed: bool }`); such an entry
    /// contributes no type — validate/checks and set-coercion treat the
    /// field as undeclared for every purpose but the index vote.
    pub fn type_name(&self) -> Option<&str> {
        match self {
            FieldTypeSpec::Bare(name) => Some(name),
            FieldTypeSpec::Extended(decl) => decl.type_name.as_deref(),
        }
    }

    /// The declared `max_length`, if any. `None` for the bare form and for
    /// the extended form when the key is absent.
    pub fn max_length(&self) -> Option<u32> {
        match self {
            FieldTypeSpec::Bare(_) => None,
            FieldTypeSpec::Extended(decl) => decl.max_length,
        }
    }

    /// The declared `indexed` override, if any.
    pub fn indexed(&self) -> Option<bool> {
        match self {
            FieldTypeSpec::Bare(_) => None,
            FieldTypeSpec::Extended(decl) => decl.indexed,
        }
    }

    /// The bound actually enforced for a bounded scalar type: the declared
    /// `max_length` if present, else `DEFAULT_STRING_MAX_LENGTH`. `None` for
    /// types that carry no length bound (`text`, `datetime`, ...).
    pub fn effective_max_length(&self) -> Option<u32> {
        match self.type_name() {
            Some("string") | Some("list_of_strings") => {
                Some(self.max_length().unwrap_or(DEFAULT_STRING_MAX_LENGTH))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSelector {
    pub path: Option<String>,
    pub path_not: Option<String>,
    #[serde(default)]
    pub frontmatter: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
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

/// Pre-compiled path patterns for `validate.ignore`.
/// Each entry in the vec corresponds to the pattern string at the same index
/// in the source `Vec<String>`.
///
/// `files.ignore` has no compiled field here: it is applied at cache-build time
/// via the graph scan gate (`Cache::files_ignore`, threaded from
/// `config.files.ignore`), which matches with the cheap segment matcher rather
/// than a compiled `PathPattern` (NRN-117, ADR 0007).
#[derive(Debug, Clone, Default)]
pub struct CompiledConfig {
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

    // files.ignore is not compiled to PathPatterns here: it is enforced at
    // cache-build time by the graph scan gate's segment matcher (NRN-117), not
    // by this regex-glob path. validate.ignore still compiles — it is consumed
    // by the validation loop.
    let validate_ignore = compile_vec(&cfg.validate.ignore, "validate.ignore", source_path)?;

    let mut compiled_rules = Vec::with_capacity(cfg.validate.rules.len());
    for rule in &cfg.validate.rules {
        let rule_label = rule.name.as_deref().unwrap_or("unnamed validate rule");
        // Derive the path matcher from `target` when `match.path` is absent.
        // post_validate already enforces that they are mutually exclusive, so
        // only one of these two branches can produce a Some(_).
        let path = if rule.r#match.path.is_none() {
            if let Some(target) = &rule.target {
                Some(
                    crate::standards::path_match::pattern_from_target(target).map_err(
                        |e: PathPatternError| ConfigError::Invalid {
                            source_path: source_path.to_owned(),
                            message: format!(
                                "rule {rule_label}: target `{target}` produced invalid path pattern: {e}"
                            ),
                        },
                    )?,
                )
            } else {
                None
            }
        } else {
            compile_optional(
                &rule.r#match.path,
                &format!("rule {rule_label}: match.path"),
                source_path,
            )?
        };
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

        // Fields sorted for deterministic error output (HashMap iteration order
        // is otherwise arbitrary, so a multi-invalid-field config would name a
        // run-to-run-varying offender).
        let mut field_type_entries: Vec<(&String, &FieldTypeSpec)> =
            rule.field_types.iter().collect();
        field_type_entries.sort_by_key(|(field, _)| field.as_str());
        for (field, spec) in field_type_entries {
            let Some(ty) = spec.type_name() else {
                // Extended form without `type` is valid only as the type-less
                // indexed-only shape (`{ indexed: bool }`, no other keys).
                if spec.max_length().is_some() || spec.indexed().is_none() {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: field_types.{field} must declare `type`, or be a type-less indexed-only entry (`{{ indexed: true|false }}`)"
                        ),
                    });
                }
                continue;
            };
            if !is_known_field_type(ty) {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "rule {rule_label}: unknown field_type '{ty}' for field '{field}'; expected one of: datetime, date, list_of_strings, wikilink, wikilink_or_list, string, text"
                    ),
                });
            }

            if let Some(max_length) = spec.max_length() {
                if !matches!(ty, "string" | "list_of_strings") {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: field_types.{field}.max_length is only valid for 'string' or 'list_of_strings' (field '{field}' is '{ty}')"
                        ),
                    });
                }
                if !(1..=STRING_MAX_LENGTH_CEILING).contains(&max_length) {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: field_types.{field}.max_length must be between 1 and {STRING_MAX_LENGTH_CEILING} (got {max_length})"
                        ),
                    });
                }
            }
        }

        // allowed_values: non-empty, scalar values only.
        // Fields sorted for deterministic error output.
        let mut allowed_value_entries: Vec<(&String, &Vec<serde_json::Value>)> =
            rule.allowed_values.iter().collect();
        allowed_value_entries.sort_by_key(|(field, _)| field.as_str());
        for (field, values) in allowed_value_entries {
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

        // Frontmatter predicate values: a scalar (exact match) or a non-empty
        // list of scalars (any-of). Fields sorted for deterministic error output.
        let mut frontmatter_predicate_entries: Vec<(&String, &serde_json::Value)> =
            rule.r#match.frontmatter.iter().collect();
        frontmatter_predicate_entries.sort_by_key(|(field, _)| field.as_str());
        for (field, value) in frontmatter_predicate_entries {
            match value {
                serde_json::Value::Array(options) => {
                    if options.is_empty() {
                        return Err(ConfigError::Invalid {
                            source_path: source_path.to_owned(),
                            message: format!(
                                "rule {rule_label}: match.frontmatter.{field} is an empty list; an any-of selector needs at least one value"
                            ),
                        });
                    }
                    // Null is excluded: it can never match a document value, so
                    // a null any-of option would be silently inert.
                    if !options
                        .iter()
                        .all(|option| is_scalar_json_value(option) && !option.is_null())
                    {
                        return Err(ConfigError::Invalid {
                            source_path: source_path.to_owned(),
                            message: format!(
                                "rule {rule_label}: match.frontmatter.{field} list elements must be strings, booleans, or numbers"
                            ),
                        });
                    }
                }
                value if !is_scalar_json_value(value) => {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: match.frontmatter.{field} must be a string, boolean, or number, or a list of those (any-of)"
                        ),
                    });
                }
                _ => {}
            }
        }

        // field_references: target_type must be a non-empty string or a
        // non-empty list of non-empty strings. "(missing)" is reserved — it
        // is the finding sentinel for a target without a `type` field, and
        // allowing it in config would silently pass every untyped target.
        // Fields sorted for deterministic error output.
        let mut reference_fields: Vec<(&String, &FieldReferenceConstraint)> =
            rule.field_references.iter().collect();
        reference_fields.sort_by_key(|(field, _)| field.as_str());
        for (field, constraint) in reference_fields {
            let is_valid_name = |value: &serde_json::Value| {
                value
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s != "(missing)")
            };
            let valid = match &constraint.target_type {
                serde_json::Value::Array(values) => {
                    !values.is_empty() && values.iter().all(is_valid_name)
                }
                value => is_valid_name(value),
            };
            if !valid {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "rule {rule_label}: field_references.{field}.target_type must be a type name or a non-empty list of type names (\"(missing)\" is reserved)"
                    ),
                });
            }
        }

        // frontmatter_defaults: path.X references must be declared in this rule's match.path.
        let declared: std::collections::BTreeSet<String> = rule
            .r#match
            .path
            .as_deref()
            .and_then(|p| {
                crate::standards::path_match::PathPattern::parse(p)
                    .ok()
                    .map(|pp| pp.declared_variables().into_iter().collect())
            })
            .unwrap_or_default();
        // Fields sorted for deterministic error output.
        let mut default_entries: Vec<(&String, &serde_json::Value)> =
            rule.frontmatter_defaults.iter().collect();
        default_entries.sort_by_key(|(field, _)| field.as_str());
        for (field, value) in &default_entries {
            let Some(s) = value.as_str() else {
                continue;
            };
            for referenced in crate::standards::template_refs::collect_path_var_refs(s) {
                if !declared.contains(&referenced) {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: field `{field}` references {{{{path.{referenced}}}}} which is not declared in this rule's match.path"
                        ),
                    });
                }
            }
        }

        // frontmatter_defaults: transforms must be known.
        // Reuses the sorted `default_entries` above for deterministic output.
        for (field, value) in &default_entries {
            let Some(s) = value.as_str() else {
                continue;
            };
            for t in crate::standards::template_refs::collect_transform_refs(s) {
                if !crate::standards::template_refs::KNOWN_TRANSFORMS.contains(&t.as_str()) {
                    return Err(ConfigError::Invalid {
                        source_path: source_path.to_owned(),
                        message: format!(
                            "rule {rule_label}: field `{field}` uses unknown transform `{t}`"
                        ),
                    });
                }
            }
        }

        // target: requires name; mutually exclusive with match.path; must not
        // start with `/` (vault paths are relative — a leading slash would cause
        // the derived matcher to strip it while generate_path does not, breaking
        // the round-trip between generation and matching).
        if let Some(target) = &rule.target {
            if rule.name.is_none() {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: "a rule declaring `target` must also declare `name` (target is the creation handle)".into(),
                });
            }
            if rule.r#match.path.is_some() {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "rule `{}` declares both `target` and `match.path`; they are mutually exclusive (the matcher is derived from `target`)",
                        rule.name.as_deref().unwrap_or("?")
                    ),
                });
            }
            if target.starts_with('/') {
                return Err(ConfigError::Invalid {
                    source_path: source_path.to_owned(),
                    message: format!(
                        "rule `{}`: `target` must be a relative vault path (remove the leading '/')",
                        rule.name.as_deref().unwrap_or("?")
                    ),
                });
            }
        }
    }

    // frontmatter_defaults: reject conflicting values for the same field across
    // rules that can co-apply to the same document. Rules whose match predicates
    // are provably disjoint (divergent literal path segments, or incompatible
    // frontmatter predicates) can never both fire on one document, so differing
    // defaults between them are not a conflict — e.g. tasks/ → `type: task` and
    // notes/ → `type: note` is legal.
    {
        // A path-glob segment is "literal" when it carries no glob metacharacter
        // and no `{{capture}}` — it must match exactly that text.
        fn is_literal_segment(seg: &str) -> bool {
            !seg.contains(['*', '?', '{', '}', '[', ']'])
        }

        // Sound, conservative path-disjointness test: walk aligned segments
        // left-to-right; if both sides hold differing literals before either
        // reaches a `**` (which matches any number of segments and breaks
        // positional alignment), the globs can never match the same path. When
        // uncertain, return false (assume they may overlap, keeping the guard).
        fn path_globs_disjoint(a: &str, b: &str) -> bool {
            for (seg_a, seg_b) in a.split('/').zip(b.split('/')) {
                if seg_a == "**" || seg_b == "**" {
                    return false;
                }
                if is_literal_segment(seg_a) && is_literal_segment(seg_b) && seg_a != seg_b {
                    return true;
                }
            }
            false
        }

        // Two rules can co-apply unless their match predicates are provably
        // disjoint: a shared frontmatter predicate demanding different values, or
        // concrete path globs that cannot intersect. `exclude` / `path_not` are
        // ignored — they only shrink a rule's match set, so skipping them keeps
        // the test conservative (it never under-reports possible overlap).
        //
        // For creatable (target-based) rules, match.path is None but the effective
        // path glob is derived from the target template by effective_match_glob
        // (the shared helper in path_match). Using it here keeps the notes-scoped
        // target rule correctly disjoint from a tasks-scoped match.path rule,
        // instead of treating it as path-unconstrained.
        // A predicate value viewed as its set of accepted scalars: an any-of
        // list is its elements; a scalar is a one-element set.
        fn predicate_options(value: &serde_json::Value) -> &[serde_json::Value] {
            match value {
                serde_json::Value::Array(options) => options,
                scalar => std::slice::from_ref(scalar),
            }
        }

        fn rules_can_coapply(a: &ValidateRule, b: &ValidateRule) -> bool {
            for (k, va) in &a.r#match.frontmatter {
                if let Some(vb) = b.r#match.frontmatter.get(k) {
                    // Overlap = some value accepted by `a` is also accepted by
                    // `b`, judged by the engine's own predicate semantics so
                    // the guard can never drift from what actually fires.
                    let overlaps = predicate_options(va).iter().any(|option| {
                        crate::standards::predicates::frontmatter_predicate_matches(option, vb)
                    });
                    if !overlaps {
                        return false;
                    }
                }
            }
            let ea = effective_match_glob(a.r#match.path.as_deref(), a.target.as_deref());
            let eb = effective_match_glob(b.r#match.path.as_deref(), b.target.as_deref());
            match (ea, eb) {
                (Some(pa), Some(pb)) => !path_globs_disjoint(&pa, &pb),
                // Either rule is genuinely unconstrained — conservatively assume overlap.
                _ => true,
            }
        }

        let rules = &cfg.validate.rules;
        for (i, rule_a) in rules.iter().enumerate() {
            let label_a = rule_a.name.as_deref().unwrap_or("(unnamed)");
            for rule_b in rules.iter().skip(i + 1) {
                if !rules_can_coapply(rule_a, rule_b) {
                    continue;
                }
                for (field, val_a) in &rule_a.frontmatter_defaults {
                    if let Some(val_b) = rule_b.frontmatter_defaults.get(field) {
                        if val_a != val_b {
                            let label_b = rule_b.name.as_deref().unwrap_or("(unnamed)");
                            return Err(ConfigError::Invalid {
                                source_path: source_path.to_owned(),
                                message: format!(
                                    "conflicting frontmatter_defaults for field `{field}`: rule `{label_a}` and rule `{label_b}` declare different values"
                                ),
                            });
                        }
                    }
                }
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
        "datetime"
            | "date"
            | "list_of_strings"
            | "wikilink"
            | "wikilink_or_list"
            | "string"
            | "text"
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
        parse_config(yaml, Utf8Path::new("/test/.norn/config.yaml"))
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
    fn multi_invalid_field_types_report_lexicographically_first_offender_deterministically() {
        // Two invalid field_types in one rule: `apple` and `zebra`. HashMap
        // iteration order is arbitrary, so without a sort the reported offender
        // would vary run-to-run. Keys are sorted, so the error must always name
        // `apple` (lexicographically first) and never bail on `zebra` first.
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        zebra: bogus_z\n        apple: bogus_a\n";
        for _ in 0..64 {
            let msg = parse(yaml).unwrap_err().to_string();
            assert!(
                msg.contains("for field 'apple'") && msg.contains("bogus_a"),
                "expected the lexicographically-first offender (apple), got: {msg}"
            );
            assert!(
                !msg.contains("zebra"),
                "must bail on apple before reaching zebra, got: {msg}"
            );
        }
    }

    // ── NRN-77: string/text field types, max_length, indexed ─────────────────

    #[test]
    fn bare_string_and_text_field_types_parse() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: string\n        notes: text\n";
        let cfg = parse(yaml).unwrap();
        let rule = &cfg.validate.rules[0];
        assert_eq!(rule.field_types["project"].type_name(), Some("string"));
        assert_eq!(rule.field_types["project"].effective_max_length(), Some(64));
        assert_eq!(rule.field_types["notes"].type_name(), Some("text"));
        assert_eq!(rule.field_types["notes"].effective_max_length(), None);
    }

    #[test]
    fn extended_field_type_form_parses_max_length_and_indexed() {
        let yaml = r#"
validate:
  rules:
    - name: r
      field_types:
        project: { type: string, max_length: 32 }
        notes: { type: text }
        status: { type: string, indexed: false }
"#;
        let cfg = parse(yaml).unwrap();
        let rule = &cfg.validate.rules[0];
        assert_eq!(rule.field_types["project"].type_name(), Some("string"));
        assert_eq!(rule.field_types["project"].max_length(), Some(32));
        assert_eq!(rule.field_types["project"].effective_max_length(), Some(32));
        assert_eq!(rule.field_types["notes"].type_name(), Some("text"));
        assert_eq!(rule.field_types["notes"].max_length(), None);
        assert_eq!(rule.field_types["status"].indexed(), Some(false));
    }

    #[test]
    fn max_length_above_ceiling_is_rejected() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { type: string, max_length: 257 }\n";
        let err = parse(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("max_length"), "got: {msg}");
        assert!(msg.contains("between 1 and 256"), "got: {msg}");
    }

    #[test]
    fn max_length_zero_is_rejected() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { type: string, max_length: 0 }\n";
        let err = parse(yaml).unwrap_err();
        assert!(err.to_string().contains("max_length"), "got: {err}");
    }

    #[test]
    fn max_length_on_text_is_rejected() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        notes: { type: text, max_length: 32 }\n";
        let err = parse(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("max_length"), "got: {msg}");
        assert!(
            msg.contains("string") || msg.contains("list_of_strings"),
            "got: {msg}"
        );
    }

    #[test]
    fn max_length_on_datetime_is_rejected() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        created: { type: datetime, max_length: 32 }\n";
        let err = parse(yaml).unwrap_err();
        assert!(err.to_string().contains("max_length"), "got: {err}");
    }

    #[test]
    fn max_length_on_list_of_strings_is_accepted() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        tags: { type: list_of_strings, max_length: 32 }\n";
        let cfg = parse(yaml).unwrap();
        assert_eq!(
            cfg.validate.rules[0].field_types["tags"].effective_max_length(),
            Some(32)
        );
    }

    #[test]
    fn field_type_extended_form_rejects_unknown_key() {
        // `FieldTypeSpec` is an untagged enum (bare string | extended object);
        // serde discards the nested deny_unknown_fields message in favor of a
        // generic "no variant matched" error, but it's still a config load
        // error, which is what this test guards.
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { type: string, bogus_key: 1 }\n";
        let err = parse(yaml).unwrap_err();
        assert!(
            err.to_string().contains("did not match any variant"),
            "got: {err}"
        );
    }

    #[test]
    fn field_type_extended_form_requires_type_key() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { max_length: 32 }\n";
        assert!(parse(yaml).is_err());
    }

    // ── Fix 2: FieldTypeDecl omits absent Option fields when serialized ──────

    #[test]
    fn field_type_spec_bare_form_serializes_as_plain_string() {
        let spec = FieldTypeSpec::Bare("string".to_string());
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(value, serde_json::json!("string"));
    }

    #[test]
    fn field_type_spec_extended_form_omits_absent_indexed_key() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { type: string, max_length: 32 }\n";
        let cfg = parse(yaml).unwrap();
        let value = serde_json::to_value(&cfg.validate.rules[0].field_types["project"]).unwrap();
        let obj = value
            .as_object()
            .expect("extended form serializes as an object");
        assert!(
            !obj.contains_key("indexed"),
            "indexed should be omitted when absent; got: {value}"
        );
        assert_eq!(obj["type"], "string");
        assert_eq!(obj["max_length"], 32);
    }

    // ── Fix 3: type-less indexed-only extended form ──────────────────────────

    #[test]
    fn field_type_extended_form_indexed_only_parses_without_type() {
        let yaml =
            "validate:\n  rules:\n    - name: r\n      field_types:\n        notes: { indexed: true }\n";
        let cfg = parse(yaml).unwrap();
        let spec = &cfg.validate.rules[0].field_types["notes"];
        assert_eq!(spec.type_name(), None);
        assert_eq!(spec.indexed(), Some(true));
        assert_eq!(spec.effective_max_length(), None);
    }

    #[test]
    fn field_type_extended_form_indexed_and_max_length_without_type_is_rejected() {
        let yaml = "validate:\n  rules:\n    - name: r\n      field_types:\n        project: { indexed: false, max_length: 5 }\n";
        assert!(parse(yaml).is_err());
    }

    // ── index.auto ─────────────────────────────────────────────────────────

    #[test]
    fn index_auto_defaults_true_when_absent() {
        let cfg = parse("").unwrap();
        assert!(cfg.index.auto);
    }

    #[test]
    fn index_auto_can_be_set_false() {
        let cfg = parse("index:\n  auto: false\n").unwrap();
        assert!(!cfg.index.auto);
    }

    #[test]
    fn index_unknown_field_is_rejected() {
        let err = parse("index:\n  notakey: x\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "got: {err}");
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
        let cfg = parse_config(yaml, Utf8Path::new("/test/.norn/config.yaml")).unwrap();
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
        let cfg = parse_config(yaml, Utf8Path::new("/test/.norn/config.yaml")).unwrap();
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
        let cfg = parse_config(yaml, Utf8Path::new("/test/.norn/config.yaml")).unwrap();
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
        let err = parse_config(yaml, Utf8Path::new("/test/.norn/config.yaml")).unwrap_err();
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
        let err = parse_config(yaml, Utf8Path::new("/test/.norn/config.yaml")).unwrap_err();
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
        // `norn config validate` can surface them as findings rather
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
        code: value-not-allowed
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
        let err = parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid path pattern"), "got: {msg}");
        assert!(msg.contains("bad"), "got: {msg}");
    }

    #[test]
    fn parses_frontmatter_defaults() {
        let yaml = r#"
validate:
  rules:
    - name: task-rule
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      required_frontmatter: [type, status]
      frontmatter_defaults:
        type: task
        status: backlog
        workspace: "[[{{path.workspace}}]]"
        created: "{{now}}"
"#;
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        let rule = &cfg.validate.rules[0];
        assert_eq!(
            rule.frontmatter_defaults.get("type"),
            Some(&serde_json::json!("task"))
        );
        assert_eq!(
            rule.frontmatter_defaults.get("status"),
            Some(&serde_json::json!("backlog"))
        );
        assert_eq!(rule.frontmatter_defaults.len(), 4);
    }

    #[test]
    fn frontmatter_defaults_optional_and_empty_by_default() {
        let yaml = r#"
validate:
  rules:
    - name: any
      match:
        path: "**/*.md"
"#;
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        assert!(cfg.validate.rules[0].frontmatter_defaults.is_empty());
    }

    #[test]
    fn config_load_rejects_unknown_path_var_in_default() {
        let yaml = r#"
validate:
  rules:
    - name: r
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        title: "{{path.bogus}}"
"#;
        let err = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("rule r") || msg.contains("`r`"),
            "msg was {msg}"
        );
        assert!(
            msg.contains("path.bogus") || msg.contains("bogus"),
            "msg was {msg}"
        );
        assert!(
            msg.contains("not declared")
                || msg.contains("undeclared")
                || msg.contains("not defined"),
            "msg was {msg}"
        );
    }

    #[test]
    fn config_load_accepts_known_path_var_in_default() {
        let yaml = r#"
validate:
  rules:
    - name: r
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        workspace: "[[{{path.workspace}}]]"
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn config_load_rejects_unknown_transform_in_default() {
        let yaml = r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        title: "{{title | bogus_transform}}"
"#;
        let err = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown transform") || msg.contains("transform"),
            "msg was {msg}"
        );
        assert!(
            msg.contains("bogus_transform") || msg.contains("bogus"),
            "msg was {msg}"
        );
    }

    #[test]
    fn config_load_accepts_known_transforms() {
        let yaml = r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        title: "{{title | strip_date_prefix | titlecase}}"
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn config_load_rejects_conflicting_defaults_across_rules() {
        let yaml = r#"
validate:
  rules:
    - name: a
      match:
        path: "**/*.md"
      frontmatter_defaults:
        status: backlog
    - name: b
      match:
        path: "tasks/**/*.md"
      frontmatter_defaults:
        status: in_progress
"#;
        let err = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("conflict") || msg.contains("conflicting"),
            "msg was {msg}"
        );
        assert!(msg.contains("status"), "msg was {msg}");
    }

    #[test]
    fn config_load_accepts_identical_defaults_across_rules() {
        let yaml = r#"
validate:
  rules:
    - name: a
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
    - name: b
      match:
        path: "notes/**/*.md"
      frontmatter_defaults:
        type: note
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn config_load_accepts_disjoint_path_rules_with_differing_defaults() {
        // tasks/ → type: task and notes/ → type: note diverge on a literal path
        // segment, so the two rules can never co-apply: not a conflict.
        let yaml = r#"
validate:
  rules:
    - name: task-folder
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        type: task
    - name: note-folder
      match:
        path: "Workspaces/{{workspace}}/notes/**/*.md"
      frontmatter_defaults:
        type: note
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn config_load_accepts_differing_defaults_when_frontmatter_predicates_disjoint() {
        // Same path glob, but match.frontmatter predicates demand incompatible
        // values — the rules cannot both fire on one document.
        let yaml = r#"
validate:
  rules:
    - name: note-rule
      match:
        path: "**/*.md"
        frontmatter:
          type: note
      frontmatter_defaults:
        status: backlog
    - name: task-rule
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      frontmatter_defaults:
        status: open
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn list_valued_match_frontmatter_parses() {
        let yaml = r#"
validate:
  rules:
    - name: base-node
      match:
        frontmatter:
          type: [task, phase, initiative]
      required_frontmatter:
        - title
"#;
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        assert_eq!(
            cfg.validate.rules[0].r#match.frontmatter["type"],
            serde_json::json!(["task", "phase", "initiative"])
        );
    }

    #[test]
    fn empty_list_match_frontmatter_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      match:\n        frontmatter:\n          type: []\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("match.frontmatter.type") && msg.contains("empty"),
            "got: {msg}"
        );
    }

    #[test]
    fn list_match_frontmatter_with_non_scalar_element_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      match:\n        frontmatter:\n          type:\n            - [a, b]\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("match.frontmatter.type"), "got: {msg}");
    }

    #[test]
    fn null_list_element_match_frontmatter_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      match:\n        frontmatter:\n          status: [null, open]\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("match.frontmatter.status")
                && msg.contains("strings, booleans, or numbers"),
            "got: {msg}"
        );
    }

    #[test]
    fn field_references_parses_scalar_and_list() {
        let yaml = r#"
validate:
  rules:
    - name: task-refs
      match:
        frontmatter:
          type: task
      field_references:
        parent:
          target_type: [phase, initiative]
        depends_on:
          target_type: task
"#;
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        let rule = &cfg.validate.rules[0];
        assert_eq!(
            rule.field_references["parent"].allowed_types(),
            vec!["phase".to_string(), "initiative".to_string()]
        );
        assert_eq!(
            rule.field_references["depends_on"].allowed_types(),
            vec!["task".to_string()]
        );
    }

    #[test]
    fn field_references_empty_target_type_list_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      field_references:\n        parent:\n          target_type: []\n",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("field_references.parent.target_type"),
            "got: {msg}"
        );
    }

    #[test]
    fn field_references_non_string_target_type_is_rejected() {
        for bad in [
            "target_type: 3",
            "target_type: [task, 3]",
            "target_type: {}",
        ] {
            let yaml = format!(
                "validate:\n  rules:\n    - name: r\n      field_references:\n        parent:\n          {bad}\n"
            );
            let err = parse(&yaml).unwrap_err();
            assert!(
                err.to_string()
                    .contains("field_references.parent.target_type")
                    || err.to_string().contains("invalid type"),
                "{bad} should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn field_references_unknown_key_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: r\n      field_references:\n        parent:\n          target_kind: [phase]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "got: {err}");
    }

    #[test]
    fn config_load_accepts_differing_defaults_when_null_predicates_never_fire() {
        // A scalar null predicate can never match a document (the engine's
        // predicate semantics reject null), so two such rules can never
        // co-apply — their differing defaults are not a conflict.
        let yaml = r#"
validate:
  rules:
    - name: a
      match:
        path: "**/*.md"
        frontmatter:
          flag: null
      frontmatter_defaults:
        status: x
    - name: b
      match:
        path: "**/*.md"
        frontmatter:
          flag: null
      frontmatter_defaults:
        status: y
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn config_load_accepts_differing_defaults_when_list_predicates_disjoint() {
        // The any-of sets {note, log} and {task} share no value, so the rules
        // can never co-apply — differing defaults are legal.
        let yaml = r#"
validate:
  rules:
    - name: note-like
      match:
        path: "**/*.md"
        frontmatter:
          type: [note, log]
      frontmatter_defaults:
        status: evergreen
    - name: task-like
      match:
        path: "**/*.md"
        frontmatter:
          type: [task]
      frontmatter_defaults:
        status: open
"#;
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
    }

    #[test]
    fn config_load_rejects_conflicting_defaults_when_list_predicates_intersect() {
        // {note, task} intersects {task} — both rules can fire on a task
        // document, so their differing defaults conflict.
        let yaml = r#"
validate:
  rules:
    - name: broad
      match:
        path: "**/*.md"
        frontmatter:
          type: [note, task]
      frontmatter_defaults:
        status: evergreen
    - name: narrow
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      frontmatter_defaults:
        status: open
"#;
        let err = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap_err();
        assert!(err.to_string().contains("conflict"), "msg was {err}");
    }

    #[test]
    fn config_load_still_rejects_conflict_when_paths_can_overlap() {
        // Both globs reach `**` before any literal divergence, so disjointness
        // cannot be proven — the conflict guard must still fire.
        let yaml = r#"
validate:
  rules:
    - name: a
      match:
        path: "Workspaces/**/*.md"
      frontmatter_defaults:
        type: note
    - name: b
      match:
        path: "Workspaces/**/foo.md"
      frontmatter_defaults:
        type: task
"#;
        let err = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap_err();
        assert!(err.to_string().contains("conflict"), "msg was {err}");
    }

    #[test]
    fn parses_templates_config_block() {
        let yaml = r#"
templates:
  date_format: "YYYY/MM/DD"
  time_format: "HH:mm:ss"
"#;
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        assert_eq!(cfg.templates.date_format, "YYYY/MM/DD");
        assert_eq!(cfg.templates.time_format, "HH:mm:ss");
    }

    #[test]
    fn templates_config_block_defaults_when_absent() {
        let yaml = "files:\n  ignore: []\n";
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        assert_eq!(cfg.templates.date_format, "YYYY-MM-DD");
        assert_eq!(cfg.templates.time_format, "HH:mm");
    }

    #[test]
    fn telemetry_config_parses_location_and_retention() {
        let cfg = parse("telemetry:\n  location: /tmp/foo\n  retention: 30d\n").unwrap();
        let t = cfg.telemetry.expect("telemetry section");
        assert_eq!(t.location.as_deref(), Some("/tmp/foo"));
        assert_eq!(
            t.retention,
            Some(std::time::Duration::from_secs(30 * 86_400))
        );
    }

    #[test]
    fn telemetry_absent_is_none() {
        let cfg = parse("validate: {}\n").unwrap();
        assert!(cfg.telemetry.is_none());
    }

    #[test]
    fn telemetry_malformed_retention_is_ignored_not_fatal() {
        let cfg = parse("telemetry:\n  retention: not-a-duration\n").unwrap();
        let t = cfg.telemetry.unwrap();
        assert!(t.retention.is_none(), "bad duration -> None, no error");
    }

    #[test]
    fn templates_config_block_partial_uses_defaults() {
        // Only date_format specified — time_format should fall back to default.
        let yaml = r#"
templates:
  date_format: "DD/MM/YYYY"
"#;
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        assert_eq!(cfg.templates.date_format, "DD/MM/YYYY");
        assert_eq!(cfg.templates.time_format, "HH:mm");
    }

    #[test]
    fn cache_block_parses_retention_and_prune() {
        let yaml = "version: 1\ncache:\n  retention: 30d\n  prune: manual\n";
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        let cache = cfg.cache.expect("cache block should parse");
        assert_eq!(
            cache.retention,
            Some(std::time::Duration::from_secs(30 * 86_400))
        );
        assert!(!cache.lazy_prune_enabled());
    }

    #[test]
    fn cache_block_defaults_and_malformed_retention() {
        // Absent block → None; malformed retention → None (best-effort);
        // absent/unknown prune → lazy enabled.
        let cfg = parse_config("version: 1\n", camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        assert!(cfg.cache.is_none());

        let yaml = "version: 1\ncache:\n  retention: nonsense\n  prune: sometimes\n";
        let cfg = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml")).unwrap();
        let cache = cfg.cache.expect("block parses despite malformed values");
        assert_eq!(cache.retention, None);
        assert!(
            cache.lazy_prune_enabled(),
            "unknown prune value falls back to lazy"
        );
    }

    #[test]
    fn rule_accepts_target_and_body() {
        let yaml = r###"
validate:
  rules:
    - name: task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
      body: "## Context\n"
      frontmatter_defaults:
        type: task
"###;
        let (cfg, _compiled) =
            parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).expect("parses");
        let r = &cfg.validate.rules[0];
        assert_eq!(
            r.target.as_deref(),
            Some("Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md")
        );
        // In YAML double-quoted strings, \n is a newline escape.
        assert_eq!(r.body.as_deref(), Some("## Context\n"));
    }

    #[test]
    fn inbox_block_parses() {
        let yaml = "inbox:\n  path: Inbox\n";
        let (cfg, _) =
            parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).expect("parses");
        assert_eq!(cfg.inbox.path.as_deref(), Some("Inbox"));
    }

    #[test]
    fn target_without_name_is_rejected() {
        let yaml = r#"
validate:
  rules:
    - target: "tasks/{{title|slugify}}.md"
"#;
        let err = parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("target") && err.contains("name"), "got: {err}");
    }

    #[test]
    fn target_with_match_path_is_rejected() {
        let yaml = r#"
validate:
  rules:
    - name: task
      target: "tasks/{{title|slugify}}.md"
      match:
        path: "tasks/*.md"
"#;
        let err = parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("target") && err.contains("match.path"),
            "got: {err}"
        );
    }

    // Regression: a target-based (creatable) rule must not falsely conflict with
    // a match.path rule when their effective path globs are disjoint.
    // Before the fix, target rules had match.path == None, causing rules_can_coapply
    // to treat them as path-unconstrained and trigger a false conflict error.
    #[test]
    fn target_based_rule_disjoint_from_match_path_rule_no_false_conflict() {
        let yaml = r#"
validate:
  rules:
    - name: notes
      target: "Workspaces/{{var.workspace}}/notes/{{title|slugify}}.md"
      frontmatter_defaults:
        type: note
    - name: tasks
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        type: task
"#;
        // notes/ and tasks/ are disjoint literal segments — config must load OK.
        parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml"))
            .expect("disjoint target and match.path rules should not conflict");
    }

    // Negative: a target-based rule and a match.path rule whose effective paths
    // DO overlap declaring different values for the same field must still error.
    #[test]
    fn target_based_rule_overlapping_path_still_conflicts() {
        let yaml = r#"
validate:
  rules:
    - name: notes-creatable
      target: "Workspaces/{{var.workspace}}/notes/{{title|slugify}}.md"
      frontmatter_defaults:
        type: note
    - name: notes-matcher
      match:
        path: "Workspaces/{{workspace}}/notes/*.md"
      frontmatter_defaults:
        type: article
"#;
        // Both rules target the notes/ folder — their effective globs overlap.
        // Different values for `type` must still be detected as a conflict.
        let err = parse_config(yaml, camino::Utf8Path::new(".norn/config.yaml"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("conflict") || err.contains("conflicting"),
            "expected conflict error, got: {err}"
        );
        assert!(
            err.contains("type"),
            "expected 'type' field in error, got: {err}"
        );
    }

    // ── Fix 3: leading-slash target is rejected ──────────────────────────────

    #[test]
    fn target_with_leading_slash_is_rejected() {
        let err = parse(
            "validate:\n  rules:\n    - name: tasks\n      target: \"/Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md\"\n"
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("relative") || msg.contains("leading '/'"),
            "expected leading-slash rejection, got: {msg}"
        );
    }

    #[test]
    fn target_without_leading_slash_is_accepted() {
        let cfg = parse(
            "validate:\n  rules:\n    - name: tasks\n      target: \"Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md\"\n"
        )
        .expect("relative target must be accepted");
        assert_eq!(cfg.validate.rules.len(), 1);
    }
}
