//! `vault.repair` — build the standards-repair plan, without applying it.
//!
//! The param struct mirrors `norn repair`'s triage filters; the handler routes
//! to the owner and returns the wire [`RepairReport`] FLAT as the tool's
//! `structuredContent` (the read-verb envelope shape — `plan` / `skipped_detail`
//! at the top level). The report carries the deterministic `MigrationPlan` plus
//! the rich per-skip detail ([`RepairReport::skipped_detail`] — reason code,
//! candidate paths, next actions) that the lean plan omits; a client feeds the
//! top-level `plan` straight to `vault.apply` to execute it. Read-only: the
//! owner never writes.

use norn_wire::{RepairParams as WireRepairParams, RepairReport};
use serde::Deserialize;

use crate::mutation_result::FlatReport;

/// Parameters for `vault.repair` — the agent-relevant triage filters plus the
/// confidence-band gate. All optional; omitted → no filter.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct RepairParams {
    /// Filter source findings by code. Comma-separated; supports glob patterns.
    #[serde(default)]
    pub code: Vec<String>,
    /// Filter source findings by severity (`warning` or `error`). Comma-separated.
    #[serde(default)]
    pub severity: Vec<String>,
    /// Filter source findings by frontmatter field name. Comma-separated.
    #[serde(default)]
    pub field: Vec<String>,
    /// Filter source findings by validate rule name. Comma-separated.
    #[serde(default)]
    pub rule: Vec<String>,
    /// Filter source findings by vault-relative path glob. Comma-separated.
    #[serde(default)]
    pub path: Vec<String>,
    /// Filter link findings by link target. Comma-separated.
    #[serde(default)]
    pub target: Vec<String>,
    /// Filter link findings by unresolved reason. Comma-separated.
    #[serde(default)]
    pub reason: Vec<String>,
    /// Narrow the plan's skipped-findings list by reason-code glob. Comma-separated.
    #[serde(default)]
    pub skip_reason: Vec<String>,
    /// Keep only High-confidence closest-match proposals (drop Medium ones).
    #[serde(default)]
    pub confidence_high: bool,
}

/// Structured output for `vault.repair` — the flat report: the deterministic
/// `plan` (feed it to `vault.apply`), the rich `skipped_detail` (reason code,
/// candidate paths, next actions), the per-code finding tally, and the run
/// header counts, all at the top level.
pub type RepairOutput = FlatReport;

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: RepairParams) -> WireRepairParams {
    WireRepairParams {
        codes: p.code,
        severities: p.severity,
        fields: p.field,
        rules: p.rule,
        paths: p.path,
        targets: p.target,
        reasons: p.reason,
        skip_reasons: p.skip_reason,
        confidence_high: p.confidence_high,
        // The MCP surface keeps full graph-diagnostic `detail` in findings — an
        // off-filesystem client cannot re-derive it, so repair runs verbose
        // (mirrors the `vault.validate` decision).
        verbose: true,
    }
}

/// Project the wire report FLAT into the MCP structured content. The whole
/// report — including the rich `skipped_detail` — is exposed, so a repair client
/// reads the candidate paths / next actions the lean plan omits.
pub(crate) fn envelope(report: RepairReport) -> RepairOutput {
    FlatReport(serde_json::to_value(&report).unwrap_or(serde_json::Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::{MigrationPlan, RepairSkipDetail};
    use serde_json::json;

    #[test]
    fn filters_map_onto_the_wire_and_verbose_is_on() {
        let wire = to_wire(RepairParams {
            code: vec!["link-*".into()],
            confidence_high: true,
            ..Default::default()
        });
        assert_eq!(wire.codes, vec!["link-*".to_string()]);
        assert!(wire.confidence_high);
        assert!(wire.verbose, "MCP repair runs verbose for full detail");
    }

    #[test]
    fn envelope_carries_the_plan_and_skip_detail() {
        let report = RepairReport {
            plan: MigrationPlan {
                schema_version: 2,
                vault_root: "/v".into(),
                ..Default::default()
            },
            skipped_detail: vec![RepairSkipDetail {
                finding_code: "link-ambiguous".into(),
                path: "notes/a.md".into(),
                reason_code: "ambiguous-target".into(),
                candidates: vec!["x/Daily.md".into()],
                next_actions: vec!["change the link".into()],
            }],
            findings_total: 1,
            total_docs: 3,
            ..Default::default()
        };
        let out = envelope(report);
        // Flat: `plan` / `skipped_detail` are top-level keys, not under `report`.
        assert_eq!(out.0["plan"]["vault_root"], json!("/v"));
        assert!(out.0.get("report").is_none(), "the report is emitted flat");
        // The repair tool surfaces the rich skip detail the lean plan omits.
        assert_eq!(
            out.0["skipped_detail"][0]["reason_code"],
            json!("ambiguous-target")
        );
        assert_eq!(
            out.0["skipped_detail"][0]["candidates"][0],
            json!("x/Daily.md")
        );
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<RepairParams>(json!({ "bogus": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("bogus"));
    }
}
