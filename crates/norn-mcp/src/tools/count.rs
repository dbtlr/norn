//! `vault.count` — total or grouped document counts.
//!
//! The param struct is the donor's flat filter-args mirror (its schemars schema
//! is the published `inputSchema`); the handler maps it to a norn-wire
//! [`CountParams`] and routes to the owner, then projects the untagged
//! [`CountReport`] into the flat [`CountEnvelope`] the donor advertised (rmcp
//! requires an `outputSchema` with a `type: object` root, which an untagged enum
//! cannot produce).

use norn_wire::{CountParams as WireCountParams, CountReport, FilterParams};
use serde::{Deserialize, Serialize};

/// Parameters for `vault.count` — mirrors `norn count`'s agent-useful flags: the
/// full find-filter surface plus `by` for grouping. `--format` is omitted (the
/// MCP tool always returns the structured envelope).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct CountParams {
    /// Frontmatter field(s) to group counts by — comma-separated, exactly the
    /// CLI's `--by` token (e.g. `"project,lifecycle"`). Without `by`, only
    /// the total is returned. One field returns a string `by` and a flat
    /// value→count `groups` map; several fields return an array `by` and
    /// nested `groups` (one map level per field, counts at the leaves).
    #[serde(default)]
    pub by: Option<String>,

    // ── Filter predicates (mirrors FilterArgs) ──────────────────────────────
    /// Full-text body substring. Case-insensitive.
    #[serde(default)]
    pub text: Option<String>,

    /// Frontmatter equality predicates `field:value`. Repeatable; all must match.
    #[serde(default)]
    pub eq: Vec<String>,

    /// Frontmatter inequality predicates `field:value`. Repeatable.
    #[serde(default)]
    pub not_eq: Vec<String>,

    /// Frontmatter ANY-of predicates `field:V1,V2,...`. Repeatable.
    #[serde(default)]
    #[serde(rename = "in")]
    pub r#in: Vec<String>,

    /// Frontmatter NOT-in predicates `field:V1,V2,...`. Repeatable.
    #[serde(default)]
    pub not_in: Vec<String>,

    /// Frontmatter prefix predicates `field:VALUE` — the field (or any array
    /// element) starts with VALUE. Case-sensitive. Repeatable; all must match.
    #[serde(default)]
    pub starts_with: Vec<String>,

    /// Frontmatter suffix predicates `field:VALUE` — the field (or any array
    /// element) ends with VALUE. Case-sensitive. Repeatable.
    #[serde(default)]
    pub ends_with: Vec<String>,

    /// Frontmatter substring predicates `field:VALUE` — the field (or any
    /// array element) contains VALUE. Case-sensitive. Repeatable.
    #[serde(default)]
    pub contains: Vec<String>,

    /// Frontmatter fields that must be present (non-null). Repeatable.
    #[serde(default)]
    pub has: Vec<String>,

    /// Frontmatter fields that must be absent or null. Repeatable.
    #[serde(default)]
    pub missing: Vec<String>,

    /// Date-before predicates `field:DATE`. ISO 8601. Repeatable.
    #[serde(default)]
    pub before: Vec<String>,

    /// Date-after predicates `field:DATE`. ISO 8601. Repeatable.
    #[serde(default)]
    pub after: Vec<String>,

    /// Date-on predicates `field:DATE`. Accepts `today`. Repeatable.
    #[serde(default)]
    pub on: Vec<String>,

    /// Path glob patterns. Repeatable.
    #[serde(default)]
    pub path: Vec<String>,

    /// Documents whose outgoing links resolve to TARGET. Repeatable; AND'd.
    #[serde(default)]
    pub links_to: Vec<String>,

    /// Include only documents with at least one unresolved link.
    #[serde(default)]
    pub unresolved_links: bool,
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

/// Map the flat MCP filter fields onto the shared wire [`FilterParams`].
pub(crate) fn to_filter(p: CountParams) -> (Option<String>, FilterParams) {
    let filter = FilterParams {
        text: p.text,
        eq: p.eq,
        not_eq: p.not_eq,
        r#in: p.r#in,
        not_in: p.not_in,
        starts_with: p.starts_with,
        ends_with: p.ends_with,
        contains: p.contains,
        has: p.has,
        missing: p.missing,
        before: p.before,
        after: p.after,
        on: p.on,
        path: p.path,
        links_to: p.links_to,
        unresolved_links: p.unresolved_links,
    };
    (p.by, filter)
}

/// Build the wire request. The single `by` comma token is split into fields the
/// same way the CLI's `--by` (a `value_delimiter = ','` list) does.
pub(crate) fn to_wire(p: CountParams) -> WireCountParams {
    let (by, filter) = to_filter(p);
    WireCountParams {
        by: by
            .as_deref()
            .map(|token| token.split(',').map(str::to_string).collect())
            .unwrap_or_default(),
        filter,
        dynamic_keys: Vec::new(),
    }
}

/// Project the untagged wire report into the flat output envelope.
pub(crate) fn envelope(report: CountReport) -> CountEnvelope {
    CountEnvelope::from_report(report)
}
