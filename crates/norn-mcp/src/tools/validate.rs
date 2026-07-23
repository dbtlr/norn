//! `vault.validate` — validate graph facts and configured rules; structured
//! findings.
//!
//! The param struct mirrors `norn validate`'s triage filters; the handler routes
//! to the owner and projects the wire [`ValidateReport`] into the typed
//! [`ValidateOutput`] envelope. Findings cross as the flat [`norn_wire::Finding`]
//! contract (ADR 0022 — no pre-serialized string tunnel), so the MCP
//! `structuredContent` carries them directly, schema and all, with no re-parse.

use norn_wire::{Finding, ValidateParams as WireValidateParams, ValidateReport};
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
/// per-finding payload as the typed, schema-bearing [`Finding`] contract (ADR
/// 0022: the flat wire type is nameable and closed, so the tool exposes a schema
/// instead of opaque values).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ValidateOutput {
    /// Validation findings, filtered by any supplied triage predicates.
    /// **Absent** in `summary` mode (the findings are rolled up into `summary`
    /// instead), so `findings: []` unambiguously means CLEAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<Vec<Finding>>,

    /// The grouped finding-count rollup, present only when `summary: true` was
    /// requested — the same shape `norn validate --summary --format json` emits.
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
        // The MCP surface keeps full graph-diagnostic `detail` in findings — an
        // off-filesystem client cannot re-derive the detail, so it runs verbose,
        // unlike the non-verbose CLI default.
        verbose: true,
        summary: p.summary,
    }
}

/// Project the wire report into the typed output envelope. Findings are already
/// the typed flat [`Finding`] contract (ADR 0022), so they move straight into
/// `structuredContent` — no string re-parse, no owner/client double-encoding
/// seam. The `--summary` body is still a pre-rendered JSON string (the grouped
/// rollup's exact `--format json --summary` bytes), so it alone can fail to
/// re-parse on owner/client skew; that surfaces as an `Err`, never a silent
/// `null` rollup, and the caller maps it to a tool ERROR.
pub(crate) fn envelope(
    report: ValidateReport,
    summary_requested: bool,
) -> Result<ValidateOutput, String> {
    if summary_requested {
        let summary = match report.summary_json.as_deref() {
            Some(s) => Some(serde_json::from_str(s).map_err(|e| {
                format!("validate --summary body did not re-parse (owner/client skew?): {e}")
            })?),
            None => None,
        };
        Ok(ValidateOutput {
            findings: None,
            summary,
        })
    } else {
        Ok(ValidateOutput {
            findings: Some(report.findings),
            summary: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::Severity;

    fn report(findings: Vec<Finding>, summary_json: Option<String>) -> ValidateReport {
        ValidateReport {
            findings,
            summary_json,
            total_docs: 0,
            rules_count: 0,
            has_errors: false,
        }
    }

    fn finding(code: &str) -> Finding {
        Finding {
            path: "notes/a.md".into(),
            code: code.into(),
            severity: Severity::Warning,
            message: "m".into(),
            rule: None,
            field: None,
            target: None,
            candidates: vec![],
            next_actions: vec![],
        }
    }

    #[test]
    fn typed_findings_pass_through_to_structured_content() {
        // ADR 0022: the wire already carries the typed flat contract, so the
        // envelope moves findings straight through — no re-parse seam remains.
        let out = envelope(report(vec![finding("x")], None), false).unwrap();
        let findings = out.findings.expect("findings present in default mode");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "x");
        // The typed finding serializes cleanly into structuredContent.
        let v = serde_json::to_value(&findings[0]).unwrap();
        assert_eq!(v["code"], "x");
        assert_eq!(v["severity"], "warning");
        assert!(out.summary.is_none());
    }

    #[test]
    fn empty_findings_is_clean_not_null() {
        let out = envelope(report(vec![], None), false).unwrap();
        assert_eq!(out.findings, Some(vec![]), "clean vault is findings: []");
    }

    #[test]
    fn summary_mode_omits_findings_and_parses_the_rollup() {
        let out = envelope(report(vec![], Some(r#"{"findings":3}"#.into())), true).unwrap();
        assert!(out.findings.is_none(), "summary mode omits findings");
        assert_eq!(out.summary.unwrap()["findings"], 3);
    }

    #[test]
    fn an_unparseable_summary_body_is_a_tool_error() {
        // The summary rollup is still a pre-rendered JSON string, so it is the
        // one seam that can fail to re-parse on owner/client skew.
        let err = envelope(report(vec![], Some("{bad".into())), true).unwrap_err();
        assert!(err.contains("did not re-parse"), "got: {err}");
    }
}
