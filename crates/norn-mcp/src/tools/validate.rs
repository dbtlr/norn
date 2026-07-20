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
///
/// A parse failure on this double-encoding seam is surfaced as an `Err`, NOT
/// silently dropped: an unparseable finding string (or a bad `--summary` body)
/// can only happen on owner/client version skew, and silently losing a finding —
/// or turning a dirty vault's rollup into `null` — is the wrong failure mode for a
/// validation surface. The caller maps it to a tool ERROR.
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
        let findings = report
            .findings
            .iter()
            .map(|s| {
                serde_json::from_str(s).map_err(|e| {
                    format!("a validate finding did not re-parse (owner/client skew?): {e}")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ValidateOutput {
            findings: Some(findings),
            summary: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(findings: Vec<String>, summary_json: Option<String>) -> ValidateReport {
        ValidateReport {
            findings,
            summary_json,
            total_docs: 0,
            rules_count: 0,
            has_errors: false,
        }
    }

    #[test]
    fn findings_reparse_to_typed_values() {
        let out = envelope(
            report(vec![r#"{"code":"x","severity":"warning"}"#.into()], None),
            false,
        )
        .unwrap();
        let findings = out.findings.expect("findings present in default mode");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["code"], "x");
        assert!(out.summary.is_none());
    }

    #[test]
    fn empty_findings_is_clean_not_null() {
        let out = envelope(report(vec![], None), false).unwrap();
        assert_eq!(out.findings, Some(vec![]), "clean vault is findings: []");
    }

    #[test]
    fn an_unparseable_finding_is_a_tool_error_not_a_silent_drop() {
        let err = envelope(report(vec!["not json".into()], None), false).unwrap_err();
        assert!(err.contains("did not re-parse"), "got: {err}");
    }

    #[test]
    fn summary_mode_omits_findings_and_parses_the_rollup() {
        let out = envelope(report(vec![], Some(r#"{"findings":3}"#.into())), true).unwrap();
        assert!(out.findings.is_none(), "summary mode omits findings");
        assert_eq!(out.summary.unwrap()["findings"], 3);
    }

    #[test]
    fn an_unparseable_summary_body_is_a_tool_error() {
        let err = envelope(report(vec![], Some("{bad".into())), true).unwrap_err();
        assert!(err.contains("did not re-parse"), "got: {err}");
    }
}
