use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::process;

use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use vault_core::{Diagnostic, Document, GraphIndex, Link, LinkStatus, Severity, VaultFile};
use vault_index::{
    build_index_with_options, concise_diagnostics, has_errors, pattern_matches_path,
    write_sqlite_cache, IndexOptions, ValidateConfig, ValidateRuleConfig, VaultConfig,
};

#[derive(Debug, Parser)]
#[command(name = "vault")]
#[command(about = "Deterministic Markdown vault graph tools")]
#[command(version)]
struct Cli {
    #[arg(
        short = 'C',
        long,
        global = true,
        default_value = ".",
        help = "Run as if vault started in this directory"
    )]
    cwd: Utf8PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Query and cache derived Markdown vault graph facts")]
    Graph(GraphCommand),
    #[command(
        about = "Validate vault graph facts and configured frontmatter rules",
        long_about = "Validate vault graph facts and configured frontmatter rules.\n\nValidation reuses graph/index facts to surface unresolved links, ambiguous links, document diagnostics, and configured frontmatter requirements. Validate does not mutate files."
    )]
    Validate(ValidateArgs),
}

#[derive(Debug, Parser)]
#[command(
    about = "Read-only graph/index commands for Markdown vaults",
    long_about = "Read-only graph/index commands for Markdown vaults.\n\nThe graph surface is a deterministic, read-only view of raw Markdown vault structure. It emits Obsidian-compatible link facts, document metadata, file inventory, diagnostics, and cache projections without applying standards-pack semantics or mutating files."
)]
struct GraphCommand {
    #[command(subcommand)]
    command: GraphSubcommand,
}

#[derive(Debug, Subcommand)]
enum GraphSubcommand {
    #[command(
        about = "Write a SQLite graph cache and emit a build summary",
        long_about = "Write a SQLite graph cache and emit a build summary.\n\nThe cache includes inventoried files, parsed Markdown documents, headings, block IDs, graph link facts, and diagnostics. --format only controls stdout; the cache is always SQLite."
    )]
    Build(BuildArgs),
    #[command(
        about = "Emit parsed Markdown documents with frontmatter, headings, links, and diagnostics"
    )]
    Documents(DocumentsArgs),
    #[command(
        about = "Emit all parsed link facts",
        long_about = "Emit all parsed link facts.\n\nIncludes body wikilinks, embeds, frontmatter/property wikilinks, URL-decoded Markdown internal links, extensionless Markdown note links, same-note heading/block references, Markdown image links to local files, and links to existing attachments. Use source_context.area and source_context.property to distinguish body links from frontmatter links."
    )]
    Links(GraphArgs),
    #[command(
        about = "Emit inventoried vault files",
        long_about = "Emit inventoried vault files.\n\nFiles include Markdown documents and non-Markdown attachments. File records can be used with exact-path backlink queries for resolved attachment targets."
    )]
    Files(GraphArgs),
    #[command(
        about = "Emit unresolved and ambiguous link facts",
        long_about = "Emit unresolved and ambiguous link facts.\n\nRows include target-missing, anchor-missing, block-ref-missing, and ambiguous reasons. Ambiguous rows include candidate document paths."
    )]
    Unresolved(GraphArgs),
    #[command(about = "Emit document parse diagnostics")]
    Diagnostics(GraphArgs),
    #[command(
        about = "Emit incoming links for an exact path or unique stem",
        long_about = "Emit incoming links for an exact vault-relative file path or unique document stem.\n\nExact paths may target Markdown documents or non-Markdown files. Stem matching only applies to Markdown documents and is case-insensitive."
    )]
    Backlinks(TargetGraphArgs),
    #[command(about = "Emit one document plus incoming, outgoing, and unresolved outgoing links")]
    Inspect(TargetGraphArgs),
}

#[derive(Debug, Parser)]
struct BuildArgs {
    #[arg(long, help = "YAML config file with graph.ignore patterns")]
    config: Option<Utf8PathBuf>,
    #[arg(
        long,
        help = "SQLite cache file path or directory. Directories receive graph.sqlite; --format only controls stdout"
    )]
    cache: Utf8PathBuf,
    #[arg(long, value_enum, default_value_t = OutputFormat::Json, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct DocumentsArgs {
    #[arg(long, help = "YAML config file with graph.ignore patterns")]
    config: Option<Utf8PathBuf>,
    #[arg(
        long = "filter",
        help = "Frontmatter-only field:value filter. Repeat to require multiple fields"
    )]
    filters: Vec<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct GraphArgs {
    #[arg(long, help = "YAML config file with graph.ignore patterns")]
    config: Option<Utf8PathBuf>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct TargetGraphArgs {
    #[arg(
        help = "Exact vault-relative path or unique document stem. Stem matching is case-insensitive"
    )]
    target: String,
    #[arg(long, help = "YAML config file with graph.ignore patterns")]
    config: Option<Utf8PathBuf>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Parser)]
struct ValidateArgs {
    #[arg(long, help = "YAML config file with graph.ignore and validate rules")]
    config: Option<Utf8PathBuf>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Jsonl, help = "Stdout format")]
    format: OutputFormat,
    #[arg(
        long,
        help = "Emit grouped validation finding counts instead of raw findings"
    )]
    summary: bool,
    #[arg(long, help = "Include verbose diagnostic details")]
    verbose: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
    Jsonl,
}

#[derive(Debug, Serialize)]
struct InspectOutput {
    document: Document,
    incoming_links: Vec<Link>,
    outgoing_links: Vec<Link>,
    unresolved_outgoing_links: Vec<Link>,
}

#[derive(Debug, Serialize)]
struct DocumentDiagnostic {
    path: Utf8PathBuf,
    diagnostic: Diagnostic,
}

#[derive(Debug, Serialize)]
struct ValidateFinding {
    code: String,
    severity: Severity,
    path: Utf8PathBuf,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_values: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<Link>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic: Option<Diagnostic>,
}

#[derive(Debug, Serialize)]
struct ValidateSummary {
    findings: usize,
    codes: BTreeMap<String, usize>,
    severities: BTreeMap<String, usize>,
    rules: BTreeMap<String, usize>,
    fields: BTreeMap<String, usize>,
    disallowed_values: BTreeMap<String, BTreeMap<String, usize>>,
    path_prefixes: BTreeMap<String, usize>,
}

struct LoadedConfig {
    index_options: IndexOptions,
    validate: ValidateConfig,
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(exit_code) => process::exit(exit_code),
        Err(error) if is_broken_pipe(&error) => process::exit(0),
        Err(error) => {
            eprintln!("{error:#}");
            process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    let cwd = effective_cwd(&cli.cwd)?;

    match cli.command {
        Command::Graph(graph) => match graph.command {
            GraphSubcommand::Build(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let cache_path = resolve_path(&cwd, &args.cache);
                let summary = write_sqlite_cache(&index, &cache_path)?;
                write_item_output(&summary, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Documents(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let documents = filter_documents(&index, &args.filters)?;
                write_output(&documents, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Links(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let links = all_links(&index);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Files(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let files = all_files(&index);
                write_output(&files, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Unresolved(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let links = unresolved_links(&index);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Diagnostics(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let diagnostics = all_diagnostics(&index);
                write_output(&diagnostics, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Backlinks(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let target_path = resolve_backlink_target_path(&index, &args.target)?;
                let links = backlinks(&index, &target_path);
                write_output(&links, args.format)?;
                Ok(exit_code_for(&index))
            }
            GraphSubcommand::Inspect(args) => {
                let mut index = build_index_for(&cwd, args.config.as_ref())?;
                trim_diagnostics(&mut index, args.verbose);
                let target_path = resolve_target_path(&index, &args.target)?;
                let output = inspect_document(&index, &target_path)?;
                write_item_output(&output, args.format)?;
                Ok(exit_code_for(&index))
            }
        },
        Command::Validate(args) => {
            let loaded_config = load_config(&cwd, args.config.as_ref())?;
            let mut index = build_index_with_options(&cwd, &loaded_config.index_options)?;
            trim_diagnostics(&mut index, args.verbose);
            let findings = validate_findings(&index, &loaded_config.validate);
            if args.summary {
                let summary = validate_summary(&findings);
                write_item_output(&summary, args.format)?;
            } else {
                write_output(&findings, args.format)?;
            }
            Ok(exit_code_for(&index))
        }
    }
}

fn build_index_for(cwd: &Utf8PathBuf, config_path: Option<&Utf8PathBuf>) -> Result<GraphIndex> {
    let loaded_config = load_config(cwd, config_path)?;
    Ok(build_index_with_options(cwd, &loaded_config.index_options)?)
}

fn load_config(cwd: &Utf8PathBuf, config_path: Option<&Utf8PathBuf>) -> Result<LoadedConfig> {
    let resolved_config_path = config_path
        .map(|config_path| resolve_path(cwd, config_path))
        .or_else(|| {
            let discovered = cwd.join(".vault/config.yaml");
            discovered.exists().then_some(discovered)
        });

    let config = match resolved_config_path {
        Some(config_path) => {
            let config_text = fs::read_to_string(&config_path)
                .map_err(|error| anyhow::anyhow!("failed to read config {config_path}: {error}"))?;
            let config_value =
                serde_yaml::from_str::<serde_yaml::Value>(&config_text).map_err(|error| {
                    anyhow::anyhow!("failed to parse config {config_path}: {error}")
                })?;
            validate_config_value(&config_path, &config_value)?;
            serde_yaml::from_value::<VaultConfig>(config_value)
                .map_err(|error| anyhow::anyhow!("failed to parse config {config_path}: {error}"))?
        }
        None => VaultConfig::default(),
    };

    Ok(LoadedConfig {
        index_options: IndexOptions {
            ignore: config.graph.ignore,
        },
        validate: config.validate,
    })
}

fn effective_cwd(cwd: &Utf8PathBuf) -> Result<Utf8PathBuf> {
    if cwd.is_absolute() {
        return Ok(cwd.clone());
    }

    let current_dir = std::env::current_dir()
        .map_err(|error| anyhow::anyhow!("failed to read current directory: {error}"))?;
    let current_dir = Utf8PathBuf::from_path_buf(current_dir).map_err(|path| {
        anyhow::anyhow!("current directory is not valid UTF-8: {}", path.display())
    })?;
    Ok(current_dir.join(cwd))
}

fn resolve_path(cwd: &Utf8PathBuf, path: &Utf8PathBuf) -> Utf8PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        cwd.join(path)
    }
}

fn validate_config_value(config_path: &Utf8PathBuf, value: &serde_yaml::Value) -> Result<()> {
    let Some(root) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: root must be a mapping");
    };

    if let Some(graph) = mapping_get(root, "graph") {
        let Some(graph) = graph.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: graph must be a mapping");
        };

        if let Some(ignore) = mapping_get(graph, "ignore") {
            validate_string_sequence(config_path, "graph.ignore", ignore)?;
        }
    }

    if let Some(validate) = mapping_get(root, "validate") {
        let Some(validate) = validate.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: validate must be a mapping");
        };

        if let Some(required_frontmatter) = mapping_get(validate, "required_frontmatter") {
            validate_string_sequence(
                config_path,
                "validate.required_frontmatter",
                required_frontmatter,
            )?;
        }

        if let Some(rules) = mapping_get(validate, "rules") {
            let Some(rules) = rules.as_sequence() else {
                anyhow::bail!("invalid config {config_path}: validate.rules must be a sequence");
            };

            for (index, rule) in rules.iter().enumerate() {
                let rule_path = format!("validate.rules[{index}]");
                validate_rule_value(config_path, &rule_path, rule)?;
            }
        }
    }

    Ok(())
}

fn validate_rule_value(
    config_path: &Utf8PathBuf,
    rule_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(rule) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {rule_path} must be a mapping");
    };

    if let Some(name) = mapping_get(rule, "name") {
        if name.as_str().is_none() {
            anyhow::bail!("invalid config {config_path}: {rule_path}.name must be a string");
        }
    }

    if let Some(rule_match) = mapping_get(rule, "match") {
        let Some(rule_match) = rule_match.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: {rule_path}.match must be a mapping");
        };

        validate_known_mapping_keys(
            config_path,
            &format!("{rule_path}.match"),
            rule_match,
            &["path", "frontmatter"],
        )?;

        if let Some(path) = mapping_get(rule_match, "path") {
            if path.as_str().is_none() {
                anyhow::bail!(
                    "invalid config {config_path}: {rule_path}.match.path must be a string"
                );
            }
        }

        if let Some(frontmatter) = mapping_get(rule_match, "frontmatter") {
            validate_frontmatter_predicates(
                config_path,
                &format!("{rule_path}.match.frontmatter"),
                frontmatter,
            )?;
        }
    }

    if let Some(required_frontmatter) = mapping_get(rule, "required_frontmatter") {
        validate_string_sequence(
            config_path,
            &format!("{rule_path}.required_frontmatter"),
            required_frontmatter,
        )?;
    }

    if let Some(allowed_values) = mapping_get(rule, "allowed_values") {
        validate_allowed_values(
            config_path,
            &format!("{rule_path}.allowed_values"),
            allowed_values,
        )?;
    }

    Ok(())
}

fn validate_known_mapping_keys(
    config_path: &Utf8PathBuf,
    field_path: &str,
    mapping: &serde_yaml::Mapping,
    known_keys: &[&str],
) -> Result<()> {
    for key in mapping.keys() {
        let Some(key) = key.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };

        if !known_keys.contains(&key) {
            anyhow::bail!("invalid config {config_path}: unknown key {field_path}.{key}");
        }
    }

    Ok(())
}

fn validate_frontmatter_predicates(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(predicates) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    for (field, expected) in predicates {
        let Some(field) = field.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };

        if !is_scalar_yaml_value(expected) {
            anyhow::bail!(
                "invalid config {config_path}: {field_path}.{field} must be a string, boolean, or number"
            );
        }
    }

    Ok(())
}

fn validate_allowed_values(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(fields) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    for (field, allowed_values) in fields {
        let Some(field) = field.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };

        let Some(values) = allowed_values.as_sequence() else {
            anyhow::bail!("invalid config {config_path}: {field_path}.{field} must be a sequence");
        };

        if values.is_empty() {
            anyhow::bail!("invalid config {config_path}: {field_path}.{field} must not be empty");
        }

        for (index, allowed_value) in values.iter().enumerate() {
            if !is_scalar_yaml_value(allowed_value) {
                anyhow::bail!(
                    "invalid config {config_path}: {field_path}.{field}[{index}] must be a string, boolean, or number"
                );
            }
        }
    }

    Ok(())
}

fn is_scalar_yaml_value(value: &serde_yaml::Value) -> bool {
    matches!(
        value,
        serde_yaml::Value::String(_) | serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_)
    )
}

fn validate_string_sequence(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(items) = value.as_sequence() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a sequence");
    };

    for (index, item) in items.iter().enumerate() {
        if item.as_str().is_none() {
            anyhow::bail!("invalid config {config_path}: {field_path}[{index}] must be a string");
        }
    }

    Ok(())
}

fn mapping_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(&serde_yaml::Value::String(key.to_string()))
}

#[cfg(test)]
mod config_validation_tests {
    use super::load_config;
    use camino::Utf8PathBuf;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rejects_malformed_validate_rule_match_path() {
        let config_path = write_temp_config(
            "validate:\n  rules:\n    - name: bad\n      match:\n        path: 123\n      required_frontmatter:\n        - type\n",
        );

        let cwd =
            Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp path should be utf8");
        let message = match load_config(&cwd, Some(&config_path)) {
            Ok(_) => panic!("config should fail validation"),
            Err(error) => error.to_string(),
        };

        assert!(message.contains("invalid config"));
        assert!(message.contains("validate.rules[0].match.path must be a string"));
    }

    #[test]
    fn rejects_malformed_scoped_required_frontmatter() {
        let config_path = write_temp_config(
            "validate:\n  rules:\n    - name: bad\n      match:\n        path: Workspaces/**/*.md\n      required_frontmatter:\n        - 123\n",
        );

        let cwd =
            Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp path should be utf8");
        let message = match load_config(&cwd, Some(&config_path)) {
            Ok(_) => panic!("config should fail validation"),
            Err(error) => error.to_string(),
        };

        assert!(message.contains("invalid config"));
        assert!(message.contains("validate.rules[0].required_frontmatter[0] must be a string"));
    }

    fn write_temp_config(contents: &str) -> Utf8PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        path.push(format!("vault-cli-config-validation-{nanos}.yaml"));
        fs::write(&path, contents).expect("temp config should be written");
        Utf8PathBuf::from_path_buf(path).expect("temp path should be utf8")
    }
}

fn trim_diagnostics(index: &mut GraphIndex, verbose: bool) {
    if verbose {
        return;
    }

    for document in &mut index.documents {
        document.diagnostics = concise_diagnostics(document);
    }
}

fn all_links(index: &GraphIndex) -> Vec<&Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .collect()
}

fn all_files(index: &GraphIndex) -> Vec<&VaultFile> {
    index.files.iter().collect()
}

fn unresolved_links(index: &GraphIndex) -> Vec<&Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.status != LinkStatus::Resolved)
        .collect()
}

fn all_diagnostics(index: &GraphIndex) -> Vec<DocumentDiagnostic> {
    index
        .documents
        .iter()
        .flat_map(|document| {
            document
                .diagnostics
                .iter()
                .cloned()
                .map(|diagnostic| DocumentDiagnostic {
                    path: document.path.clone(),
                    diagnostic,
                })
        })
        .collect()
}

fn validate_findings(index: &GraphIndex, config: &ValidateConfig) -> Vec<ValidateFinding> {
    let mut findings = Vec::new();

    for document in &index.documents {
        for diagnostic in &document.diagnostics {
            findings.push(ValidateFinding {
                code: diagnostic.code.clone(),
                severity: diagnostic.severity.clone(),
                path: document.path.clone(),
                message: diagnostic.message.clone(),
                field: None,
                rule: None,
                actual_value: None,
                allowed_values: None,
                link: None,
                diagnostic: Some(diagnostic.clone()),
            });
        }

        for field in &config.required_frontmatter {
            if !document_has_frontmatter_field(document, field) {
                findings.push(ValidateFinding {
                    code: "frontmatter-required-field-missing".to_string(),
                    severity: Severity::Warning,
                    path: document.path.clone(),
                    message: format!("required frontmatter field is missing: {field}"),
                    field: Some(field.clone()),
                    rule: None,
                    actual_value: None,
                    allowed_values: None,
                    link: None,
                    diagnostic: None,
                });
            }
        }

        for rule in matching_validate_rules(document, &config.rules) {
            let rule_name = rule.name.clone();
            for field in &rule.required_frontmatter {
                if !document_has_frontmatter_field(document, field) {
                    findings.push(ValidateFinding {
                        code: "frontmatter-required-field-missing".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("required frontmatter field is missing: {field}"),
                        field: Some(field.clone()),
                        rule: rule_name.clone(),
                        actual_value: None,
                        allowed_values: None,
                        link: None,
                        diagnostic: None,
                    });
                }
            }

            for (field, allowed_values) in &rule.allowed_values {
                if let Some(actual_value) = document_frontmatter_field(document, field) {
                    if !allowed_values
                        .iter()
                        .any(|allowed_value| frontmatter_value_matches(actual_value, allowed_value))
                    {
                        findings.push(ValidateFinding {
                            code: "frontmatter-field-value-not-allowed".to_string(),
                            severity: Severity::Warning,
                            path: document.path.clone(),
                            message: format!("frontmatter field has a disallowed value: {field}"),
                            field: Some(field.clone()),
                            rule: rule_name.clone(),
                            actual_value: Some(actual_value.clone()),
                            allowed_values: Some(allowed_values.clone()),
                            link: None,
                            diagnostic: None,
                        });
                    }
                }
            }
        }

        for link in &document.links {
            match link.status {
                LinkStatus::Resolved => {}
                LinkStatus::Unresolved => {
                    findings.push(ValidateFinding {
                        code: "link-unresolved".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("unresolved link target: {}", link.target),
                        field: None,
                        rule: None,
                        actual_value: None,
                        allowed_values: None,
                        link: Some(link.clone()),
                        diagnostic: None,
                    });
                }
                LinkStatus::Ambiguous => {
                    findings.push(ValidateFinding {
                        code: "link-ambiguous".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("ambiguous link target: {}", link.target),
                        field: None,
                        rule: None,
                        actual_value: None,
                        allowed_values: None,
                        link: Some(link.clone()),
                        diagnostic: None,
                    });
                }
            }
        }
    }

    findings
}

fn validate_summary(findings: &[ValidateFinding]) -> ValidateSummary {
    let mut summary = ValidateSummary {
        findings: findings.len(),
        codes: BTreeMap::new(),
        severities: BTreeMap::new(),
        rules: BTreeMap::new(),
        fields: BTreeMap::new(),
        disallowed_values: BTreeMap::new(),
        path_prefixes: BTreeMap::new(),
    };

    for finding in findings {
        increment(&mut summary.codes, &finding.code);
        increment(&mut summary.severities, severity_key(&finding.severity));
        if let Some(rule) = &finding.rule {
            increment(&mut summary.rules, rule);
        }
        if let Some(field) = &finding.field {
            increment(&mut summary.fields, field);
        }
        if finding.code == "frontmatter-field-value-not-allowed" {
            if let (Some(field), Some(actual_value)) = (&finding.field, &finding.actual_value) {
                let value_counts = summary.disallowed_values.entry(field.clone()).or_default();
                increment(value_counts, summary_value_key(actual_value));
            }
        }
        increment(&mut summary.path_prefixes, &path_prefix_key(&finding.path));
    }

    summary
}

fn increment(counts: &mut BTreeMap<String, usize>, key: impl AsRef<str>) {
    *counts.entry(key.as_ref().to_string()).or_insert(0) += 1;
}

fn severity_key(severity: &Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

fn path_prefix_key(path: &Utf8PathBuf) -> String {
    let path = path.as_str();
    match path.split_once('/') {
        Some((prefix, _)) if !prefix.is_empty() => prefix.to_string(),
        _ => "root".to_string(),
    }
}

fn summary_value_key(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

fn matching_validate_rules<'a>(
    document: &Document,
    rules: &'a [ValidateRuleConfig],
) -> Vec<&'a ValidateRuleConfig> {
    rules
        .iter()
        .filter(|rule| validate_rule_matches(document, rule))
        .collect()
}

fn validate_rule_matches(document: &Document, rule: &ValidateRuleConfig) -> bool {
    if let Some(path_pattern) = &rule.r#match.path {
        if !pattern_matches_path(path_pattern, &document.path) {
            return false;
        }
    }

    frontmatter_predicates_match(document, &rule.r#match.frontmatter)
}

fn frontmatter_predicates_match(
    document: &Document,
    predicates: &std::collections::HashMap<String, serde_json::Value>,
) -> bool {
    if predicates.is_empty() {
        return true;
    }

    let Some(frontmatter) = document.frontmatter.as_ref() else {
        return false;
    };

    predicates.iter().all(|(field, expected)| {
        frontmatter
            .get(field)
            .is_some_and(|actual| frontmatter_value_matches(actual, expected))
    })
}

fn frontmatter_value_matches(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual, expected) {
        (serde_json::Value::String(actual), serde_json::Value::String(expected)) => {
            actual == expected
        }
        (serde_json::Value::Bool(actual), serde_json::Value::Bool(expected)) => actual == expected,
        (serde_json::Value::Number(actual), serde_json::Value::Number(expected)) => {
            actual == expected
        }
        _ => false,
    }
}

fn document_has_frontmatter_field(document: &Document, field: &str) -> bool {
    document_frontmatter_field(document, field).is_some()
}

fn document_frontmatter_field<'a>(
    document: &'a Document,
    field: &str,
) -> Option<&'a serde_json::Value> {
    document
        .frontmatter
        .as_ref()
        .and_then(|frontmatter| frontmatter.get(field))
        .filter(|value| !value.is_null())
}

fn backlinks<'a>(index: &'a GraphIndex, target_path: &Utf8PathBuf) -> Vec<&'a Link> {
    index
        .documents
        .iter()
        .flat_map(|document| document.links.iter())
        .filter(|link| link.resolved_path.as_ref() == Some(target_path))
        .collect()
}

fn resolve_backlink_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    if let Some(file) = index.files.iter().find(|file| file.path == target) {
        return Ok(file.path.clone());
    }

    resolve_target_path(index, target)
}

fn resolve_target_path(index: &GraphIndex, target: &str) -> Result<Utf8PathBuf> {
    if let Some(document) = index
        .documents
        .iter()
        .find(|document| document.path == target)
    {
        return Ok(document.path.clone());
    }

    let matches = index
        .documents
        .iter()
        .filter(|document| document.stem.eq_ignore_ascii_case(target))
        .map(|document| document.path.clone())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("no document matched path or stem: {target}"),
        many => bail!(
            "ambiguous document stem: {target}; candidates: {}",
            many.iter()
                .map(|path| path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn inspect_document(index: &GraphIndex, target_path: &Utf8PathBuf) -> Result<InspectOutput> {
    let document = index
        .documents
        .iter()
        .find(|document| &document.path == target_path)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("document not found after resolution: {target_path}"))?;

    let incoming_links = backlinks(index, target_path)
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let outgoing_links = document.links.clone();
    let unresolved_outgoing_links = document
        .links
        .iter()
        .filter(|link| link.status != LinkStatus::Resolved)
        .cloned()
        .collect::<Vec<_>>();

    Ok(InspectOutput {
        document,
        incoming_links,
        outgoing_links,
        unresolved_outgoing_links,
    })
}

fn filter_documents<'a>(index: &'a GraphIndex, filters: &[String]) -> Result<Vec<&'a Document>> {
    let parsed_filters = filters
        .iter()
        .map(|filter| parse_filter(filter))
        .collect::<Result<Vec<_>>>()?;

    Ok(index
        .documents
        .iter()
        .filter(|document| {
            parsed_filters
                .iter()
                .all(|(field, expected)| frontmatter_matches(document, field, expected))
        })
        .collect())
}

fn parse_filter(filter: &str) -> Result<(String, String)> {
    let Some((field, value)) = filter.split_once(':') else {
        bail!("invalid filter, expected field:value: {filter}");
    };

    let field = field.trim();
    let value = value.trim();
    if field.is_empty() || value.is_empty() {
        bail!("invalid filter, expected non-empty field and value: {filter}");
    }

    Ok((field.to_string(), value.to_string()))
}

fn frontmatter_matches(document: &Document, field: &str, expected: &str) -> bool {
    let Some(frontmatter) = &document.frontmatter else {
        return false;
    };
    let Some(value) = frontmatter.get(field) else {
        return false;
    };

    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| scalar_value_matches(value, expected)),
        other => scalar_value_matches(other, expected),
    }
}

fn scalar_value_matches(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(actual) => actual == expected,
        serde_json::Value::Bool(actual) => actual.to_string() == expected,
        serde_json::Value::Number(actual) => actual.to_string() == expected,
        _ => false,
    }
}

fn write_output<T: Serialize>(items: &[T], format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            write_json_line(&mut stdout, &serde_json::to_string_pretty(items)?)?;
        }
        OutputFormat::Jsonl => {
            for item in items {
                write_json_line(&mut stdout, &serde_json::to_string(item)?)?;
            }
        }
    }
    Ok(())
}

fn write_item_output<T: Serialize>(item: &T, format: OutputFormat) -> Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    match format {
        OutputFormat::Json => {
            write_json_line(&mut stdout, &serde_json::to_string_pretty(item)?)?;
        }
        OutputFormat::Jsonl => {
            write_json_line(&mut stdout, &serde_json::to_string(item)?)?;
        }
    }
    Ok(())
}

fn write_json_line(stdout: &mut impl Write, json: &str) -> Result<()> {
    stdout.write_all(json.as_bytes())?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
    })
}

fn exit_code_for(index: &GraphIndex) -> i32 {
    if has_errors(index) {
        1
    } else {
        0
    }
}
