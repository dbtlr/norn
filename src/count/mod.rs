//! `vault count` — grouped or total document counts. Shares the full
//! filter flag surface with `vault find` via `FilterArgs`; adds `--by`
//! for grouping.

pub mod render;

use crate::cache::Cache;
use crate::core::DocumentSummary;
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

use crate::cli::CountArgs;
use crate::filter_args::build_document_query;

#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum CountOutput {
    Total {
        total: usize,
    },
    Grouped {
        by: String,
        total: usize,
        groups: BTreeMap<String, usize>,
    },
}

pub fn run(cache: &Cache, args: &CountArgs) -> Result<CountOutput> {
    let query = build_document_query(&args.filters)?;
    let docs = cache.documents_matching(&query)?;
    let total = docs.len();

    match &args.by {
        None => Ok(CountOutput::Total { total }),
        Some(field) => Ok(CountOutput::Grouped {
            by: field.clone(),
            total,
            groups: group_by(&docs, field),
        }),
    }
}

fn group_by(docs: &[DocumentSummary], field: &str) -> BTreeMap<String, usize> {
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for doc in docs {
        let key = doc
            .frontmatter
            .as_ref()
            .and_then(|fm| fm.get(field))
            .map(render_key)
            .unwrap_or_else(|| "(missing)".to_string());
        *groups.entry(key).or_insert(0) += 1;
    }
    groups
}

fn render_key(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "(null)".to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn synth() -> (TempDir, Utf8PathBuf) {
        // Use a non-hidden prefix; the vault walker prunes ".tmp" paths.
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-count-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        (tmp, root)
    }

    fn write(root: &Utf8PathBuf, name: &str, frontmatter: &str) {
        let body = format!("---\n{}\n---\nbody\n", frontmatter);
        std::fs::write(root.join(name).as_std_path(), body).unwrap();
    }

    #[test]
    fn total_only_when_no_by() {
        let (_tmp, root) = synth();
        write(&root, "a.md", "type: note");
        write(&root, "b.md", "type: note");
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let args = crate::cli::CountArgs {
            by: None,
            filters: crate::filter_args::FilterArgs::default(),
            format: crate::cli::CountFormat::Text,
        };
        let out = run(&cache, &args).unwrap();
        assert_eq!(out, CountOutput::Total { total: 2 });
    }

    #[test]
    fn groups_by_frontmatter_field() {
        let (_tmp, root) = synth();
        write(&root, "a.md", "type: note\nstatus: active");
        write(&root, "b.md", "type: note\nstatus: backlog");
        write(&root, "c.md", "type: note\nstatus: backlog");
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let args = crate::cli::CountArgs {
            by: Some("status".to_string()),
            filters: crate::filter_args::FilterArgs::default(),
            format: crate::cli::CountFormat::Text,
        };
        let out = run(&cache, &args).unwrap();
        let expected: BTreeMap<String, usize> =
            [("active".to_string(), 1), ("backlog".to_string(), 2)]
                .into_iter()
                .collect();
        assert_eq!(
            out,
            CountOutput::Grouped {
                by: "status".to_string(),
                total: 3,
                groups: expected,
            }
        );
    }

    #[test]
    fn missing_field_groups_as_missing_marker() {
        let (_tmp, root) = synth();
        write(&root, "a.md", "type: note\nstatus: active");
        write(&root, "b.md", "type: note");
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let args = crate::cli::CountArgs {
            by: Some("status".to_string()),
            filters: crate::filter_args::FilterArgs::default(),
            format: crate::cli::CountFormat::Text,
        };
        let out = run(&cache, &args).unwrap();
        match out {
            CountOutput::Grouped { groups, .. } => {
                assert_eq!(groups.get("active"), Some(&1));
                assert_eq!(groups.get("(missing)"), Some(&1));
            }
            _ => panic!("expected Grouped"),
        }
    }
}
