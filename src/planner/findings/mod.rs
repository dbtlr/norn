//! Finding-derived MigrationPlan generation.
//!
//! `plan_from_findings` is the entry point for the findings intent source of
//! the shared planner. It adapts today's `plan_repairs` output (a `RepairPlan`)
//! into the unified `MigrationPlan` format.

use crate::core::GraphIndex;
use crate::migration_plan::MIGRATION_PLAN_SCHEMA_VERSION;
use crate::migration_plan::{MigrationOp, MigrationPlan, SkippedFinding};
use crate::standards::{
    plan_repairs, Confidence, Finding, FootnoteDetails, RepairConfig, RepairPlanFilters,
};
use camino::Utf8PathBuf;
use std::collections::HashMap;

/// Adapter: runs `plan_repairs` and converts the resulting `RepairPlan` into
/// a `MigrationPlan`. This is the findings-intent-source entry point of the
/// shared planner — the counterpart to `planner::intent::expand`.
///
/// Footnotes are mapped per-op by `change_id`. Any footnote whose `change_id`
/// matches a `PlannedChange` is attached as the `MigrationOp.footnote` for
/// that op. Because today's `RepairPlan` only ever emits
/// `ClosestMatchSuggestion` footnotes (one per change), there is a 1:1 mapping
/// and no ambiguity. If multiple footnotes share a `change_id` in a future
/// schema, only the last one wins — that is noted in the design archive.
pub(crate) fn plan_from_findings(
    vault_root: Utf8PathBuf,
    filters: RepairPlanFilters,
    findings: Vec<Finding>,
    config: &RepairConfig,
    index: &GraphIndex,
) -> MigrationPlan {
    let repair_plan = plan_repairs(vault_root.clone(), filters, findings, config, index);

    // Build a change_id → footnote description map for per-op attachment.
    let footnote_by_change_id: HashMap<String, String> = repair_plan
        .footnotes
        .iter()
        .map(|f| {
            let desc = match &f.details {
                FootnoteDetails::ClosestMatch(d) => {
                    let confidence_label = match f.confidence {
                        Confidence::High => "high",
                        Confidence::Medium => "medium",
                    };
                    format!(
                        "closest-match suggestion (confidence: {}): \"{}\" → \"{}\" (edit distance: {})",
                        confidence_label,
                        d.original_target,
                        d.candidate_stem,
                        d.normalized_distance,
                    )
                }
            };
            (f.change_id.clone(), desc)
        })
        .collect();

    // Convert each PlannedChange → MigrationOp.
    let operations: Vec<MigrationOp> = repair_plan
        .changes
        .into_iter()
        .map(|change| {
            let footnote = footnote_by_change_id.get(&change.change_id).cloned();
            let kind = change.operation.clone();

            // Serialize the full PlannedChange into fields, then remove the
            // `operation` key since it becomes `MigrationOp.kind`.
            let mut fields =
                serde_json::to_value(&change).expect("PlannedChange must always serialize");
            if let Some(obj) = fields.as_object_mut() {
                obj.remove("operation");

                // `move_document` ops must speak the unified planner vocabulary
                // (`src`/`dst`) so they apply through `norn apply`. The repair
                // PlannedChange uses `path`/`destination` (Plan Task 16 renamed
                // the intent-source path); remap here so the findings-source and
                // intent-source converge on the same on-disk op shape. The
                // applier's `expand` move_document arm recomputes link_risk/hash,
                // so the leftover repair-specific keys are harmless passengers.
                if kind == "move_document" {
                    if let Some(path) = obj.remove("path") {
                        obj.insert("src".to_string(), path);
                    }
                    if let Some(dest) = obj.remove("destination") {
                        obj.insert("dst".to_string(), dest);
                    }
                }
            }

            MigrationOp {
                kind,
                id: None,
                requires: vec![],
                fields,
                footnote,
            }
        })
        .collect();

    // Convert repair::SkippedFinding → migration_plan::SkippedFinding.
    //
    // `reason` carries the kebab-case skip-reason CODE (e.g. "missing-default"),
    // not the prose. The CLI's `--skip-reason` filter matches against this code,
    // and the report renderer derives human prose from it via
    // `repair::skip_reasons::prose_for`. `finding_code` carries the underlying
    // validation finding code (e.g. "link-target-missing").
    let skipped: Vec<SkippedFinding> = repair_plan
        .skipped_findings
        .into_iter()
        .map(|sf| SkippedFinding {
            finding_code: sf.code,
            path: sf.path.to_string(),
            reason: sf.skip_reason.code().to_string(),
            footnote: None,
        })
        .collect();

    let generated_at = chrono::Utc::now().to_rfc3339();

    MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.to_string(),
        generator: Some("norn-repair".to_string()),
        generated_at: Some(generated_at),
        operations,
        skipped,
        plan_footnote: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Link, LinkKind, LinkStatus, Severity, UnresolvedReason};
    use crate::standards::{Finding, FindingBody};

    fn vault_root() -> Utf8PathBuf {
        "/vault".into()
    }

    /// Build a minimal GraphIndex with the given (path, stem) pairs.
    fn index_with_stems(pairs: &[(&str, &str)]) -> GraphIndex {
        let documents = pairs
            .iter()
            .map(|(path, stem)| crate::core::Document {
                path: (*path).into(),
                stem: stem.to_string(),
                hash: format!("hash-{path}"),
                frontmatter: None,
                body_text: String::new(),
                headings: vec![],
                block_ids: vec![],
                links: vec![],
                diagnostics: vec![],
                aliases: vec![],
                alias_malformed: vec![],
            })
            .collect();
        GraphIndex {
            root: vault_root(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    fn finding_link_unresolved(path: &str, target: &str) -> Finding {
        let link = Link {
            source_path: path.into(),
            raw: format!("[[{target}]]"),
            kind: LinkKind::Wikilink,
            target: target.into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: None,
            unresolved_reason: Some(UnresolvedReason::TargetMissing),
            candidates: vec![],
            status: LinkStatus::Unresolved,
        };
        Finding::from_link(path.into(), link)
    }

    fn finding_disallowed_value(path: &str, field: &str, value: serde_json::Value) -> Finding {
        Finding {
            code: "value-not-allowed".into(),
            severity: Severity::Warning,
            path: path.into(),
            message: format!("frontmatter field has a disallowed value: {field}"),
            body: FindingBody::DisallowedValue {
                rule: Some("test-rule".into()),
                field: field.into(),
                actual_value: value,
                allowed_values: vec![serde_json::json!("allowed")],
            },
        }
    }

    #[test]
    fn plan_from_findings_produces_migration_plan_with_generator_set() {
        // A "Norn Brand" link with slug-normalizable target → closest-match → rewrite_link op.
        let finding = finding_link_unresolved("source.md", "Norn Brand");
        let index = index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);
        let repair_config = RepairConfig::default();

        let plan = plan_from_findings(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &repair_config,
            &index,
        );

        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.generator.as_deref(), Some("norn-repair"));
        assert!(plan.generated_at.is_some());
        // Closest-match should produce exactly one op.
        assert!(!plan.operations.is_empty() || !plan.skipped.is_empty());
    }

    #[test]
    fn plan_from_findings_closest_match_op_has_correct_kind_and_fields() {
        let finding = finding_link_unresolved("source.md", "Norn Brand");
        let index = index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);
        let repair_config = RepairConfig::default();

        let plan = plan_from_findings(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &repair_config,
            &index,
        );

        assert_eq!(plan.operations.len(), 1);
        let op = &plan.operations[0];
        assert_eq!(op.kind, "rewrite_link");
        // fields must not contain an "operation" key — it was promoted to kind.
        assert!(
            op.fields.get("operation").is_none(),
            "fields must not contain 'operation' after stripping; fields={:?}",
            op.fields
        );
        // fields must contain the change metadata.
        assert!(
            op.fields.get("change_id").is_some(),
            "fields must carry change_id"
        );
    }

    #[test]
    fn plan_from_findings_closest_match_op_carries_footnote() {
        let finding = finding_link_unresolved("source.md", "Norn Brand");
        let index = index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);
        let repair_config = RepairConfig::default();

        let plan = plan_from_findings(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &repair_config,
            &index,
        );

        assert_eq!(plan.operations.len(), 1);
        let op = &plan.operations[0];
        assert!(
            op.footnote.is_some(),
            "closest-match rewrite_link op must carry a footnote"
        );
        let note = op.footnote.as_ref().unwrap();
        assert!(
            note.contains("closest-match"),
            "footnote must describe the closest-match suggestion; got: {}",
            note
        );
        assert!(
            note.contains("Norn Brand"),
            "footnote must reference original target; got: {}",
            note
        );
        assert!(
            note.contains("norn-brand"),
            "footnote must reference candidate stem; got: {}",
            note
        );
    }

    #[test]
    fn plan_from_findings_skipped_finding_maps_to_migration_skipped() {
        // An unresolved link with no closest-match candidate → skipped in RepairPlan.
        let finding = finding_link_unresolved("source.md", "xyzzy-zzz-completely-unknown");
        let index = index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);
        let repair_config = RepairConfig::default();

        let plan = plan_from_findings(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &repair_config,
            &index,
        );

        assert_eq!(plan.operations.len(), 0);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].path, "source.md");
        assert!(!plan.skipped[0].reason.is_empty());
        assert!(!plan.skipped[0].finding_code.is_empty());
    }

    #[test]
    fn plan_from_findings_empty_findings_produces_empty_plan() {
        let index = index_with_stems(&[("source.md", "source")]);
        let repair_config = RepairConfig::default();

        let plan = plan_from_findings(
            vault_root(),
            RepairPlanFilters::default(),
            vec![],
            &repair_config,
            &index,
        );

        assert_eq!(plan.schema_version, 1);
        assert_eq!(plan.generator.as_deref(), Some("norn-repair"));
        assert!(plan.generated_at.is_some());
        assert!(plan.operations.is_empty());
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn plan_from_findings_skipped_no_rule_matched_finding_maps_correctly() {
        // A disallowed-value finding with no repair rules → skipped.
        let finding = finding_disallowed_value("task.md", "status", serde_json::json!("someday"));
        let index = index_with_stems(&[("task.md", "task")]);
        let repair_config = RepairConfig::default(); // no rules

        let plan = plan_from_findings(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &repair_config,
            &index,
        );

        assert_eq!(plan.operations.len(), 0);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].finding_code, "value-not-allowed");
        assert_eq!(plan.skipped[0].path, "task.md");
    }
}
