//! ApplyReport — the unified output envelope for migrate, move, delete,
//! rewrite-wikilink, and future new/set conversions.
//!
//! Replaces MoveReport, DeleteReport, RepairApplyReport.

use serde::{Deserialize, Serialize};

pub const APPLY_REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub schema_version: u32,
    /// Trace ID shared by every telemetry event emitted for this invocation.
    pub trace_id: String,
    pub plan_hash: String,
    pub vault_root: String,
    pub dry_run: bool,
    pub applied: usize,
    pub skipped: usize,
    pub failed: usize,
    pub remaining: usize,
    pub operations: Vec<ApplyReportOp>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<ApplyWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReportOp {
    pub op_id: String,
    pub kind: String,
    pub status: OpStatus,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<ApplyError>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub footnote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cascade: Option<CascadeSummary>,
}

/// Per-op summary of the backlink cascade triggered by a `move_document` or
/// `delete_document` op. Counts (`planned`/`applied`/`skipped`/`files`) are
/// always present; `rewrites`/`skips` lists are populated only under
/// `--verbose`.
///
/// - `planned`  — backlinks the plan intended to rewrite (from `link_risk`).
/// - `applied`  — backlinks actually rewritten on disk (the actual, not the forecast).
/// - `skipped`  — planned-not-applied (drift); each carries a reason.
/// - `failed`   — backlinks that hit a real FS error and remained un-rewritten.
/// - `files`    — distinct files actually rewritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeSummary {
    pub planned: usize,
    pub applied: usize,
    pub skipped: usize,
    /// Backlinks that hit a real FS error and remained un-rewritten after the
    /// retry pass (dangling). Always present.
    pub failed: usize,
    pub files: usize,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rewrites: Vec<CascadeRewrite>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub skips: Vec<CascadeSkip>,
    /// Per-failure detail. NOT verbose-gated — a failure is ERROR-severity and
    /// must be visible by default (and feeds the stderr warning). Present
    /// whenever non-empty.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub failures: Vec<CascadeFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeFailure {
    pub file: String,
    pub from: String,
    pub to: String,
    /// Reason code: `read_failed` | `write_failed`.
    pub reason: String,
    /// The underlying io error string (e.g. "Permission denied (os error 13)").
    /// Present when known; the actionable "why" behind the reason code.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeRewrite {
    pub file: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CascadeSkip {
    pub file: String,
    pub from: String,
    pub to: String,
    /// Reason code (v1: `"drifted"`). Extensible — a later slice adds failure codes.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    Applied,
    Skipped,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyWarning {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_report_serializes_with_per_op_status() {
        let report = ApplyReport {
            schema_version: 2,
            trace_id: "".into(),
            plan_hash: "abc123".into(),
            vault_root: "/abs/vault".into(),
            dry_run: false,
            applied: 1,
            skipped: 0,
            failed: 0,
            remaining: 0,
            operations: vec![ApplyReportOp {
                op_id: "0".into(),
                kind: "move_document".into(),
                status: OpStatus::Applied,
                from: None,
                summary: "moved a.md → b.md".into(),
                error: None,
                footnote: None,
                cascade: None,
            }],
            warnings: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ApplyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.applied, 1);
        assert_eq!(back.operations[0].status, OpStatus::Applied);
    }

    #[test]
    fn op_status_serializes_as_snake_case() {
        let json = serde_json::to_string(&OpStatus::NotRun).unwrap();
        assert_eq!(json, "\"not_run\"");
        let parsed: OpStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(parsed, OpStatus::Failed);
    }

    #[test]
    fn cascade_summary_serializes_counts_always_lists_when_present() {
        let op = ApplyReportOp {
            op_id: "0".into(),
            kind: "move_document".into(),
            status: OpStatus::Applied,
            from: None,
            summary: "moved a.md → b.md".into(),
            error: None,
            footnote: None,
            cascade: Some(CascadeSummary {
                planned: 3,
                applied: 2,
                skipped: 1,
                failed: 0,
                files: 2,
                rewrites: vec![CascadeRewrite {
                    file: "x.md".into(),
                    from: "[[a]]".into(),
                    to: "[[b]]".into(),
                }],
                skips: vec![CascadeSkip {
                    file: "y.md".into(),
                    from: "[[a]]".into(),
                    to: "[[b]]".into(),
                    reason: "drifted".into(),
                }],
                failures: vec![],
            }),
        };
        let json = serde_json::to_value(&op).unwrap();
        assert_eq!(json["cascade"]["planned"], 3);
        assert_eq!(json["cascade"]["applied"], 2);
        assert_eq!(json["cascade"]["skipped"], 1);
        assert_eq!(json["cascade"]["files"], 2);
        assert_eq!(json["cascade"]["skips"][0]["reason"], "drifted");

        let bare = ApplyReportOp {
            op_id: "1".into(),
            kind: "set_frontmatter".into(),
            status: OpStatus::Applied,
            from: None,
            summary: "set type".into(),
            error: None,
            footnote: None,
            cascade: None,
        };
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert!(bare_json.get("cascade").is_none());
    }

    #[test]
    fn cascade_summary_serializes_failed_count_and_failures_list() {
        let summary = CascadeSummary {
            planned: 3,
            applied: 1,
            skipped: 1,
            failed: 1,
            files: 1,
            rewrites: vec![],
            skips: vec![],
            failures: vec![CascadeFailure {
                file: "d.md".into(),
                from: "[[a]]".into(),
                to: "[[b]]".into(),
                reason: "write_failed".into(),
                detail: Some("Permission denied (os error 13)".into()),
            }],
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["failed"], 1);
        assert_eq!(json["failures"][0]["reason"], "write_failed");
        assert_eq!(json["failures"][0]["file"], "d.md");
        assert_eq!(
            json["failures"][0]["detail"],
            "Permission denied (os error 13)"
        );
    }
}
