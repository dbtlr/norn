//! Core `describe` — vault structure (placement aid) and, with `--data`, a
//! contents-summary. Shared by the CLI `norn describe` command and the
//! `vault.describe` MCP tool so the two cannot drift.

pub mod data;

use anyhow::Result;
use serde::Serialize;

use crate::cache::Cache;
use crate::cli::{FindArgs, SortPaginateArgs};
use crate::config_loader::LoadedConfig;
use crate::filter_args::FilterArgs;

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct PathRule {
    pub glob: String,
    pub name: Option<String>,
    pub frontmatter_defaults: serde_json::Value,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CreatableRule {
    pub name: String,
    pub target: String,
    pub required_vars: Vec<String>,
    pub frontmatter_defaults: serde_json::Value,
    pub body: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DescribeOutput {
    pub folders: Vec<String>,
    pub path_rules: Vec<PathRule>,
    pub creatable_rules: Vec<CreatableRule>,
    pub inbox: Option<String>,
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
