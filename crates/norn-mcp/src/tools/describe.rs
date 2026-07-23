//! `vault.describe` — the vault's structure and configured schema.
//!
//! The param struct mirrors `norn describe`'s daily surface; the handler routes
//! to the owner and returns the wire [`DescribeReport`] FLAT as the tool's
//! `structuredContent` (the read-verb envelope shape — `folders` / `path_rules`
//! / `schema` at the top level, not wrapped under a `report` key), via the
//! [`FlatReport`] newtype that satisfies rmcp's `type: object` schema demand.

use norn_wire::{DescribeParams as WireDescribeParams, DescribeReport, FilterParams};
use serde::Deserialize;

use crate::mutation_result::FlatReport;

/// Parameters for `vault.describe` — the structure view always, plus a
/// contents-summary when `data` is set or a `by` grouping is given. The
/// find-filter surface narrows the summary population.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct DescribeParams {
    /// Include the contents summary (per-field value distributions, date bounds).
    /// Implied when `by` is non-empty.
    #[serde(default)]
    pub data: bool,
    /// Group the contents summary by frontmatter field(s) — comma-separated,
    /// exactly the CLI's `--by` token. Implies `data`.
    #[serde(default)]
    pub by: Option<String>,
    /// Cap the value buckets shown per field. Absent → the verb default (20);
    /// `0` → uncapped.
    #[serde(default)]
    pub limit: Option<usize>,

    // ── Filter predicates (narrow the summary population) ────────────────────
    /// Full-text body substring. Case-insensitive.
    #[serde(default)]
    pub text: Option<String>,
    /// Frontmatter equality predicates `field:value`. Repeatable.
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
    /// Frontmatter prefix predicates `field:VALUE`. Repeatable.
    #[serde(default)]
    pub starts_with: Vec<String>,
    /// Frontmatter suffix predicates `field:VALUE`. Repeatable.
    #[serde(default)]
    pub ends_with: Vec<String>,
    /// Frontmatter substring predicates `field:VALUE`. Repeatable.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Frontmatter fields that must be present (non-null). Repeatable.
    #[serde(default)]
    pub has: Vec<String>,
    /// Frontmatter fields that must be absent or null. Repeatable.
    #[serde(default)]
    pub missing: Vec<String>,
    /// Date-before predicates `field:DATE`. Repeatable.
    #[serde(default)]
    pub before: Vec<String>,
    /// Date-after predicates `field:DATE`. Repeatable.
    #[serde(default)]
    pub after: Vec<String>,
    /// Date-on predicates `field:DATE`. Repeatable.
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

/// Structured output for `vault.describe` — the flat report (`folders`,
/// `path_rules`, `creatable_rules`, `inbox`, `schema`, and the optional `data`
/// summary at the top level).
pub type DescribeOutput = FlatReport;

/// Build the wire request from the MCP params. The single `by` comma token is
/// split into fields the same way the CLI's `--by` list does.
pub(crate) fn to_wire(p: DescribeParams) -> WireDescribeParams {
    let by: Vec<String> =
        p.by.as_deref()
            .map(|token| token.split(',').map(str::to_string).collect())
            .unwrap_or_default();
    WireDescribeParams {
        // `--by` implies `--data` (mirrors the verb's own normalization).
        data: p.data || !by.is_empty(),
        by,
        limit: p.limit,
        filter: FilterParams {
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
        },
        dynamic_keys: Vec::new(),
    }
}

/// Project the wire report FLAT into the MCP structured content.
pub(crate) fn envelope(report: DescribeReport) -> DescribeOutput {
    FlatReport(serde_json::to_value(&report).unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn by_token_splits_and_implies_data() {
        let wire = to_wire(DescribeParams {
            by: Some("type,status".into()),
            ..Default::default()
        });
        assert_eq!(wire.by, vec!["type".to_string(), "status".to_string()]);
        assert!(wire.data, "--by implies data");
    }

    #[test]
    fn plain_describe_omits_data() {
        let wire = to_wire(DescribeParams::default());
        assert!(!wire.data);
        assert!(wire.by.is_empty());
    }

    #[test]
    fn filter_maps_onto_the_wire_filter() {
        let wire = to_wire(DescribeParams {
            eq: vec!["type:note".into()],
            ..Default::default()
        });
        assert_eq!(wire.filter.eq, vec!["type:note".to_string()]);
    }

    #[test]
    fn envelope_emits_the_report_flat() {
        let report = DescribeReport {
            folders: vec!["notes".into()],
            schema: json!({"fields": []}),
            ..Default::default()
        };
        let out = envelope(report);
        // Flat: `folders` is a top-level key, not nested under `report`.
        assert_eq!(out.0["folders"][0], json!("notes"));
        assert!(out.0.get("report").is_none(), "the report is emitted flat");
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<DescribeParams>(json!({ "bogus": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("bogus"));
    }
}
