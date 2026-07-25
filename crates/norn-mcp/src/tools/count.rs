//! `vault.count` — total or grouped document counts.
//!
//! The param struct is a flat filter-args mirror (its schemars schema
//! is the published `inputSchema`); the handler maps it to a norn-wire
//! [`CountParams`] and routes to the owner, then projects the untagged
//! [`CountReport`] into the flat [`CountEnvelope`] (rmcp
//! requires an `outputSchema` with a `type: object` root, which an untagged enum
//! cannot produce).

use norn_wire::{CountParams as WireCountParams, CountReport};
use serde::{Deserialize, Serialize};

use crate::tools::filters::filter_params;

filter_params! {
    /// Parameters for `vault.count` — mirrors `norn count`'s agent-useful flags: the
    /// full find-filter surface plus `by` for grouping. `--format` is omitted (the
    /// MCP tool always returns the structured envelope).
    #[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
    #[serde(deny_unknown_fields)]
    pub struct CountParams {
        /// Frontmatter field(s) to group counts by — comma-separated, exactly the
        /// CLI's `--by` token (e.g. `"project,lifecycle"`). Without `by`, only
        /// the total is returned. One field returns a string `by` and a flat
        /// value→count `groups` map; several fields return an array `by` and
        /// nested `groups` (one map level per field, counts at the leaves).
        #[serde(default)]
        pub by: Option<String>,
    }
}

/// Flat output envelope for `vault.count` — covers every [`CountReport`] variant
/// in a single `type: object` root so rmcp's schema validation passes: `total`
/// always present; `by` and `groups` set only when a `--by` field was requested.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CountEnvelope {
    /// Total number of matching documents.
    pub total: usize,
    /// Grouping key(s): a string for single-key grouping, an array of
    /// strings for multi-key (set when `by` was requested).
    #[schemars(schema_with = "by_schema")]
    pub by: Option<serde_json::Value>,
    /// Per-value document counts, sorted by field value: flat for one key,
    /// nested for several (set when `by` was requested).
    #[schemars(schema_with = "groups_schema")]
    pub groups: Option<serde_json::Value>,
}

/// Typed schema for `by`: string (one key) | string array (several) | null.
fn by_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            { "type": "string" },
            { "type": "array", "items": { "type": "string" } },
            { "type": "null" }
        ]
    })
}

/// Typed schema for `groups`: a map whose values are counts (one key) or
/// nested maps bottoming out in counts (several), or null when no grouping
/// was requested.
fn groups_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [
            {
                "type": "object",
                "additionalProperties": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "object" }
                    ]
                }
            },
            { "type": "null" }
        ]
    })
}

impl CountEnvelope {
    fn from_report(report: CountReport) -> Self {
        match report {
            CountReport::Total { total } => Self {
                total,
                by: None,
                groups: None,
            },
            CountReport::Grouped { by, total, groups } => Self {
                total,
                by: Some(serde_json::Value::String(by)),
                groups: Some(serde_json::to_value(groups).expect("count groups serialize")),
            },
            CountReport::GroupedMulti { by, total, groups } => Self {
                total,
                by: Some(serde_json::to_value(by).expect("count by serialize")),
                groups: Some(serde_json::to_value(groups).expect("count groups serialize")),
            },
        }
    }
}

/// Build the wire request. The single `by` comma token is split into fields the
/// same way the CLI's `--by` (a `value_delimiter = ','` list) does.
pub(crate) fn to_wire(p: CountParams) -> WireCountParams {
    WireCountParams {
        by: p
            .by
            .as_deref()
            .map(|token| token.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        filter: p.to_filter(),
        dynamic_keys: Vec::new(),
    }
}

/// Project the untagged wire report into the flat output envelope.
pub(crate) fn envelope(report: CountReport) -> CountEnvelope {
    CountEnvelope::from_report(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn total_projects_to_flat_envelope() {
        let env = envelope(CountReport::Total { total: 7 });
        assert_eq!(
            serde_json::to_value(&env).unwrap(),
            json!({"total":7,"by":null,"groups":null})
        );
    }

    #[test]
    fn single_by_projects_string_by_and_flat_groups() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 3usize);
        let env = envelope(CountReport::Grouped {
            by: "status".into(),
            total: 3,
            groups,
        });
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["by"], json!("status"));
        assert_eq!(v["groups"], json!({"active":3}));
        assert_eq!(v["total"], json!(3));
    }

    #[test]
    fn by_comma_token_splits_like_the_cli_delimiter() {
        let wire = to_wire(CountParams {
            by: Some("type,status".into()),
            ..CountParams::default()
        });
        assert_eq!(wire.by, vec!["type".to_string(), "status".to_string()]);
    }

    #[test]
    fn filter_fields_map_onto_the_wire_filter() {
        let wire = to_wire(CountParams {
            eq: vec!["type:note".into()],
            r#in: vec!["status:a,b".into()],
            unresolved_links: true,
            ..CountParams::default()
        });
        assert_eq!(wire.filter.eq, vec!["type:note".to_string()]);
        assert_eq!(wire.filter.r#in, vec!["status:a,b".to_string()]);
        assert!(wire.filter.unresolved_links);
    }
}
