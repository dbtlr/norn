//! `norn set` output report: JSON envelope + records-block rendering.

// These items are pub for Phase 5.4 wiring; the binary doesn't call them yet.
#![allow(dead_code)]

use std::io::Write;

use camino::Utf8PathBuf;
use serde::Serialize;

use crate::set::validate::SetWarning;

pub const SET_REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct SetReport {
    pub schema_version: u32,
    /// Trace ID shared by every telemetry event emitted for this invocation.
    /// Empty on dry-run/preview (no stream is persisted).
    pub trace_id: String,
    pub operation: String,
    pub target: Utf8PathBuf,
    pub frontmatter_changes: Vec<FrontmatterChange>,
    pub body_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_bytes_new: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_bytes_old: Option<usize>,
    pub applied: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<SetWarning>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FrontmatterChange {
    pub op: String,
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<bool>,
}

/// Build a SetReport from the preflight outcome + applied flag.
///
/// `trace_id` is the telemetry trace ID for the invocation; pass `""` on
/// dry-run/preview paths where no event stream is persisted.
pub fn build_report(
    outcome: &crate::set::synth::PreflightOutcome,
    applied: bool,
    trace_id: &str,
) -> SetReport {
    let mut frontmatter_changes: Vec<FrontmatterChange> = Vec::new();
    for c in outcome
        .plan
        .changes
        .iter()
        .filter(|c| c.operation != "replace_body")
    {
        let op = match c.operation.as_str() {
            "set_frontmatter" | "add_frontmatter" => "set",
            "remove_frontmatter" => "remove",
            other => other,
        };
        let field = c.field.clone().unwrap_or_default();
        let entry = FrontmatterChange {
            op: op.to_string(),
            field,
            old: c.expected_old_value.clone(),
            new: c.new_value.clone(),
            value: None,
            found: None,
        };
        frontmatter_changes.push(entry);
    }

    SetReport {
        schema_version: SET_REPORT_SCHEMA_VERSION,
        trace_id: trace_id.to_string(),
        operation: "set".to_string(),
        target: outcome.target.clone(),
        frontmatter_changes,
        body_changed: outcome.body_changed,
        body_bytes_new: outcome.body_bytes_new,
        body_bytes_old: if outcome.body_changed {
            Some(outcome.body_bytes_old)
        } else {
            None
        },
        applied,
        warnings: outcome.warnings.clone(),
    }
}

/// Serialize a SetReport as newline-terminated JSON to `out`.
pub fn render_json<W: Write>(out: &mut W, report: &SetReport) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, report).map_err(std::io::Error::other)?;
    writeln!(out)?;
    Ok(())
}

/// TTY records-block summary of a SetReport. Lists changed fields with their
/// before/after, body-bytes delta, and a `--yes` next-step hint on dry-run.
pub fn render_records<W: Write>(out: &mut W, report: &SetReport) -> std::io::Result<()> {
    let verb = if report.applied {
        "set"
    } else {
        "dry-run: set"
    };
    writeln!(out, "{verb} {}", report.target)?;

    for change in &report.frontmatter_changes {
        match change.op.as_str() {
            "set" => writeln!(
                out,
                "  {}: {} → {}",
                change.field,
                value_repr(change.old.as_ref()),
                value_repr(change.new.as_ref())
            )?,
            "push" => writeln!(
                out,
                "  {}: push {}",
                change.field,
                value_repr(change.value.as_ref())
            )?,
            "pop" => writeln!(
                out,
                "  {}: pop {} (found: {})",
                change.field,
                value_repr(change.value.as_ref()),
                change.found.unwrap_or(false)
            )?,
            "remove" => writeln!(
                out,
                "  {}: remove (was {})",
                change.field,
                value_repr(change.old.as_ref())
            )?,
            other => writeln!(out, "  {}: {} (unknown op)", change.field, other)?,
        }
    }

    if report.body_changed {
        let new = report.body_bytes_new.unwrap_or(0);
        let old = report.body_bytes_old.unwrap_or(0);
        writeln!(out, "  body: {old} → {new} bytes")?;
    }

    if !report.warnings.is_empty() {
        writeln!(out, "  warnings: {}", report.warnings.len())?;
        for w in report.warnings.iter().take(3) {
            writeln!(out, "    - {}", warning_label(w))?;
        }
        if report.warnings.len() > 3 {
            writeln!(out, "    … ({} more)", report.warnings.len() - 3)?;
        }
    }

    if !report.applied {
        writeln!(out)?;
        writeln!(out, "Apply with --yes")?;
    }

    Ok(())
}

fn value_repr(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "<none>".to_string(),
    }
}

fn warning_label(w: &SetWarning) -> String {
    match w {
        SetWarning::UnknownField { field, .. } => format!("unknown field: {field}"),
        SetWarning::WikilinkUnresolved { field, target } => {
            format!("unresolved wikilink in {field}: [[{target}]]")
        }
        SetWarning::WikilinkAmbiguous { field, target, .. } => {
            format!("ambiguous wikilink in {field}: [[{target}]]")
        }
        SetWarning::ForceBypass { field, .. } => format!("--force bypass: {field}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Task 5.2: JSON envelope shape ────────────────────────────────────────

    #[test]
    fn set_report_json_envelope_shape() {
        let report = SetReport {
            schema_version: 2,
            trace_id: "".to_string(),
            operation: "set".to_string(),
            target: "notes/foo.md".into(),
            frontmatter_changes: vec![
                FrontmatterChange {
                    op: "set".to_string(),
                    field: "status".to_string(),
                    old: Some(json!("draft")),
                    new: Some(json!("active")),
                    value: None,
                    found: None,
                },
                FrontmatterChange {
                    op: "push".to_string(),
                    field: "aliases".to_string(),
                    old: None,
                    new: None,
                    value: Some(json!("foo")),
                    found: None,
                },
            ],
            body_changed: true,
            body_bytes_new: Some(4821),
            body_bytes_old: Some(4520),
            applied: true,
            warnings: vec![],
        };
        let json_str = serde_json::to_string(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["operation"], "set");
        assert_eq!(parsed["target"], "notes/foo.md");
        assert_eq!(parsed["frontmatter_changes"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["frontmatter_changes"][0]["op"], "set");
        assert_eq!(parsed["frontmatter_changes"][1]["op"], "push");
        assert_eq!(parsed["body_changed"], true);
        assert_eq!(parsed["applied"], true);
        // schema_version is u32 → in JSON it's a Number
        assert_eq!(parsed["schema_version"], 2);
    }

    // ── Task 5.3: TTY records renderer ───────────────────────────────────────

    #[test]
    fn render_records_dry_run_emits_summary_and_next_step() {
        let report = SetReport {
            schema_version: 2,
            trace_id: "".to_string(),
            operation: "set".to_string(),
            target: "notes/foo.md".into(),
            frontmatter_changes: vec![FrontmatterChange {
                op: "set".to_string(),
                field: "status".to_string(),
                old: Some(json!("draft")),
                new: Some(json!("active")),
                value: None,
                found: None,
            }],
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied: false,
            warnings: vec![],
        };
        let mut out = Vec::new();
        render_records(&mut out, &report).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("notes/foo.md"), "should contain target");
        assert!(s.contains("status"));
        assert!(s.contains("draft") && s.contains("active"));
        assert!(s.contains("--yes"), "should suggest --yes on dry-run");
    }

    #[test]
    fn render_records_applied_omits_yes_hint() {
        let report = SetReport {
            schema_version: 2,
            trace_id: "".to_string(),
            operation: "set".to_string(),
            target: "notes/foo.md".into(),
            frontmatter_changes: vec![],
            body_changed: true,
            body_bytes_new: Some(100),
            body_bytes_old: Some(80),
            applied: true,
            warnings: vec![],
        };
        let mut out = Vec::new();
        render_records(&mut out, &report).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("notes/foo.md"));
        assert!(s.contains("body") && s.contains("80") && s.contains("100"));
        assert!(
            !s.contains("--yes"),
            "applied report should NOT suggest --yes"
        );
    }

    #[test]
    fn render_records_truncates_warning_list_at_three() {
        let report = SetReport {
            schema_version: 2,
            trace_id: "".to_string(),
            operation: "set".to_string(),
            target: "notes/foo.md".into(),
            frontmatter_changes: vec![],
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied: false,
            warnings: vec![
                SetWarning::UnknownField {
                    field: "a".into(),
                    message: "x".into(),
                },
                SetWarning::UnknownField {
                    field: "b".into(),
                    message: "x".into(),
                },
                SetWarning::UnknownField {
                    field: "c".into(),
                    message: "x".into(),
                },
                SetWarning::UnknownField {
                    field: "d".into(),
                    message: "x".into(),
                },
                SetWarning::UnknownField {
                    field: "e".into(),
                    message: "x".into(),
                },
            ],
        };
        let mut out = Vec::new();
        render_records(&mut out, &report).unwrap();
        let s = String::from_utf8(out).unwrap();
        // Header line shows total
        assert!(s.contains("5"));
        // Truncation note appears
        assert!(s.contains("more") || s.contains("…"));
    }
}
