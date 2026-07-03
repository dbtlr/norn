//! Core `describe` — vault structure (placement aid) and, with `--data`, a
//! contents-summary. Shared by the CLI `norn describe` command and the
//! `vault.describe` MCP tool so the two cannot drift.

pub mod data;
pub mod render;

use anyhow::Result;
use serde::Serialize;

use crate::cache::Cache;
use crate::cli::{FindArgs, SortPaginateArgs};
use crate::config_loader::LoadedConfig;
use crate::describe::data::{summarize, DataOptions};
use crate::filter_args::{build_document_query, FilterArgs};

/// A single declared path rule: which glob gets which frontmatter defaults.
///
/// Surfaced from a `ValidateRule` that declares a `match.path`. The
/// `frontmatter_defaults` map is the rule's `frontmatter_defaults` verbatim
/// (e.g. `{"type": "note"}`) — the values `norn new` scaffolds onto a doc
/// created at a path matching `glob`. An agent reads `glob` to learn where a
/// kind of doc lives, and `frontmatter_defaults` to learn what it inherits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PathRule {
    /// The rule's `match.path` glob (e.g. `Workspaces/{{workspace}}/notes/*.md`).
    pub glob: String,
    /// Optional rule name, if the config declared one.
    pub name: Option<String>,
    /// The frontmatter defaults a doc at a matching path inherits, as declared.
    /// Empty object when the rule sets no defaults (it is still a placement
    /// signal — the glob tells the agent where that kind of doc lives).
    pub frontmatter_defaults: serde_json::Value,
}

/// A rule that can be used to create a new document via `vault.new { rule: "..." }`.
///
/// Only rules that declare BOTH a `name` AND a `target` template are creatable.
/// An agent can call `vault.new { rule: name, title: "...", vars: {...} }` to
/// create a document at the path derived from `target`, without knowing the
/// concrete path in advance.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CreatableRule {
    /// The rule's declared name — pass this as `vault.new { rule: name }`.
    pub name: String,
    /// The path template (e.g. `Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md`).
    /// Shows the agent what the concrete path will look like once rendered.
    pub target: String,
    /// Variable names (from `{{var.X}}` / `{{path.X}}` tokens) the template requires.
    /// The agent must supply these via `vault.new { vars: { "workspace": "..." } }`.
    pub required_vars: Vec<String>,
    /// The frontmatter defaults a doc created with this rule inherits.
    pub frontmatter_defaults: serde_json::Value,
    /// Optional body scaffold template for the new document's body.
    /// Rendered with the same substitution context used for path generation.
    pub body: Option<String>,
}

/// Structured output for `vault.describe`. Root is `type: object`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DescribeOutput {
    /// Distinct vault-relative directories that currently hold documents,
    /// sorted. The vault root is represented as `""` (docs at top level). This
    /// is the folder tree the agent uses to see where each kind of doc lives.
    pub folders: Vec<String>,
    /// Declared path rules: for each config rule with a `match.path`, the glob
    /// plus the frontmatter defaults a doc at a matching path inherits.
    pub path_rules: Vec<PathRule>,
    /// Rules that can be used with `vault.new { rule: "name" }` to create a
    /// document at a derived path. Each entry carries the rule name, its target
    /// template, the required variable names, frontmatter defaults, and optional
    /// body scaffold. Separate from `path_rules` so the existing contract is
    /// undisturbed — an agent that already parses `path_rules` keeps working.
    pub creatable_rules: Vec<CreatableRule>,
    /// Configured inbox path (from `inbox.path` in the vault config), if any.
    /// When present, `vault.new { title: "..." }` (no path, no rule) routes the
    /// new document to `<inbox>/<title|slugify>.md`.
    pub inbox: Option<String>,
    /// The full configured frontmatter schema/standards — the `validate` config
    /// serialized verbatim (required/forbidden fields, field types, allowed
    /// values, per-rule selectors). `serde_json::Value` so no standard is lost.
    pub schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<data::DataSummary>,
}

/// Structure-only describe (the pre-existing contract). `data` is `None`.
pub fn structure(cache: &Cache, config: &LoadedConfig) -> Result<DescribeOutput> {
    let folders = collect_folders(cache)?;

    let path_rules = config
        .validate
        .rules
        .iter()
        .filter_map(|rule| {
            rule.r#match.path.as_ref().map(|glob| PathRule {
                glob: glob.clone(),
                name: rule.name.clone(),
                frontmatter_defaults: serde_json::to_value(&rule.frontmatter_defaults)
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();

    let creatable_rules = config
        .validate
        .rules
        .iter()
        .filter_map(|rule| {
            let name = rule.name.as_ref()?;
            let target = rule.target.as_ref()?;
            let required_vars = crate::new::generate::referenced_vars(target);
            Some(CreatableRule {
                name: name.clone(),
                target: target.clone(),
                required_vars,
                frontmatter_defaults: serde_json::to_value(&rule.frontmatter_defaults)
                    .unwrap_or(serde_json::Value::Null),
                body: rule.body.clone(),
            })
        })
        .collect();

    let inbox = config.vault_config.inbox.path.clone();
    let schema = serde_json::to_value(&config.validate)?;

    Ok(DescribeOutput {
        folders,
        path_rules,
        creatable_rules,
        inbox,
        schema,
        data: None,
    })
}

/// Full describe: structure, plus a contents-summary when `data` is `Some`.
/// The summary honors the find-filter surface in `filters`.
pub fn describe(
    cache: &Cache,
    config: &LoadedConfig,
    filters: &FilterArgs,
    data: Option<DataOptions>,
) -> Result<DescribeOutput> {
    let mut out = structure(cache, config)?;
    if let Some(opts) = data {
        let mut query = build_document_query(filters)?;
        query.links_to = crate::filter_args::resolve_links_to(cache, &filters.links_to)?;
        let docs = cache.documents_matching(&query)?;
        out.data = Some(summarize(&docs, config, &opts));
    }
    Ok(out)
}

fn collect_folders(cache: &Cache) -> Result<Vec<String>> {
    let args = FindArgs {
        filters: FilterArgs::default(),
        all: true,
        paging: SortPaginateArgs {
            sort: None,
            desc: false,
            limit: None,
            no_limit: true,
            starts_at: 1,
        },
        format: None,
        all_cols: false,
        col: Vec::new(),
        no_pager: false,
    };
    let documents = crate::find::query::query(cache, &args, None)?;
    let mut folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for doc in &documents {
        let Some(path) = doc.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        let parent = match path.rfind('/') {
            Some(idx) => &path[..idx],
            None => "",
        };
        folders.insert(parent.to_string());
    }
    Ok(folders.into_iter().collect())
}
