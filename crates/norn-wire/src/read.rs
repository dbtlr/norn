//! The `find` / `count` request (`Params`) and response (`Report`) vocabulary.
//!
//! Pure serde types — the typed successor to the donor's `find`/`count` route
//! envelopes. The owner executes a request against its warm cache and answers
//! with the matching `Report`; the CLI renders that `Report` into the
//! user-facing formats. No logic, no IO, no dependency on any other norn crate.
//!
//! Two request shapes, two response shapes:
//!
//! - [`FindParams`] → [`FindReport`]: the filtered/sorted/paged document set,
//!   carried as flat [`FindDoc`] projections (path, stem, hash, frontmatter,
//!   body). Column selection and output formatting are the CLI's job — the
//!   report carries the whole matched row so every `--col`/`--format` choice is
//!   a pure presentation transform over it.
//! - [`CountParams`] → [`CountReport`]: a total, a single-field distribution, or
//!   a nested multi-field group tree. [`CountReport`] is `#[serde(untagged)]` so
//!   its serialization IS the `count --format json` contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::{FilterParams, SortPaginateParams};

/// A `find` request: the shared filter + sort/paging vocabulary. The find-only
/// `--limit` default (10) and the `--all` help-gate are applied CLI-side and
/// verb-side respectively, never encoded here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FindParams {
    #[serde(skip_serializing_if = "is_default_filter")]
    pub filter: FilterParams,
    #[serde(skip_serializing_if = "is_default_paging")]
    pub paging: SortPaginateParams,
    /// Load each match's deep connection facets (headings + the three link
    /// sets) alongside the flat projection. Off by default — a plain `find`
    /// never pays the per-match connection load; the CLI turns this on only
    /// when a deep `--col` facet (`.headings` / `.outgoing_links` /
    /// `.unresolved_links` / `.incoming_links`) or `--all-cols` is requested,
    /// so the empty-vs-loaded distinction is the CLI's: it renders a deep facet
    /// only when it asked for one (and so it was loaded), never a misleading
    /// empty array for an unrequested facet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub with_connections: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default_filter(f: &FilterParams) -> bool {
    *f == FilterParams::default()
}

fn is_default_paging(p: &SortPaginateParams) -> bool {
    *p == SortPaginateParams::default()
}

/// A `find` response: the flat document projections plus the paging envelope
/// (`total` before limit/offset, `returned` after, `starts_at`, `truncated`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindReport {
    pub documents: Vec<FindDoc>,
    pub total: usize,
    pub returned: usize,
    pub starts_at: usize,
    pub truncated: bool,
}

/// One matched document. Frontmatter and body are carried as-parsed so the CLI
/// can project any `--col` field or emit the whole block. The deep connection
/// facets (headings + the three link sets) are carried as pre-serialized JSON
/// values (`serde_json::to_value` of the cache's `Heading` / `Link` /
/// `IncomingLink`), so the CLI's `--format json` emission is byte-identical to
/// the cache's own serialization; they are populated only when the request set
/// [`FindParams::with_connections`], and are empty otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindDoc {
    pub path: String,
    pub stem: String,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body_text: String,
    /// Serialized `Heading` values (`{ level, slug, text, source_span }`),
    /// document order. Empty unless connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<Value>,
    /// Serialized resolved `Link` values. Empty unless connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outgoing_links: Vec<Value>,
    /// Serialized unresolved `Link` values. Empty unless connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_links: Vec<Value>,
    /// Serialized `IncomingLink` values (`{ source_path, link }`). Empty unless
    /// connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incoming_links: Vec<Value>,
}

/// A `count` request: the `--by` grouping fields plus the shared filter surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CountParams {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<String>,
    #[serde(skip_serializing_if = "is_default_filter")]
    pub filter: FilterParams,
}

/// A `count` response. `#[serde(untagged)]` so the serialized form is exactly
/// the `count --format json` output the oracle produces:
///
/// - no `--by` → `{"total":N}`
/// - one `--by` field → `{"by":"status","total":N,"groups":{…}}`
/// - many `--by` fields → `{"by":["type","status"],"total":N,"groups":{…nested…}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CountReport {
    Grouped {
        by: String,
        total: usize,
        groups: BTreeMap<String, usize>,
    },
    GroupedMulti {
        by: Vec<String>,
        total: usize,
        groups: BTreeMap<String, GroupNode>,
    },
    Total {
        total: usize,
    },
}

/// One node in a multi-field count group tree: a terminal count, or a nested
/// map one grouping level deeper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupNode {
    Leaf(usize),
    Branch(BTreeMap<String, GroupNode>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_find_params_serialize_empty() {
        assert_eq!(
            serde_json::to_value(FindParams::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn count_total_serializes_as_bare_total() {
        let r = CountReport::Total { total: 7 };
        assert_eq!(serde_json::to_value(&r).unwrap(), json!({ "total": 7 }));
    }

    #[test]
    fn count_grouped_serializes_with_by_and_groups() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 3usize);
        groups.insert("done".to_string(), 1usize);
        let r = CountReport::Grouped {
            by: "status".to_string(),
            total: 4,
            groups,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "by": "status", "total": 4, "groups": { "active": 3, "done": 1 } })
        );
    }

    #[test]
    fn count_grouped_multi_nests() {
        let mut inner = BTreeMap::new();
        inner.insert("active".to_string(), GroupNode::Leaf(2));
        let mut groups = BTreeMap::new();
        groups.insert("task".to_string(), GroupNode::Branch(inner));
        let r = CountReport::GroupedMulti {
            by: vec!["type".to_string(), "status".to_string()],
            total: 2,
            groups,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "by": ["type", "status"],
                "total": 2,
                "groups": { "task": { "active": 2 } }
            })
        );
    }

    #[test]
    fn count_report_round_trips_through_json() {
        for r in [
            CountReport::Total { total: 0 },
            CountReport::Grouped {
                by: "k".into(),
                total: 1,
                groups: BTreeMap::from([("v".to_string(), 1usize)]),
            },
        ] {
            let v = serde_json::to_value(&r).unwrap();
            let back: CountReport = serde_json::from_value(v).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn find_report_round_trips() {
        let r = FindReport {
            documents: vec![FindDoc {
                path: "a.md".into(),
                stem: "a".into(),
                hash: "h".into(),
                frontmatter: Some(json!({"type": "note"})),
                body_text: "body".into(),
                headings: vec![],
                outgoing_links: vec![],
                unresolved_links: vec![],
                incoming_links: vec![],
            }],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: FindReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }
}
