use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use vault_frontmatter::{
    extract_frontmatter, serialize_value_preserving_style, top_level_property_spans, QuoteError,
};

use crate::findings::Finding;
use crate::repair::{PlannedChange, RepairPlan};
use crate::summarize;
use crate::summary::Summary;

const REPAIR_PLAN_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("unsupported repair plan schema version: expected {expected}, got {got}")]
    UnsupportedSchemaVersion { expected: u32, got: u32 },

    #[error("repair plan vault root does not match effective cwd: plan {plan}, cwd {cwd}")]
    VaultRootMismatch {
        plan: Utf8PathBuf,
        cwd: Utf8PathBuf,
    },

    #[error("repair plan targets a document not in the index: {path}")]
    UnknownPath { path: Utf8PathBuf },

    #[error("stale repair plan for {path}: expected hash {expected}, found {actual}")]
    StaleDocumentHash {
        path: Utf8PathBuf,
        expected: String,
        actual: String,
    },

    #[error("repair plan contains conflicting changes for {path} field {field}")]
    ConflictingFieldChange { path: Utf8PathBuf, field: String },

    #[error("repair plan contains conflicting document hash preconditions for {path}")]
    ConflictingHashes { path: Utf8PathBuf },

    #[error("stale repair plan for {path} field {field}: expected {expected}, found {actual}")]
    ExpectedOldValueMismatch {
        path: Utf8PathBuf,
        field: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported repair operation for {path}: {operation}")]
    UnsupportedOperation {
        path: Utf8PathBuf,
        operation: String,
    },

    #[error("cannot minimal-edit frontmatter for {path}: {reason}")]
    CannotMinimalEdit { path: Utf8PathBuf, reason: String },

    #[error("frontmatter parse failed for {path}: {message}")]
    FrontmatterParseFailed {
        path: Utf8PathBuf,
        message: String,
    },

    #[error("set_frontmatter change missing new_value for {path}")]
    MissingNewValue { path: Utf8PathBuf },
}

#[derive(Debug, Serialize)]
pub struct RepairApplyReport {
    pub schema_version: u32,
    pub dry_run: bool,
    pub changed_files: Vec<Utf8PathBuf>,
    pub applied_changes: usize,
    pub plan_context: RepairApplyPlanContext,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<RepairApplyVerification>,
}

#[derive(Debug, Serialize)]
pub struct RepairApplyPlanContext {
    pub skipped_findings: usize,
    pub unsupported_findings: usize,
    pub ambiguous_findings: usize,
}

#[derive(Debug, Serialize)]
pub struct RepairApplyVerification {
    pub remaining_findings: usize,
    pub summary: Summary,
}

impl RepairApplyReport {
    pub fn new(plan: &RepairPlan, dry_run: bool) -> Self {
        Self {
            schema_version: plan.schema_version,
            dry_run,
            changed_files: Vec::new(),
            applied_changes: plan.changes.len(),
            plan_context: RepairApplyPlanContext {
                skipped_findings: plan.skipped_findings.len(),
                unsupported_findings: plan.unsupported_findings.len(),
                ambiguous_findings: plan.ambiguous_findings.len(),
            },
            verification: None,
        }
    }

    pub fn with_verification(mut self, findings: &[Finding]) -> Self {
        let summary = summarize(findings);
        self.verification = Some(RepairApplyVerification {
            remaining_findings: summary.findings,
            summary,
        });
        self
    }
}

pub fn validate_plan_for_apply(cwd: &Utf8PathBuf, plan: &RepairPlan) -> Result<(), ApplyError> {
    if plan.schema_version != REPAIR_PLAN_SCHEMA_VERSION {
        return Err(ApplyError::UnsupportedSchemaVersion {
            expected: REPAIR_PLAN_SCHEMA_VERSION,
            got: plan.schema_version,
        });
    }
    if &plan.vault_root != cwd {
        return Err(ApplyError::VaultRootMismatch {
            plan: plan.vault_root.clone(),
            cwd: cwd.clone(),
        });
    }
    Ok(())
}

pub fn changes_by_path(
    plan: &RepairPlan,
) -> Result<BTreeMap<Utf8PathBuf, Vec<&PlannedChange>>, ApplyError> {
    let mut grouped: BTreeMap<Utf8PathBuf, Vec<&PlannedChange>> = BTreeMap::new();
    let mut seen_fields = BTreeSet::new();

    for change in &plan.changes {
        if !matches!(
            change.operation.as_str(),
            "set_frontmatter" | "remove_frontmatter"
        ) {
            return Err(ApplyError::UnsupportedOperation {
                path: change.path.clone(),
                operation: change.operation.clone(),
            });
        }
        let key = (change.path.clone(), change.field.clone());
        if !seen_fields.insert(key) {
            return Err(ApplyError::ConflictingFieldChange {
                path: change.path.clone(),
                field: change.field.clone(),
            });
        }
        grouped
            .entry(change.path.clone())
            .or_default()
            .push(change);
    }

    for (path, changes) in &grouped {
        let hash = &changes[0].document_hash;
        if changes.iter().any(|change| &change.document_hash != hash) {
            return Err(ApplyError::ConflictingHashes { path: path.clone() });
        }
    }

    Ok(grouped)
}

pub fn apply_file_changes(
    content: &str,
    changes: &[&PlannedChange],
) -> Result<String, ApplyError> {
    let path = if let Some(change) = changes.first() {
        change.path.clone()
    } else {
        return Ok(content.to_string());
    };

    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    let Some(frontmatter_range) = frontmatter_range else {
        return Err(ApplyError::CannotMinimalEdit {
            path,
            reason: "document has no frontmatter".into(),
        });
    };
    if !diagnostics.is_empty() {
        return Err(ApplyError::FrontmatterParseFailed {
            path,
            message: diagnostics
                .iter()
                .map(|d| d.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let Some(frontmatter_value) = frontmatter else {
        return Err(ApplyError::FrontmatterParseFailed {
            path,
            message: "frontmatter could not be parsed".into(),
        });
    };
    let Some(current_object) = frontmatter_value.as_object() else {
        return Err(ApplyError::CannotMinimalEdit {
            path,
            reason: "frontmatter is not a top-level mapping".into(),
        });
    };

    let spans = top_level_property_spans(content, frontmatter_range.clone());

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();

    for change in changes {
        let current_value = current_object.get(&change.field);
        check_expected_old_value(&path, &change.field, &change.expected_old_value, current_value)?;

        let span = spans.iter().find(|s| s.name == change.field);

        match change.operation.as_str() {
            "set_frontmatter" => {
                let Some(span) = span else {
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: format!("field {} not present in frontmatter", change.field),
                    });
                };
                let Some(value_range) = span.value_range.clone() else {
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: format!(
                            "field {} has style {:?}; set_frontmatter requires a scalar value",
                            change.field, span.style
                        ),
                    });
                };
                let new_value = change
                    .new_value
                    .as_ref()
                    .ok_or_else(|| ApplyError::MissingNewValue { path: path.clone() })?;
                let replacement = serialize_value_preserving_style(new_value, span.style)
                    .map_err(|e| match e {
                        QuoteError::StructuredOriginalStyle(_) | QuoteError::NonScalarValue => {
                            ApplyError::CannotMinimalEdit {
                                path: path.clone(),
                                reason: e.to_string(),
                            }
                        }
                        QuoteError::Unrepresentable { .. } => ApplyError::CannotMinimalEdit {
                            path: path.clone(),
                            reason: e.to_string(),
                        },
                    })?;
                edits.push((value_range, replacement));
            }
            "remove_frontmatter" => {
                let Some(span) = span else {
                    return Err(ApplyError::CannotMinimalEdit {
                        path: path.clone(),
                        reason: format!("field {} not present in frontmatter", change.field),
                    });
                };
                edits.push((span.line_range.clone(), String::new()));
            }
            other => {
                return Err(ApplyError::UnsupportedOperation {
                    path: path.clone(),
                    operation: other.to_string(),
                });
            }
        }
    }

    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = content.to_string();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}

fn check_expected_old_value(
    path: &Utf8PathBuf,
    field: &str,
    expected: &Option<Value>,
    actual: Option<&Value>,
) -> Result<(), ApplyError> {
    match (expected, actual) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (None, None) => Ok(()),
        (None, Some(Value::Null)) => Ok(()),
        (Some(expected), Some(actual)) => Err(ApplyError::ExpectedOldValueMismatch {
            path: path.clone(),
            field: field.to_string(),
            expected: format!("{expected}"),
            actual: format!("{actual}"),
        }),
        (Some(expected), None) => Err(ApplyError::ExpectedOldValueMismatch {
            path: path.clone(),
            field: field.to_string(),
            expected: format!("{expected}"),
            actual: "missing".to_string(),
        }),
        (None, Some(actual)) => Err(ApplyError::ExpectedOldValueMismatch {
            path: path.clone(),
            field: field.to_string(),
            expected: "missing".to_string(),
            actual: format!("{actual}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repair::{RepairPlanFilters, RepairPlanSummary};
    use serde_json::json;

    fn empty_plan(schema_version: u32, vault_root: &str) -> RepairPlan {
        RepairPlan {
            schema_version,
            vault_root: vault_root.into(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 0,
                planned_changes: 0,
                skipped_findings: 0,
                unsupported_findings: 0,
                ambiguous_findings: 0,
            },
            changes: vec![],
            skipped_findings: vec![],
            unsupported_findings: vec![],
            ambiguous_findings: vec![],
        }
    }

    fn make_change(
        path: &str,
        field: &str,
        hash: &str,
        operation: &str,
        new_value: Option<Value>,
    ) -> PlannedChange {
        PlannedChange {
            path: path.into(),
            document_hash: hash.to_string(),
            finding_code: "frontmatter-disallowed-value".into(),
            finding_rule: None,
            repair_rule: "test".into(),
            operation: operation.to_string(),
            field: field.to_string(),
            expected_old_value: None,
            new_value,
        }
    }

    #[test]
    fn validate_plan_rejects_unsupported_schema_version() {
        let plan = empty_plan(99, "/vault");
        let err = validate_plan_for_apply(&"/vault".into(), &plan).unwrap_err();
        assert!(matches!(
            err,
            ApplyError::UnsupportedSchemaVersion {
                expected: 2,
                got: 99
            }
        ));
    }

    #[test]
    fn validate_plan_rejects_vault_root_mismatch() {
        let plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/other");
        let err = validate_plan_for_apply(&"/vault".into(), &plan).unwrap_err();
        assert!(matches!(err, ApplyError::VaultRootMismatch { .. }));
    }

    #[test]
    fn validate_plan_accepts_matching_schema_and_root() {
        let plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        validate_plan_for_apply(&"/vault".into(), &plan).unwrap();
    }

    #[test]
    fn changes_by_path_groups_by_path() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![
            make_change("a.md", "status", "h1", "set_frontmatter", Some(json!("done"))),
            make_change("a.md", "kind", "h1", "remove_frontmatter", None),
            make_change("b.md", "status", "h2", "set_frontmatter", Some(json!("done"))),
        ];
        let grouped = changes_by_path(&plan).unwrap();
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&Utf8PathBuf::from("a.md")].len(), 2);
        assert_eq!(grouped[&Utf8PathBuf::from("b.md")].len(), 1);
    }

    #[test]
    fn changes_by_path_rejects_conflicting_field_changes() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![
            make_change("a.md", "status", "h1", "set_frontmatter", Some(json!("done"))),
            make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("backlog")),
            ),
        ];
        let err = changes_by_path(&plan).unwrap_err();
        assert!(matches!(err, ApplyError::ConflictingFieldChange { .. }));
    }

    #[test]
    fn changes_by_path_rejects_conflicting_hashes_for_same_path() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![
            make_change("a.md", "status", "h1", "set_frontmatter", Some(json!("done"))),
            make_change("a.md", "kind", "h2", "remove_frontmatter", None),
        ];
        let err = changes_by_path(&plan).unwrap_err();
        assert!(matches!(err, ApplyError::ConflictingHashes { .. }));
    }

    #[test]
    fn changes_by_path_rejects_unsupported_operation() {
        let mut plan = empty_plan(REPAIR_PLAN_SCHEMA_VERSION, "/vault");
        plan.changes = vec![make_change("a.md", "status", "h1", "rename_file", None)];
        let err = changes_by_path(&plan).unwrap_err();
        assert!(matches!(err, ApplyError::UnsupportedOperation { .. }));
    }

    fn apply_change(content: &str, change: &PlannedChange) -> Result<String, ApplyError> {
        apply_file_changes(content, &[change])
    }

    #[test]
    fn set_frontmatter_replaces_plain_scalar_value() {
        let content = "---\nstatus: someday\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("completed")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("completed")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nstatus: completed\n---\n# body\n");
    }

    #[test]
    fn set_frontmatter_preserves_double_quoted_style() {
        let content = "---\nworkspace: \"[[vault-cli]]\"\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("[[vault-cli]]")),
            new_value: Some(json!("[[other]]")),
            ..make_change(
                "a.md",
                "workspace",
                "h1",
                "set_frontmatter",
                Some(json!("[[other]]")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nworkspace: \"[[other]]\"\n---\n# body\n");
    }

    #[test]
    fn set_frontmatter_preserves_single_quoted_style() {
        let content = "---\nworkspace: '[[vault-cli]]'\n---\n# body\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("[[vault-cli]]")),
            new_value: Some(json!("[[other]]")),
            ..make_change(
                "a.md",
                "workspace",
                "h1",
                "set_frontmatter",
                Some(json!("[[other]]")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nworkspace: '[[other]]'\n---\n# body\n");
    }

    #[test]
    fn set_frontmatter_preserves_same_line_comment() {
        let content = "---\nstatus: someday  # legacy\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("completed")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("completed")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\nstatus: completed  # legacy\n---\n");
    }

    #[test]
    fn remove_frontmatter_deletes_full_line() {
        let content = "---\ntitle: hi\nkind: legacy\nstatus: done\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("legacy")),
            ..make_change("a.md", "kind", "h1", "remove_frontmatter", None)
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: hi\nstatus: done\n---\n");
    }

    #[test]
    fn remove_frontmatter_can_delete_block_value_lines() {
        let content = "---\ntitle: hi\naliases:\n  - one\n  - two\nstatus: done\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["one", "two"])),
            ..make_change("a.md", "aliases", "h1", "remove_frontmatter", None)
        };
        let result = apply_change(content, &change).unwrap();
        assert_eq!(result, "---\ntitle: hi\nstatus: done\n---\n");
    }

    #[test]
    fn set_frontmatter_rejects_block_sequence_target() {
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!(["one", "two"])),
            ..make_change(
                "a.md",
                "aliases",
                "h1",
                "set_frontmatter",
                Some(json!("one")),
            )
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(matches!(err, ApplyError::CannotMinimalEdit { .. }));
    }

    #[test]
    fn apply_rejects_expected_old_value_mismatch() {
        let content = "---\nstatus: completed\n---\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("backlog")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("backlog")),
            )
        };
        let err = apply_change(content, &change).unwrap_err();
        assert!(matches!(err, ApplyError::ExpectedOldValueMismatch { .. }));
    }

    #[test]
    fn apply_treats_yaml_null_as_absent_for_expected_old_value() {
        let content = "---\nstatus: ~\n---\n";
        let change = PlannedChange {
            expected_old_value: None,
            new_value: Some(json!("backlog")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("backlog")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        assert!(result.contains("status: backlog"));
    }

    #[test]
    fn apply_preserves_markdown_body_exactly() {
        let content =
            "---\nstatus: someday\n---\n# Heading\n\nParagraph with `code` and **bold**.\n";
        let change = PlannedChange {
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("completed")),
            ..make_change(
                "a.md",
                "status",
                "h1",
                "set_frontmatter",
                Some(json!("completed")),
            )
        };
        let result = apply_change(content, &change).unwrap();
        let body_start = result.find("# Heading").unwrap();
        assert_eq!(
            &result[body_start..],
            "# Heading\n\nParagraph with `code` and **bold**.\n"
        );
    }

    #[test]
    fn apply_returns_cannot_minimal_edit_for_missing_field() {
        let content = "---\ntitle: hi\n---\n";
        let change = make_change("a.md", "status", "h1", "remove_frontmatter", None);
        let err = apply_change(content, &change).unwrap_err();
        assert!(matches!(err, ApplyError::CannotMinimalEdit { .. }));
    }
}
