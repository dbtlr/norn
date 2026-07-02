//! `norn count` — grouped or total document counts. Shares the full
//! filter flag surface with `norn find` via `FilterArgs`; adds `--by`
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
    /// Single-key grouping. `by` stays a plain string and `groups` a flat
    /// value→count map — this exact JSON shape is load-bearing for external
    /// consumers; multi-key grouping is the separate `GroupedMulti` shape.
    Grouped {
        by: String,
        total: usize,
        groups: BTreeMap<String, usize>,
    },
    /// Multi-key grouping: `by` is the key list and `groups` nests one map
    /// level per key, counts at the leaves.
    GroupedMulti {
        by: Vec<String>,
        total: usize,
        groups: BTreeMap<String, GroupNode>,
    },
}

/// One level of a nested grouping tree: a leaf count, or a branch keyed by
/// the next `--by` field's rendered values.
#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum GroupNode {
    Leaf(usize),
    Branch(BTreeMap<String, GroupNode>),
}

pub fn run(cache: &Cache, args: &CountArgs) -> Result<CountOutput> {
    if args.by.iter().any(|field| field.trim().is_empty()) {
        anyhow::bail!("invalid --by value: empty field name");
    }
    let mut query = build_document_query(&args.filters)?;
    query.links_to = crate::filter_args::resolve_links_to(cache, &args.filters.links_to)?;
    let docs = cache.documents_matching(&query)?;
    let total = docs.len();

    match args.by.as_slice() {
        [] => Ok(CountOutput::Total { total }),
        [field] => Ok(CountOutput::Grouped {
            by: field.clone(),
            total,
            groups: group_by(&docs, field),
        }),
        fields => Ok(CountOutput::GroupedMulti {
            by: fields.to_vec(),
            total,
            groups: group_by_multi(&docs, fields),
        }),
    }
}

fn group_by(docs: &[DocumentSummary], field: &str) -> BTreeMap<String, usize> {
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for doc in docs {
        *groups.entry(doc_key(doc, field)).or_insert(0) += 1;
    }
    groups
}

/// Nest documents one map level per field, counting at the leaves. Documents
/// missing a field bucket under `(missing)` at that level, mirroring the
/// single-key behavior.
fn group_by_multi(docs: &[DocumentSummary], fields: &[String]) -> BTreeMap<String, GroupNode> {
    let refs: Vec<&DocumentSummary> = docs.iter().collect();
    group_refs(&refs, fields)
}

fn group_refs(docs: &[&DocumentSummary], fields: &[String]) -> BTreeMap<String, GroupNode> {
    let (first, rest) = fields
        .split_first()
        .expect("group_refs requires at least one field");
    let mut buckets: BTreeMap<String, Vec<&DocumentSummary>> = BTreeMap::new();
    for doc in docs {
        buckets.entry(doc_key(doc, first)).or_default().push(doc);
    }
    buckets
        .into_iter()
        .map(|(key, bucket)| {
            let node = if rest.is_empty() {
                GroupNode::Leaf(bucket.len())
            } else {
                GroupNode::Branch(group_refs(&bucket, rest))
            };
            (key, node)
        })
        .collect()
}

fn doc_key(doc: &DocumentSummary, field: &str) -> String {
    doc.frontmatter
        .as_ref()
        .and_then(|fm| fm.get(field))
        .map(render_key)
        .unwrap_or_else(|| "(missing)".to_string())
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
            .prefix("norn-count-")
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
            by: vec![],
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
            by: vec!["status".to_string()],
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
            by: vec!["status".to_string()],
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
