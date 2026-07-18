//! `EditReport` — the `norn edit` / `vault.edit` output envelope. Same outer
//! shape as `SetReport`, with an `edits` array (one entry per applied op)
//! replacing `frontmatter_changes`.

use crate::edit::transform::EditDescriptor;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::io::Write;

/// `Deserialize` is derived so the CLI→service routing seam (NRN-229) can
/// rebuild an `EditReport` from a routed `vault.edit`'s `structuredContent` and
/// render it through the SAME `render_json`/`render_records` the direct path
/// uses — the load-bearing routed↔direct isomorphism (ADR 0005), mirroring
/// `SetReport`. Serialization is unchanged, so the MCP tool output stays
/// byte-identical.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditReport {
    pub schema_version: u32,
    /// Shared by every telemetry event for this invocation. Empty on dry-run.
    pub trace_id: String,
    pub operation: String,
    pub target: Utf8PathBuf,
    pub edits: Vec<EditChange>,
    pub body_changed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_bytes_old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_bytes_new: Option<usize>,
    pub applied: bool,
    /// Machine-branchable apply outcome (NRN-220): `applied` on success and
    /// dry-run; `refused` when a coded refusal was captured (code in `error`).
    pub outcome: crate::apply_report::ApplyOutcome,
    /// Structured refusal envelope (kebab `code` + `message` + optional `path`)
    /// when `outcome` is `refused`; `None` otherwise. A consumer branches on
    /// `error.code` (`anchor-not-found`, `stale-document-hash`, …), not prose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::apply_report::ApplyError>,
}

impl EditReport {
    /// Build a minimal refusal report (NRN-220): a coded refusal captured on the
    /// MCP path. Nothing applied; `error` carries the stable machine code.
    pub fn refused(target: Utf8PathBuf, error: crate::apply_report::ApplyError) -> Self {
        EditReport {
            schema_version: SCHEMA_VERSION,
            trace_id: String::new(),
            operation: "edit".to_string(),
            target,
            edits: Vec::new(),
            body_changed: false,
            body_bytes_old: None,
            body_bytes_new: None,
            applied: false,
            outcome: crate::apply_report::ApplyOutcome::Refused,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditChange {
    pub op: String,
    pub anchor: String,
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrences: Option<usize>,
    pub applied: bool,
}

pub const SCHEMA_VERSION: u32 = 1;

/// Build an `EditReport` from a preflight outcome + the per-op descriptors.
pub fn build_report(
    outcome: &crate::set::synth::PreflightOutcome,
    descriptors: &[EditDescriptor],
    applied: bool,
    trace_id: &str,
) -> EditReport {
    EditReport {
        schema_version: SCHEMA_VERSION,
        trace_id: trace_id.to_string(),
        operation: "edit".to_string(),
        target: outcome.target.clone(),
        edits: descriptors
            .iter()
            .map(|d| EditChange {
                op: d.op.clone(),
                anchor: d.anchor_desc.clone(),
                matched: true,
                occurrences: d.occurrences,
                applied,
            })
            .collect(),
        body_changed: outcome.body_changed,
        body_bytes_old: Some(outcome.body_bytes_old),
        body_bytes_new: outcome.body_bytes_new,
        applied,
        outcome: crate::apply_report::ApplyOutcome::Applied,
        error: None,
    }
}

/// Serialize an `EditReport` as newline-terminated JSON.
pub fn render_json<W: Write>(out: &mut W, report: &EditReport) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, report).map_err(std::io::Error::other)?;
    writeln!(out)?;
    Ok(())
}

/// TTY records-block summary.
pub fn render_records<W: Write>(out: &mut W, report: &EditReport) -> std::io::Result<()> {
    let verb = if report.applied {
        "edit"
    } else {
        "dry-run: edit"
    };
    writeln!(out, "{verb} {}", report.target)?;
    for change in &report.edits {
        match change.occurrences {
            Some(n) => writeln!(out, "  {} ({}, {n}×)", change.op, change.anchor)?,
            None => writeln!(out, "  {} ({})", change.op, change.anchor)?,
        }
    }
    if report.body_changed {
        let old = report.body_bytes_old.unwrap_or(0);
        let new = report.body_bytes_new.unwrap_or(0);
        writeln!(out, "  body: {old} → {new} bytes")?;
    }
    if !report.applied {
        writeln!(out)?;
        writeln!(out, "Apply with --yes")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome() -> crate::set::synth::PreflightOutcome {
        // `RepairPlan` does not derive `Default`, so build it the same way
        // `set::synth::preflight_and_plan` does (its sub-structs do derive it).
        let plan = crate::standards::RepairPlan {
            schema_version: crate::standards::REPAIR_PLAN_SCHEMA_VERSION,
            vault_root: Utf8PathBuf::from("."),
            source_filters: crate::standards::RepairPlanFilters::default(),
            summary: crate::standards::RepairPlanSummary {
                findings: 0,
                planned_changes: 0,
                skipped: crate::standards::SkippedSummary::default(),
            },
            changes: Vec::new(),
            skipped_findings: Vec::new(),
            footnotes: Vec::new(),
        };
        crate::set::synth::PreflightOutcome {
            plan,
            warnings: vec![],
            target: Utf8PathBuf::from("note.md"),
            body_changed: true,
            body_bytes_new: Some(20),
            body_bytes_old: 10,
        }
    }

    #[test]
    fn json_has_edit_envelope() {
        let descs = vec![EditDescriptor {
            op: "str_replace".into(),
            anchor_desc: r#"old="a""#.into(),
            occurrences: Some(1),
        }];
        let report = build_report(&outcome(), &descs, true, "trace123");
        let mut buf = Vec::new();
        render_json(&mut buf, &report).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["operation"], "edit");
        assert_eq!(v["applied"], true);
        assert_eq!(v["edits"][0]["op"], "str_replace");
        assert_eq!(v["edits"][0]["occurrences"], 1);
        assert_eq!(v["trace_id"], "trace123");
    }
}
