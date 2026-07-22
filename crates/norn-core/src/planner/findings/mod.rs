//! Finding-derived MigrationPlan generation.
//!
//! `plan_from_findings` is the findings intent source of the shared planner. The
//! planner ([`plan_repairs`](crate::standards::plan_repairs)) emits the
//! `MigrationPlan` of typed ops NATIVELY now (ADR 0024 — a repair plan IS a
//! migration plan); this entry point only stamps the run provenance
//! (`generator`/`generated_at`) the pure planner leaves unset, and forwards the
//! rich skip detail for the report. There is no `RepairPlan`→`MigrationPlan` serde
//! round trip — repair constructs the wire ops at each planning site.

use crate::domain::GraphIndex;
use crate::standards::{plan_repairs, Finding, RepairConfig, RepairPlanFilters, RepairPlanResult};
use camino::Utf8PathBuf;

/// The findings-intent-source entry point of the shared planner — the counterpart
/// to `planner::intent::expand`. Runs [`plan_repairs`](crate::standards::plan_repairs)
/// (which builds the wire ops directly) and stamps the `generator`/`generated_at`
/// provenance the pure planner leaves unset. Returns the full
/// [`RepairPlanResult`] so the `repair` verb can surface the rich skip detail on
/// its report.
///
/// The `repair` VERB's findings→plan entry point (`crate::read::repair`); the
/// intent expanders are the other intent source, live via the executor.
pub(crate) fn plan_from_findings(
    vault_root: Utf8PathBuf,
    filters: RepairPlanFilters,
    findings: Vec<Finding>,
    config: &RepairConfig,
    index: &GraphIndex,
) -> RepairPlanResult {
    let mut result = plan_repairs(vault_root, filters, findings, config, index);

    // `generated_at` is non-load-bearing provenance metadata. Read the wall clock
    // via `SystemTime` and format through chrono's timestamp constructor so
    // norn-core does not need chrono's ambient `clock` feature (`Utc::now`) —
    // keeping the dependency surface minimal (matches the telemetry `Clock` seam).
    let generated_at = {
        let since_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        chrono::DateTime::<chrono::Utc>::from_timestamp(
            since_epoch.as_secs() as i64,
            since_epoch.subsec_nanos(),
        )
        .unwrap_or_default()
        .to_rfc3339()
    };

    result.plan.generator = Some("norn-repair".to_string());
    result.plan.generated_at = Some(generated_at);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Link, LinkKind, LinkStatus, UnresolvedReason};
    use crate::standards::Finding;
    use norn_wire::MIGRATION_PLAN_SCHEMA_VERSION;

    fn vault_root() -> Utf8PathBuf {
        "/vault".into()
    }

    /// Build a minimal GraphIndex with the given (path, stem) pairs.
    fn index_with_stems(pairs: &[(&str, &str)]) -> GraphIndex {
        let documents = pairs
            .iter()
            .map(|(path, stem)| crate::domain::Document {
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
        Finding::frontmatter_disallowed_value(
            path.into(),
            Some("test-rule".into()),
            field.into(),
            value,
            vec![serde_json::json!("allowed")],
        )
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
        )
        .plan;

        assert_eq!(plan.schema_version, MIGRATION_PLAN_SCHEMA_VERSION);
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
        )
        .plan;

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
        )
        .plan;

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
        )
        .plan;

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
        )
        .plan;

        assert_eq!(plan.schema_version, MIGRATION_PLAN_SCHEMA_VERSION);
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
        )
        .plan;

        assert_eq!(plan.operations.len(), 0);
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].finding_code, "value-not-allowed");
        assert_eq!(plan.skipped[0].path, "task.md");
    }
}
