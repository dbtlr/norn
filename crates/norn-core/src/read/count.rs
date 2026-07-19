//! The `count` verb's execute seam (the 0016 Params/execute/Report vocabulary).
//!
//! Ported from the donor `src/count/`: build the predicate query, resolve
//! `--links-to`, scan the full match set with
//! [`Cache::documents_matching`](crate::cache::Cache::documents_matching) (no
//! sort/limit/paging — count never pages), then reduce to a total, a
//! single-field distribution, or a nested multi-field group tree.
//!
//! The `Ok(Ok)` / `Ok(Err)` / `Err` contract is identical to
//! [`find::execute`](crate::read::find::execute) — see its module docs.

use std::collections::BTreeMap;

use anyhow::Result;

use norn_wire::{CountParams, CountReport, GroupNode};

use crate::cache::Cache;
use crate::domain::DocumentSummary;
use crate::query::filter_args::build_document_query;
use crate::read::{render_key, MISSING};

/// The most `--by` fields a single count may nest (donor parity).
const MAX_BY_FIELDS: usize = 16;

/// Run a `count` request against the warm cache.
pub fn execute(
    cache: &Cache,
    params: &CountParams,
    today: &str,
) -> Result<Result<CountReport, String>> {
    // Normalize `--by`: trim each field, reject empty / over-cap / duplicate.
    let mut by: Vec<String> = Vec::with_capacity(params.by.len());
    for raw in &params.by {
        let field = raw.trim().to_string();
        if field.is_empty() {
            return Ok(Err("--by field name cannot be empty".to_string()));
        }
        if by.contains(&field) {
            return Ok(Err(format!("--by field `{field}` is repeated")));
        }
        by.push(field);
    }
    if by.len() > MAX_BY_FIELDS {
        return Ok(Err(format!(
            "--by accepts at most {MAX_BY_FIELDS} fields (got {})",
            by.len()
        )));
    }

    let mut query = match build_document_query(&params.filter, today) {
        Ok(q) => q,
        Err(e) => return Ok(Err(e.to_string())),
    };

    if !params.filter.links_to.is_empty() {
        let index = cache.load_graph_index()?;
        for target in &params.filter.links_to {
            match crate::target::resolve_target_path(&index, target) {
                Ok(path) => query.links_to.push(path),
                Err(e) => return Ok(Err(e.to_string())),
            }
        }
    }

    let docs = cache.documents_matching(&query)?;
    let total = docs.len();

    let report = match by.as_slice() {
        [] => CountReport::Total { total },
        [field] => CountReport::Grouped {
            by: field.clone(),
            total,
            groups: group_by(&docs, field),
        },
        fields => CountReport::GroupedMulti {
            groups: group_by_multi(&docs, fields),
            by: fields.to_vec(),
            total,
        },
    };

    Ok(Ok(report))
}

/// Count documents bucketed by one field's value.
fn group_by(docs: &[DocumentSummary], field: &str) -> BTreeMap<String, usize> {
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for doc in docs {
        *groups.entry(doc_key(doc, field)).or_insert(0) += 1;
    }
    groups
}

/// Recursively nest one map level per field.
fn group_by_multi(docs: &[DocumentSummary], fields: &[String]) -> BTreeMap<String, GroupNode> {
    group_refs(&docs.iter().collect::<Vec<_>>(), fields)
}

fn group_refs(docs: &[&DocumentSummary], fields: &[String]) -> BTreeMap<String, GroupNode> {
    let (field, rest) = fields.split_first().expect("non-empty fields");
    let mut buckets: BTreeMap<String, Vec<&DocumentSummary>> = BTreeMap::new();
    for doc in docs {
        buckets.entry(doc_key(doc, field)).or_default().push(doc);
    }
    buckets
        .into_iter()
        .map(|(key, group)| {
            let node = if rest.is_empty() {
                GroupNode::Leaf(group.len())
            } else {
                GroupNode::Branch(group_refs(&group, rest))
            };
            (key, node)
        })
        .collect()
}

/// The group-key string for a document's value of `field`.
fn doc_key(doc: &DocumentSummary, field: &str) -> String {
    doc.frontmatter
        .as_ref()
        .and_then(|fm| fm.get(field))
        .map(render_key)
        .unwrap_or_else(|| MISSING.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use norn_wire::FilterParams;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-18";

    fn vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        let docs: &[(&str, &str)] = &[
            ("a", "---\ntype: note\nstatus: active\n---\n"),
            ("b", "---\ntype: task\nstatus: active\n---\n"),
            ("c", "---\ntype: task\nstatus: done\n---\n"),
            ("d", "---\ntype: note\n---\n"),
        ];
        for (name, body) in docs {
            std::fs::write(root.join(format!("{name}.md")).as_std_path(), body).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    #[test]
    fn bare_count_returns_total() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let report = execute(&cache, &CountParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report, CountReport::Total { total: 4 });
    }

    #[test]
    fn count_by_type_distributes() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = CountParams {
            by: vec!["type".into()],
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        match report {
            CountReport::Grouped { by, total, groups } => {
                assert_eq!(by, "type");
                assert_eq!(total, 4);
                assert_eq!(groups["note"], 2);
                assert_eq!(groups["task"], 2);
            }
            other => panic!("expected Grouped, got {other:?}"),
        }
    }

    #[test]
    fn count_by_missing_field_buckets_as_missing() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = CountParams {
            by: vec!["status".into()],
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        match report {
            CountReport::Grouped { groups, .. } => {
                assert_eq!(groups["active"], 2);
                assert_eq!(groups["done"], 1);
                assert_eq!(groups[MISSING], 1);
            }
            other => panic!("expected Grouped, got {other:?}"),
        }
    }

    #[test]
    fn count_by_two_fields_nests() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = CountParams {
            by: vec!["type".into(), "status".into()],
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        match report {
            CountReport::GroupedMulti { by, total, groups } => {
                assert_eq!(by, vec!["type", "status"]);
                assert_eq!(total, 4);
                let task = &groups["task"];
                match task {
                    GroupNode::Branch(inner) => {
                        assert_eq!(inner["active"], GroupNode::Leaf(1));
                        assert_eq!(inner["done"], GroupNode::Leaf(1));
                    }
                    other => panic!("expected Branch, got {other:?}"),
                }
            }
            other => panic!("expected GroupedMulti, got {other:?}"),
        }
    }

    #[test]
    fn count_with_filter_narrows_before_grouping() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = CountParams {
            by: vec!["status".into()],
            filter: FilterParams {
                eq: vec!["type:task".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        match report {
            CountReport::Grouped { total, groups, .. } => {
                assert_eq!(total, 2);
                assert_eq!(groups["active"], 1);
                assert_eq!(groups["done"], 1);
            }
            other => panic!("expected Grouped, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_by_field_is_a_user_error() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = CountParams {
            by: vec!["type".into(), "type".into()],
            ..Default::default()
        };
        assert!(execute(&cache, &params, TODAY).unwrap().is_err());
    }
}
