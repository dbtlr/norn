//! ApplyReport — the unified output envelope for migrate, move, delete,
//! rewrite-wikilink, and future new/set conversions.
//!
//! Replaces MoveReport, DeleteReport, RepairApplyReport.

use serde::{Deserialize, Serialize};

pub const APPLY_REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub schema_version: u32,
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
            schema_version: 1,
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
}
