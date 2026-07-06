//! Unified applier: planner expansion pre-pass + delegation to today's
//! pass-based apply_repair_plan_with_context.
//!
//! This module is the integration point that wires the MigrationPlan →
//! PlannedChange expansion to the existing pass-based apply orchestrator
//! (repair_apply.rs). Every document-mutation command (move, delete,
//! rewrite-wikilink, migrate) builds a MigrationPlan and applies it here,
//! emitting a single ApplyReport envelope.
//!
//! # Provenance tracking
//!
//! Each PlannedChange carries a `parent_op_idx` (index into
//! `plan.operations`) so the ApplyReport can:
//! - set `from = Some(parent_idx.to_string())` for changes produced by
//!   high-level expansions (move_folder → N move_document ops)
//! - propagate the parent MigrationOp's `footnote` to each child ApplyReportOp

use crate::apply_report::{
    ApplyReport, ApplyReportOp, ApplyWarning, CascadeFailure, CascadeRewrite, CascadeSkip,
    CascadeSummary, OpStatus, APPLY_REPORT_SCHEMA_VERSION,
};
use crate::core::GraphIndex;
use crate::migration_plan::{MigrationOp, MigrationPlan};
use crate::planner::intent::{expand, HIGH_LEVEL_KINDS};
use crate::repair_apply::{apply_repair_plan_with_context, CreateApplyContext};
use crate::standards::apply::CascadeRecord;
use crate::standards::apply::RepairApplyReport;
use crate::standards::{
    PlanWarning, PlannedChange, RepairPlan, RepairPlanFilters, RepairPlanSummary, SkippedSummary,
    REPAIR_PLAN_SCHEMA_VERSION,
};
use crate::telemetry::event::{
    action_event_name, ATTR_LINK_FROM, ATTR_LINK_TO, ATTR_REASON_CODE, ATTR_REASON_MESSAGE,
    ATTR_STATUS, ATTR_TARGET,
};
use crate::telemetry::Event;
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeSet;

/// Context for `apply_migration_plan`.
pub(crate) struct ApplyContext {
    /// When true, no filesystem mutations are made; report shows what would happen.
    pub dry_run: bool,
    /// When true, create intermediate parent directories for create_document ops.
    pub parents: bool,
    /// When true, per-op cascade summaries include the full rewrite/skip lists.
    pub verbose: bool,
}

/// Apply a `MigrationPlan` against an in-memory `GraphIndex`, delegating to the
/// existing pass-based apply orchestrator.
///
/// # Phase 1 — Expansion
///
/// Each `MigrationOp` in `plan.operations` is expanded via
/// `planner::intent::expand`. High-level ops (e.g. `move_folder`) expand to N
/// `PlannedChange`s; low-level ops expand to exactly one. Provenance is
/// tracked so the report can surface which parent op each change came from.
///
/// # Phase 2 — Hash hydration
///
/// The intent expander sets `document_hash = ""` for operator-originated
/// move/delete ops (it has no index at that layer). Before delegating to the
/// existing apply orchestrator (which hash-checks delete/rewrite/frontmatter ops),
/// we fill in the real hash from the index for any change that has an empty hash
/// and whose operation is hash-checked (delete_document, rewrite_link,
/// replace_body, set/add/remove_frontmatter).
///
/// move_document hashes are NOT checked by the existing orchestrator, so an
/// empty hash there is fine.
///
/// # Phase 3 — Delegation
///
/// A synthetic `RepairPlan` is built from the expanded changes and handed to
/// `apply_repair_plan_with_context`. That function owns all the pass sequencing.
///
/// # Phase 4 — Conversion
///
/// The `RepairApplyReport` is converted to an `ApplyReport` with per-op status,
/// provenance (`from`), footnote propagation, and summary lines.
pub(crate) fn apply_migration_plan(
    plan: &MigrationPlan,
    index: &GraphIndex,
    ctx: ApplyContext,
    sink: &mut crate::telemetry::EventSink,
) -> Result<ApplyReport> {
    // ------------------------------------------------------------------
    // Phase 1: expansion + provenance tracking
    // ------------------------------------------------------------------

    // `all_changes[i]` came from `plan.operations[provenance[i]]`.
    let mut all_changes: Vec<PlannedChange> = Vec::new();
    let mut provenance: Vec<usize> = Vec::new(); // change idx → parent op idx

    for (i, op) in plan.operations.iter().enumerate() {
        let expanded = expand(op, index)?;
        for c in expanded {
            provenance.push(i);
            all_changes.push(c);
        }
    }

    // Emit one `op_planned` per expanded change, collecting the returned span
    // ids into a parallel vec (indexed like `all_changes`). `from` references
    // the parent op index when that parent is a high-level (multi-expansion)
    // op; otherwise `None`.
    let span_ids: Vec<String> = all_changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            let parent_idx = provenance[i];
            let from = if is_high_level_op(&plan.operations[parent_idx]) {
                Some(parent_idx)
            } else {
                None
            };
            sink.start_op(&change.operation, change.path.as_str(), from)
        })
        .collect();

    // change_id -> op span id. `hydrated` is `all_changes` mapped 1:1 (same
    // length and order — hydration only fills empty hashes, never adds/removes
    // changes), so its `change_id`s align with `span_ids` built from
    // `all_changes`. We zip `all_changes` (the span_ids source) with span_ids.
    let spans: std::collections::HashMap<String, String> = all_changes
        .iter()
        .zip(span_ids.iter())
        .map(|(c, s)| (c.change_id.clone(), s.clone()))
        .collect();

    // ------------------------------------------------------------------
    // Phase 2: hash hydration
    // ------------------------------------------------------------------
    // The intent expander emits empty document_hash for move_document and
    // delete_document (operator-driven ops have no hash at expansion time).
    // The apply orchestrator hash-checks delete_document, rewrite_link,
    // replace_body, and frontmatter changes — fill those in from the index.

    let index_hashes: std::collections::BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();

    let hydrated: Vec<PlannedChange> = all_changes
        .iter()
        .map(|c| {
            if c.document_hash.is_empty() && needs_hash_check(&c.operation) {
                if let Some(hash) = index_hashes.get(&c.path) {
                    let mut c2 = c.clone();
                    c2.document_hash = hash.clone();
                    return c2;
                }
            }
            c.clone()
        })
        .collect();

    // ------------------------------------------------------------------
    // Phase 3: delegation to today's applier
    // ------------------------------------------------------------------

    let vault_root = Utf8PathBuf::from(&plan.vault_root);
    let repair_plan = RepairPlan {
        schema_version: REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.clone(),
        source_filters: RepairPlanFilters::default(),
        summary: RepairPlanSummary {
            findings: hydrated.len(),
            planned_changes: hydrated.len(),
            skipped: SkippedSummary::default(),
        },
        changes: hydrated.clone(),
        skipped_findings: Vec::new(),
        footnotes: Vec::new(),
    };

    let create_ctx = CreateApplyContext {
        parents: ctx.parents,
        // NRN-138 ignore re-check applies to `new`-synthesized create_document
        // changes (already guarded at plan time by synth::build_plan); the
        // migration-plan create_document ops routed through here have no such
        // guard to backstop, so leave this empty.
        ..Default::default()
    };

    let apply_result = apply_repair_plan_with_context(
        &vault_root,
        index,
        &repair_plan,
        ctx.dry_run,
        &create_ctx,
        sink,
        &spans,
    )?;

    // ------------------------------------------------------------------
    // Phase 4: convert RepairApplyReport → ApplyReport
    // ------------------------------------------------------------------

    let ops = build_report_ops(
        &hydrated,
        &provenance,
        &plan.operations,
        &apply_result,
        ctx.dry_run,
        ctx.verbose,
        &span_ids,
        sink.events(),
    );

    let applied = ops
        .iter()
        .filter(|o| matches!(o.status, OpStatus::Applied))
        .count();
    let failed = ops
        .iter()
        .filter(|o| matches!(o.status, OpStatus::Failed))
        .count();
    let skipped = ops
        .iter()
        .filter(|o| matches!(o.status, OpStatus::Skipped))
        .count();
    let remaining = ops
        .iter()
        .filter(|o| matches!(o.status, OpStatus::NotRun))
        .count();

    let warnings: Vec<ApplyWarning> = apply_result
        .warnings
        .iter()
        .map(|w| {
            // PlanWarning is a tagged enum; convert to a code+message shape
            // for ApplyWarning.
            let (code, message) = match &w.warning {
                PlanWarning::StemCollisionAfterMove {
                    new_stem,
                    new_path,
                    collides_with,
                } => (
                    "stem_collision_after_move".to_string(),
                    format!(
                        "stem '{}' ({}) collides with: {}",
                        new_stem,
                        new_path,
                        collides_with
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
            };
            ApplyWarning {
                code,
                message,
                path: Some(w.path.to_string()),
            }
        })
        .collect();

    Ok(ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        // Dry-runs persist no log, so the trace_id correlates to nothing — emit
        // empty for symmetry with SetReport/NewReport dry-run output.
        trace_id: if ctx.dry_run {
            String::new()
        } else {
            sink.trace_id().to_string()
        },
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        dry_run: ctx.dry_run,
        applied,
        skipped,
        failed,
        remaining,
        operations: ops,
        warnings,
    })
}

/// Returns true for operation kinds that the existing apply orchestrator
/// hash-checks. Operations not listed here (e.g. move_document, create_document)
/// are not subject to hash checks and can safely have an empty document_hash.
fn needs_hash_check(operation: &str) -> bool {
    matches!(
        operation,
        "delete_document"
            | "rewrite_link"
            | "replace_body"
            | "set_frontmatter"
            | "add_frontmatter"
            | "remove_frontmatter"
            // Section/body edit ops (NRN-98) are hash-checked in Pass 1d2, so an
            // omitted document_hash must be hydrated from the index just like
            // replace_body — otherwise a plan without a hash aborts spuriously.
            | "str_replace"
            | "replace_section"
            | "append_to_section"
            | "delete_section"
            | "insert_before_heading"
            | "insert_after_heading"
    )
}

/// Build a one-liner summary for an `ApplyReportOp`.
///
/// `create_display` overrides the path shown for a `create_document` op — used
/// to surface the apply-time-resolved `{{seq}}` id (NRN-101) instead of the
/// unresolved template `change.path`.
fn build_summary(
    change: &PlannedChange,
    dry_run: bool,
    create_display: Option<&camino::Utf8Path>,
) -> String {
    let prefix = if dry_run { "would " } else { "" };
    match change.operation.as_str() {
        "move_document" => {
            let dst = change
                .destination
                .as_ref()
                .map(|p| p.as_str())
                .unwrap_or("<unknown>");
            format!("{}move {} → {}", prefix, change.path, dst)
        }
        "delete_document" => format!("{}delete {}", prefix, change.path),
        "create_document" => {
            let path = create_display.unwrap_or(change.path.as_path());
            format!("{prefix}create {path}")
        }
        "replace_body" => format!("{}replace body of {}", prefix, change.path),
        "rewrite_link" => {
            let from = change
                .expected_old_value
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let to = change
                .new_value
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            format!(
                "{}rewrite link in {} ({} → {})",
                prefix, change.path, from, to
            )
        }
        "set_frontmatter" | "add_frontmatter" | "remove_frontmatter" => {
            let field = change.field.as_deref().unwrap_or("?");
            format!(
                "{}{} frontmatter field '{}' in {}",
                prefix, change.operation, field, change.path
            )
        }
        other => format!("{}{} {}", prefix, other, change.path),
    }
}

/// Returns true when the parent MigrationOp is a high-level kind (expands to
/// multiple PlannedChanges). Used to set the `from` field in ApplyReportOp.
fn is_high_level_op(op: &MigrationOp) -> bool {
    HIGH_LEVEL_KINDS.contains(&op.kind.as_str())
}

/// Fold a (possibly missing) `CascadeRecord` into a serialized `CascadeSummary`.
/// Counts are always present; the `rewrites`/`skips` lists are populated only
/// when `verbose` is set. A missing record (op had no backlinks) yields an
/// all-zero summary.
fn build_cascade_summary(rec: Option<&CascadeRecord>, verbose: bool) -> CascadeSummary {
    let rec = match rec {
        Some(r) => r,
        None => {
            return CascadeSummary {
                planned: 0,
                applied: 0,
                skipped: 0,
                failed: 0,
                files: 0,
                rewrites: Vec::new(),
                skips: Vec::new(),
                failures: Vec::new(),
            }
        }
    };
    let files: BTreeSet<&Utf8Path> = rec.rewritten.iter().map(|r| r.file.as_path()).collect();
    let rewrites = if verbose {
        rec.rewritten
            .iter()
            .map(|r| CascadeRewrite {
                file: r.file.to_string(),
                from: r.from.clone(),
                to: r.to.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let skips = if verbose {
        rec.skipped
            .iter()
            .map(|s| CascadeSkip {
                file: s.file.to_string(),
                from: s.from.clone(),
                to: s.to.clone(),
                reason: s.reason.code().to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let failures = rec
        .failed
        .iter()
        .map(|f| CascadeFailure {
            file: f.file.to_string(),
            from: f.from.clone(),
            to: f.to.clone(),
            reason: f.reason.code().to_string(),
            detail: if f.detail.is_empty() {
                None
            } else {
                Some(f.detail.clone())
            },
        })
        .collect();
    CascadeSummary {
        planned: rec.planned,
        applied: rec.rewritten.len(),
        skipped: rec.skipped.len(),
        failed: rec.failed.len(),
        files: files.len(),
        rewrites,
        skips,
        failures,
    }
}

/// Find an attribute value on an event by key.
fn attr<'a>(e: &'a Event, key: &str) -> Option<&'a str> {
    e.attributes
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.as_str())
}

/// Fold a cascade summary for a move/delete op out of the in-memory event log.
///
/// Every `norn.action.rewrite_link` event under the op's `span` is a cascade
/// entry (each affected backlink produces exactly one terminal event). We
/// partition them by status into applied / skipped / failed, reproducing the
/// same counts the old `RepairApplyReport`-derived path produced.
fn fold_cascade_from_events(events: &[Event], span: Option<&str>, verbose: bool) -> CascadeSummary {
    let cascade_events: Vec<&Event> = events
        .iter()
        .filter(|e| e.span_id.as_deref() == span && e.name == action_event_name("rewrite_link"))
        .collect();

    let applied_events: Vec<&Event> = cascade_events
        .iter()
        .copied()
        .filter(|e| attr(e, ATTR_STATUS) == Some("applied"))
        .collect();
    let skipped_events: Vec<&Event> = cascade_events
        .iter()
        .copied()
        .filter(|e| attr(e, ATTR_STATUS) == Some("skipped"))
        .collect();
    let failed_events: Vec<&Event> = cascade_events
        .iter()
        .copied()
        .filter(|e| attr(e, ATTR_STATUS) == Some("failed"))
        .collect();

    let files: BTreeSet<&str> = applied_events
        .iter()
        .map(|e| attr(e, ATTR_TARGET).unwrap_or(""))
        .collect();

    let rewrites = if verbose {
        applied_events
            .iter()
            .map(|e| CascadeRewrite {
                file: attr(e, ATTR_TARGET).unwrap_or("").to_string(),
                from: attr(e, ATTR_LINK_FROM).unwrap_or("").to_string(),
                to: attr(e, ATTR_LINK_TO).unwrap_or("").to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let skips = if verbose {
        skipped_events
            .iter()
            .map(|e| CascadeSkip {
                file: attr(e, ATTR_TARGET).unwrap_or("").to_string(),
                from: attr(e, ATTR_LINK_FROM).unwrap_or("").to_string(),
                to: attr(e, ATTR_LINK_TO).unwrap_or("").to_string(),
                reason: attr(e, ATTR_REASON_CODE).unwrap_or("").to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };

    let failures = failed_events
        .iter()
        .map(|e| CascadeFailure {
            file: attr(e, ATTR_TARGET).unwrap_or("").to_string(),
            from: attr(e, ATTR_LINK_FROM).unwrap_or("").to_string(),
            to: attr(e, ATTR_LINK_TO).unwrap_or("").to_string(),
            reason: attr(e, ATTR_REASON_CODE).unwrap_or("").to_string(),
            detail: attr(e, ATTR_REASON_MESSAGE).map(|s| s.to_string()),
        })
        .collect();

    CascadeSummary {
        planned: applied_events.len() + skipped_events.len() + failed_events.len(),
        applied: applied_events.len(),
        skipped: skipped_events.len(),
        failed: failed_events.len(),
        files: files.len(),
        rewrites,
        skips,
        failures,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_report_ops(
    changes: &[PlannedChange],
    provenance: &[usize],
    plan_ops: &[MigrationOp],
    apply_result: &RepairApplyReport, // still used for the dry-run cascade forecast
    dry_run: bool,
    verbose: bool,
    span_ids: &[String],
    events: &[Event],
) -> Vec<ApplyReportOp> {
    // NRN-101: create_document ops are recorded in `created_documents` in the
    // same order they appear here, each carrying its apply-time-resolved
    // `{{seq}}` path. Walk that list in lockstep so summaries show the real
    // (or dry-run predicted) id, not the unresolved template.
    let mut created_iter = apply_result.created_documents.iter();
    changes
        .iter()
        .enumerate()
        .map(move |(i, change)| {
            let parent_idx = provenance[i];
            let parent_op = &plan_ops[parent_idx];

            // "from" is set when the parent is a high-level op that expanded
            // into multiple changes. For 1:1 (low-level) ops, `from` is None.
            let from = if is_high_level_op(parent_op) {
                Some(parent_idx.to_string())
            } else {
                None
            };

            let span = span_ids.get(i).map(|s| s.as_str());

            let status = if dry_run {
                OpStatus::NotRun
            } else {
                // An op is Applied iff an op-action event for THIS op's kind
                // exists under its span with status "applied"; otherwise
                // Skipped (processed, no realization) — matching the previous
                // infer_status semantics.
                let op_action_name = action_event_name(&change.operation);
                let applied = span.is_some()
                    && events.iter().any(|e| {
                        e.span_id.as_deref() == span
                            && e.name == op_action_name
                            && attr(e, ATTR_STATUS) == Some("applied")
                    });
                if applied {
                    OpStatus::Applied
                } else {
                    OpStatus::Skipped
                }
            };

            let create_display = if change.operation == "create_document" {
                created_iter.next().map(|c| c.path.as_path())
            } else {
                None
            };
            let summary = build_summary(change, dry_run, create_display);

            let cascade = match change.operation.as_str() {
                "move_document" | "delete_document" => Some(if dry_run {
                    // Keep today's forecast cascade from the RepairApplyReport.
                    let rec = apply_result
                        .cascades
                        .iter()
                        .find(|c| c.source_path == change.path);
                    build_cascade_summary(rec, verbose)
                } else {
                    fold_cascade_from_events(events, span, verbose)
                }),
                _ => None,
            };

            ApplyReportOp {
                op_id: i.to_string(),
                kind: change.operation.clone(),
                status,
                from,
                summary,
                error: None, // see note below
                footnote: parent_op.footnote.clone(),
                cascade,
            }
        })
        .collect()
}

// Note on error field: the existing `apply_repair_plan_with_context` returns
// `Err(anyhow::Error)` for any failure and aborts the whole apply — there is
// no per-change error tracking. If the call returns `Ok`, all changes succeeded
// (or were no-ops, mapped to Skipped). The `error` field in ApplyReportOp is
// therefore always `None` in the current implementation. Per-change error
// tracking is a future enhancement (post Plan Task 20 when we own the apply loop).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration_plan::{MigrationOp, MigrationPlan};
    use crate::telemetry::{Clock, EventSink, IdGen};
    use camino::Utf8Path;

    /// A deterministic in-memory sink for the applier unit tests.
    fn test_sink() -> EventSink {
        EventSink::discard(
            IdGen::with_seed(0),
            Clock::fixed("2026-05-29T00:00:00.000Z"),
        )
    }

    fn synth_vault() -> (tempfile::TempDir, GraphIndex) {
        let tmp = tempfile::Builder::new()
            .prefix("applier-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
        std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n[[a]]\n").unwrap();
        let utf8_root = Utf8Path::from_path(root).unwrap();
        let index = crate::graph::build_index(utf8_root).unwrap();
        (tmp, index)
    }

    #[test]
    fn applier_dry_run_returns_apply_report_without_mutating() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 1,
            vault_root: vault_root.clone(),
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "a.md", "dst": "renamed.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: true,
            parents: false,
            verbose: false,
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        assert_eq!(report.schema_version, APPLY_REPORT_SCHEMA_VERSION);
        assert!(report.dry_run);
        assert_eq!(report.operations.len(), 1);
        assert_eq!(report.operations[0].kind, "move_document");
        // Dry-run: file unchanged
        assert!(tmp.path().join("a.md").exists());
        assert!(!tmp.path().join("renamed.md").exists());
    }

    #[test]
    fn applier_apply_actually_mutates_and_marks_applied() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 1,
            vault_root: vault_root.clone(),
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "a.md", "dst": "renamed.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        assert_eq!(report.applied, 1);
        assert!(matches!(
            report.operations[0].status,
            crate::apply_report::OpStatus::Applied
        ));
        // Apply: file moved
        assert!(!tmp.path().join("a.md").exists());
        assert!(tmp.path().join("renamed.md").exists());
    }

    #[test]
    fn applier_propagates_parent_provenance_on_high_level_expansion() {
        let (tmp, _index) = synth_vault();
        std::fs::create_dir_all(tmp.path().join("src_dir")).unwrap();
        std::fs::write(
            tmp.path().join("src_dir/c.md"),
            "---\ntype: note\n---\n# C\n",
        )
        .unwrap();
        // Rebuild the index now that src_dir/c.md exists.
        let utf8_root = Utf8Path::from_path(tmp.path()).unwrap();
        let index = crate::graph::build_index(utf8_root).unwrap();

        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 1,
            vault_root,
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "move_folder".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "src_dir", "dst": "dst_dir", "parents": true}),
                footnote: Some("Rename folder".into()),
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: true,
            parents: false,
            verbose: false,
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        // Expanded ops should reference parent op_id 0
        for op in &report.operations {
            assert_eq!(
                op.from.as_deref(),
                Some("0"),
                "expanded op should reference parent op_id 0"
            );
            // Footnote propagated from parent
            assert_eq!(op.footnote.as_deref(), Some("Rename folder"));
        }
    }

    #[test]
    fn move_op_carries_cascade_summary_from_actuals() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 1,
            vault_root,
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "a.md", "dst": "renamed.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        let op = report
            .operations
            .iter()
            .find(|o| o.kind == "move_document")
            .unwrap();
        let cascade = op.cascade.as_ref().expect("move op must carry a cascade");
        assert_eq!(cascade.planned, 1);
        assert_eq!(cascade.applied, 1);
        assert_eq!(cascade.skipped, 0);
        assert_eq!(cascade.files, 1);
        assert!(cascade.rewrites.is_empty());
        assert!(cascade.skips.is_empty());
    }

    #[test]
    fn verbose_populates_cascade_rewrite_list() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 1,
            vault_root,
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "move_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({"src": "a.md", "dst": "renamed.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: true,
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        let op = report
            .operations
            .iter()
            .find(|o| o.kind == "move_document")
            .unwrap();
        let cascade = op.cascade.as_ref().unwrap();
        assert_eq!(cascade.rewrites.len(), 1);
        assert_eq!(cascade.rewrites[0].file, "b.md");
    }

    #[test]
    fn migrate_add_frontmatter_on_ambiguous_key_doc_is_refused_not_duplicated() {
        // V1 (NRN-141): `"\x61"` decodes to serde key `a` but the scanner reads
        // `x61`, so the span locator refuses the whole document (empty spans). A
        // migrate-plan `add_frontmatter` for the ALREADY-present `title` then
        // slips past the `FieldAlreadyPresent` refusal (which keys off span
        // presence) and would splice a duplicate `title:` line — unparseable YAML
        // that drops every field while the run reports success. The post-image
        // gate must refuse before any write, leaving the file byte-identical.
        let tmp = tempfile::Builder::new()
            .prefix("applier-v1-ambig-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        let doc = "---\ntitle: hi\n\"\\x61\": 1\nstatus: draft\n---\nbody\n";
        std::fs::write(root.join("doc.md"), doc).unwrap();
        let index = crate::graph::build_index(Utf8Path::from_path(root).unwrap()).unwrap();

        let plan = MigrationPlan {
            schema_version: 1,
            vault_root: root.to_string_lossy().to_string(),
            generator: None,
            generated_at: None,
            operations: vec![MigrationOp {
                kind: "add_frontmatter".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({
                    "path": "doc.md", "field": "title", "new_value": "DUP"
                }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
        };
        let result = apply_migration_plan(&plan, &index, ctx, &mut test_sink());
        assert!(
            result.is_err(),
            "duplicate-key splice on an ambiguous-key doc must be refused, got {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("doc.md")).unwrap(),
            doc,
            "file must be byte-identical after the refusal"
        );
    }

    #[test]
    fn migrate_duplicate_field_ops_are_refused_before_any_write() {
        // NRN-141 round 2 ground truth: `apply_file_changes` computes spans once
        // against the ORIGINAL content and accumulates byte-range edits, so two
        // ops on the same path+field would splice with stale offsets. Such a
        // plan can never reach it: `changes_by_path` — the validation gate every
        // apply path runs before Phase A — refuses duplicate (path, field) ops
        // with ConflictingFieldChange. This pins that refusal for the claimed
        // vector, a hand-authored migrate plan; the file must stay untouched.
        let tmp = tempfile::Builder::new()
            .prefix("applier-dup-field-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        let doc = "---\nstatus: draft\n---\nbody\n";
        std::fs::write(root.join("doc.md"), doc).unwrap();
        let index = crate::graph::build_index(Utf8Path::from_path(root).unwrap()).unwrap();

        let set_status = |value: &str| MigrationOp {
            kind: "set_frontmatter".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": "doc.md", "field": "status",
                "expected_old_value": "draft", "new_value": value,
            }),
            footnote: None,
        };
        let plan = MigrationPlan {
            schema_version: 1,
            vault_root: root.to_string_lossy().to_string(),
            generator: None,
            generated_at: None,
            operations: vec![set_status("x"), set_status("longer-value")],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
        };
        let result = apply_migration_plan(&plan, &index, ctx, &mut test_sink());
        let err = result.expect_err("duplicate-field plan must be refused up front");
        assert!(
            err.to_string().contains("conflicting changes"),
            "expected the ConflictingFieldChange refusal, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("doc.md")).unwrap(),
            doc,
            "file must be byte-identical after the refusal"
        );
    }
}
