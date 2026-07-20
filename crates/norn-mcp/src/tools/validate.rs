//! `vault.validate` — validate graph facts and configured rules; structured
//! findings.
//!
//! The param struct mirrors `norn validate`'s triage filters; the handler routes
//! to the owner and projects the wire [`ValidateReport`] (whose findings cross as
//! pre-serialized JSON strings — the owner's byte-identical `serde_json` of each
//! `Finding`) into the typed [`ValidateOutput`] envelope: the strings are parsed
//! back to `Value` (the owner already produced the canonical key order), so the
//! MCP `structuredContent` carries typed findings rather than double-serialized
//! strings.

use norn_wire::{ValidateParams as WireValidateParams, ValidateReport};
use serde::{Deserialize, Serialize};

/// Parameters for `vault.validate` — the agent-relevant triage filters. All
/// optional; omitted → no filter (return all findings).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct ValidateParams {
    /// Filter findings by code. Comma-separated values match any listed code.
    /// Supports glob patterns (e.g. `link-*` matches all link findings).
    #[serde(default)]
    pub code: Vec<String>,

    /// Filter findings by severity (`warning` or `error`). Comma-separated.
    #[serde(default)]
    pub severity: Vec<String>,

    /// Filter findings by frontmatter field name. Comma-separated.
    #[serde(default)]
    pub field: Vec<String>,

    /// Filter findings by validate rule name. Comma-separated.
    #[serde(default)]
    pub rule: Vec<String>,

    /// Filter findings by vault-relative path glob. Comma-separated.
    #[serde(default)]
    pub path: Vec<String>,

    /// Filter link findings by link target. Comma-separated.
    #[serde(default)]
    pub target: Vec<String>,

    /// Filter link findings by unresolved reason. Comma-separated.
    #[serde(default)]
    pub reason: Vec<String>,

    /// Return the grouped finding-count rollup instead of the raw findings list —
    /// the structured analogue of `norn validate --summary`. When set, `findings`
    /// is omitted and `summary` carries the rollup. Triage filters still apply first.
    #[serde(default)]
    pub summary: bool,
}

/// Structured output for `vault.validate` — a `type: object` root wrapping the
/// per-finding payload as generic `Value` (a `Finding` carries a path type with
/// no `JsonSchema` impl).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ValidateOutput {
    /// Validation findings, filtered by any supplied triage predicates. Each
    /// entry is the JSON form of a `Finding`. **Absent** in `summary` mode (the
    /// findings are rolled up into `summary` instead), so `findings: []`
    /// unambiguously means CLEAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<serde_json::Value>>,

    /// The grouped finding-count rollup, present only when `summary: true` was
    /// requested — byte-for-byte the same shape `norn validate --summary --format
    /// json` emits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<serde_json::Value>,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: ValidateParams) -> WireValidateParams {
    WireValidateParams {
        codes: p.code,
        severities: p.severity,
        fields: p.field,
        rules: p.rule,
        paths: p.path,
        targets: p.target,
        reasons: p.reason,
        // The MCP surface keeps full graph-diagnostic `detail` in findings — the
        // donor's `vault.validate` ran in the verbose mode (an off-filesystem
        // client cannot re-derive the detail), unlike the non-verbose CLI default.
        verbose: true,
        summary: p.summary,
    }
}

/// Project the wire report into the typed output envelope. The pre-serialized
/// finding strings are re-parsed to `Value` (the owner emitted them in the
/// canonical key order) so the surface carries typed findings, never
/// double-serialized strings.
pub(crate) fn envelope(report: ValidateReport, summary_requested: bool) -> ValidateOutput {
    if summary_requested {
        ValidateOutput {
            findings: None,
            summary: report
                .summary_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
        }
    } else {
        let findings = report
            .findings
            .iter()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect();
        ValidateOutput {
            findings: Some(findings),
            summary: None,
        }
    }
}
