//! `vault.describe` — the vault's structure and configured schema.
//!
//! The param struct carries `norn describe`'s data-mode and filter surface; the
//! handler routes to the owner and returns the wire [`DescribeReport`] FLAT as
//! the tool's `structuredContent` (the read-verb envelope shape — `folders` /
//! `path_rules` / `schema` at the top level, not wrapped under a `report` key),
//! via the [`FlatReport`] newtype that satisfies rmcp's `type: object` schema
//! demand.
//!
//! The report is ALWAYS the full declared config — every folder, every rule with
//! its defaults, the whole frontmatter schema. There is no counts-only mode
//! here: the CLI's counts projection (`norn describe` without `--schema`) is a
//! display-layer rendering of this same report, not a second owner response
//! shape. A params-level selector for it is tracked as NRN-492.

use norn_wire::{DescribeParams as WireDescribeParams, DescribeReport};
use serde::Deserialize;

use crate::mutation_result::FlatReport;
use crate::tools::filters::filter_params;

filter_params! {
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
    }
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
        filter: p.to_filter(),
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
