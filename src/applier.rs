//! Unified applier: planner expansion pre-pass + delegation to today's
//! pass-based apply_repair_plan_with_context.
//!
//! This module is the integration point that wires the MigrationPlan →
//! PlannedChange expansion to the existing pass-based apply orchestrator
//! (repair_apply.rs). Every document-mutation command (move, delete,
//! rewrite-wikilink, apply) builds a MigrationPlan and applies it here,
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
    ApplyReport, ApplyReportOp, ApplyReportPrecondition, ApplyWarning, CascadeFailure,
    CascadeRewrite, CascadeSkip, CascadeSummary, LinkImpact, OpStatus, PreconditionStatus,
    APPLY_REPORT_SCHEMA_VERSION,
};
use crate::core::GraphIndex;
use crate::migration_plan::{MigrationOp, MigrationPlan, OwnerSelector, PlanPrecondition};
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
use std::collections::{BTreeSet, HashMap};

/// Context for `apply_migration_plan`.
#[derive(Default)]
pub(crate) struct ApplyContext {
    /// When true, no filesystem mutations are made; report shows what would happen.
    pub dry_run: bool,
    /// When true, create intermediate parent directories for create_document ops.
    pub parents: bool,
    /// When true, per-op cascade summaries include the full rewrite/skip lists.
    pub verbose: bool,
    /// When true (NRN-150/MMR-202), a byte-identical CLEAN REFUSAL (a failure
    /// raised before ANY filesystem write — the runtime write-state fact is still
    /// false) is returned as an `ApplyReport` — the offending op `failed` with a
    /// structured `error.code`, the untouched ops `not_run`, `outcome = refused` —
    /// instead of a bare `Err`. The MCP mutation tools set this so a client
    /// branches on `code` rather than receiving an opaque `internal_error`; the
    /// CLI leaves it false and renders the structured envelope from the `Err`
    /// itself (exit 2).
    ///
    /// A PARTIAL apply (a write already landed, then an op failed) is NOT gated by
    /// this flag: it is always returned as `Ok(report)` with `outcome = failed`
    /// (exit 1) on BOTH surfaces — so the truthful partial state is never hidden
    /// behind a bare `Err` (CLI) or lost as a byte-identical `refused` (MCP). The
    /// refused-vs-failed decision is the runtime write-state, never the
    /// per-variant `ApplyError::is_precondition()` flag.
    pub refuse_as_report: bool,
    /// Index policy used for the fresh owner snapshot. Plans with no logical
    /// preconditions do not pay for this second filesystem scan.
    pub owner_index_options: crate::graph::IndexOptions,
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
/// On a clean COMMIT, the returned [`ApplyReport`] carries the changed-file set
/// on its `touched_paths` field (NRN-252 / NRN-158) — populated from
/// `RepairApplyReport::touched_paths`. The MCP warm mutation tools (`move` /
/// `delete` / `rewrite_wikilink` / `apply`) feed it to their cache-increment
/// commit; the CLI ignores it (and it is `#[serde(skip)]`, so it never touches
/// the wire report). A refusal writes nothing (empty set) and a partial failure
/// leaves the next read's `detect` to heal the cache (also empty).
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
        let expanded = match expand(op, index) {
            Ok(expanded) => expanded,
            Err(e) => {
                // Expansion is pure (index-only, no filesystem write), so ANY
                // failure here is provably PRE-WRITE — the vault is byte-identical
                // (NRN-231 review F1). Under `refuse_as_report` (the daemon/MCP
                // surface, ADR 0011) cross it as a coded, report-shaped refusal so
                // a routed apply reconstructs the exact exit-2 refusal the direct
                // arm renders, instead of a false post-send-uncertain. The CLI
                // leaves `refuse_as_report` false and still renders the `Err`
                // envelope itself (exit 2).
                if ctx.refuse_as_report {
                    return Ok(build_plan_refusal_report(
                        plan,
                        ctx.dry_run,
                        i,
                        crate::apply_report::ApplyError::from_anyhow(&e),
                    ));
                }
                return Err(e);
            }
        };
        for c in expanded {
            provenance.push(i);
            all_changes.push(c);
        }
    }

    // Resolve every create template before the single owner-set barrier. The
    // concrete paths flow into the delegate, so allocation is performed once
    // under the mutation lock and cannot drift between checking and writing.
    let operation_stems = resolve_create_paths(plan, index, &mut all_changes, &provenance)?;
    let owner_index = if plan.preconditions.is_empty() {
        None
    } else {
        Some(crate::graph::build_index_with_options(
            &index.root,
            &ctx.owner_index_options,
        )?)
    };
    let preconditions = evaluate_owner_preconditions(
        plan,
        owner_index.as_ref().unwrap_or(index),
        &operation_stems,
    )?;
    if preconditions
        .iter()
        .any(|precondition| precondition.status == PreconditionStatus::Failed)
    {
        return Ok(build_owner_precondition_refusal_report(
            plan,
            ctx.dry_run,
            preconditions,
        ));
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

    // Runtime write-state fact (NRN-150/183): flipped to `true` by the delegate
    // the instant its FIRST filesystem write lands. This — NOT the per-variant
    // `ApplyError::is_precondition()` flag — is the refused-vs-failed gate below,
    // because the same variant (`stale-document-hash`, `unknown-path`) is raised
    // from both a pre-write site (Phase A1 content CAS) and a
    // post-write-possible site (the Phase B delete pass, after Phase A2 already
    // wrote), so a variant flag structurally cannot answer "was the vault
    // mutated?".
    let mut wrote_any = false;
    let apply_result = match apply_repair_plan_with_context(
        &vault_root,
        index,
        &repair_plan,
        ctx.dry_run,
        &create_ctx,
        sink,
        &spans,
        Some(&mut wrote_any),
    ) {
        Ok(r) => r,
        Err(e) => {
            if wrote_any {
                // PARTIAL APPLY: disk was already mutated before the abort. Emit
                // the TRUTHFUL failed report (outcome = failed, exit 1) on BOTH
                // surfaces — already-applied ops `applied`, the failing op
                // `failed` + `error.code`, the rest `not_run`. This must NEVER
                // imply a byte-identical vault; a consumer sees the partial state
                // and which op failed, and knows to re-read rather than retry.
                let rich = e.downcast_ref::<crate::standards::apply::ApplyError>();
                let envelope = rich
                    .map(crate::apply_report::ApplyError::from_rich)
                    .unwrap_or_else(|| crate::apply_report::ApplyError::from_anyhow(&e));
                let error_path = rich.and_then(|r| r.path().map(|p| p.to_path_buf()));
                let trace_id = sink.trace_id().to_string();
                return Ok(build_partial_failure_report(
                    plan,
                    &hydrated,
                    &provenance,
                    &span_ids,
                    sink.events(),
                    envelope,
                    error_path.as_deref(),
                    &trace_id,
                    preconditions.clone(),
                ));
            }
            // CLEAN REFUSAL: nothing written yet (`wrote_any == false`), the vault
            // is byte-identical. Return-report-on-refusal (NRN-150/MMR-202) when
            // the caller opted in (`refuse_as_report`); otherwise (CLI) propagate
            // the `Err` so the structured stdout envelope renders and the arm
            // exits 2.
            //
            // NRN-231 review F1: this covers the WHOLE pre-write error class, not
            // just typed rich `ApplyError`s. A bare `anyhow` raised before any
            // write (create_document frontmatter/serialize validation, the
            // `{{seq}}`/ignore/exists guards, the up-front validation gates) is
            // also byte-identical here, so under `refuse_as_report` it too crosses
            // as a coded, report-shaped refusal — `internal-error` + the `{e:#}`
            // message, exactly what the CLI's `Err` path renders — instead of
            // escaping as a bare MCP error that a routed apply would misreport as
            // post-send-uncertain. Because the gate is the runtime write-state
            // (`wrote_any`), a bare error raised MID-apply took the `if wrote_any`
            // partial-failure branch above and never reaches here.
            if ctx.refuse_as_report {
                let rich = e.downcast_ref::<crate::standards::apply::ApplyError>();
                let envelope = rich
                    .map(crate::apply_report::ApplyError::from_rich)
                    .unwrap_or_else(|| crate::apply_report::ApplyError::from_anyhow(&e));
                let error_path = rich.and_then(|r| r.path().map(|p| p.to_path_buf()));
                return Ok(build_refusal_report(
                    plan,
                    &hydrated,
                    &provenance,
                    ctx.dry_run,
                    envelope,
                    error_path.as_deref(),
                    preconditions.clone(),
                ));
            }
            return Err(e);
        }
    };

    // The apply COMMITTED cleanly — capture the touched-file set to carry on the
    // projected report for the caller's increment commit (NRN-252 / NRN-158).
    let touched_paths = apply_result.touched_paths();

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
        index,
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

    // NRN-183: collapse the tri-state into one machine-readable field. A
    // runtime op-failure (`failed > 0`) maps to `failed` (exit 1); otherwise the
    // apply/dry-run succeeded (`applied`, exit 0). The `refused` outcome (exit 2)
    // is produced only by `build_refusal_report` on a precondition refusal.
    let outcome = if failed > 0 {
        crate::apply_report::ApplyOutcome::Failed
    } else {
        crate::apply_report::ApplyOutcome::Applied
    };

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
        preconditions,
        operations: ops,
        warnings,
        outcome,
        touched_paths,
    })
}

fn evaluate_owner_preconditions(
    plan: &MigrationPlan,
    index: &GraphIndex,
    operation_stems: &HashMap<String, String>,
) -> Result<Vec<ApplyReportPrecondition>> {
    let mut precondition_ids = BTreeSet::new();
    for precondition in &plan.preconditions {
        if !precondition_ids.insert(precondition.id()) {
            anyhow::bail!(
                "duplicate owner-set precondition id in MigrationPlan: {}",
                precondition.id()
            );
        }
    }

    let mut results = plan
        .preconditions
        .iter()
        .map(|precondition| -> Result<ApplyReportPrecondition> {
            Ok(match precondition {
                PlanPrecondition::OwnerSet {
                    id,
                    selector,
                    expected_paths,
                } => {
                    let mut expected_paths = expected_paths.clone();
                    expected_paths.sort();
                    expected_paths.dedup();
                    let mut actual_paths = match selector {
                        OwnerSelector::Stem { stem } => {
                            if stem.is_empty() {
                                anyhow::bail!(
                                    "owner-set precondition '{}' has an empty stem selector",
                                    precondition.id()
                                );
                            }
                            index
                                .documents
                                .iter()
                                .filter(|document| document.stem.eq_ignore_ascii_case(stem))
                                .map(|document| document.path.to_string())
                                .collect::<Vec<_>>()
                        }
                        OwnerSelector::StemFromOperation {
                            stem_from_operation,
                        } => {
                            let stem = operation_stems.get(stem_from_operation).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "owner-set precondition '{}' references missing or non-create operation id '{}'",
                                    precondition.id(),
                                    stem_from_operation
                                )
                            })?;
                            index
                                .documents
                                .iter()
                                .filter(|document| document.stem.eq_ignore_ascii_case(stem))
                                .map(|document| document.path.to_string())
                                .collect::<Vec<_>>()
                        }
                        OwnerSelector::Eq { eq } => {
                            if eq.is_empty() {
                                anyhow::bail!(
                                    "owner-set precondition '{}' has an empty eq selector",
                                    precondition.id()
                                );
                            }
                            index
                                .documents
                                .iter()
                                .filter_map(|document| match document_matches_eq(document, eq) {
                                    Ok(true) => Some(Ok(document.path.to_string())),
                                    Ok(false) => None,
                                    Err(error) => Some(Err(error)),
                                })
                                .collect::<Result<Vec<_>>>()?
                        }
                    };
                    actual_paths.sort();
                    actual_paths.dedup();
                    let mismatch = expected_paths != actual_paths;
                    ApplyReportPrecondition {
                        id: id.clone(),
                        status: if mismatch {
                            PreconditionStatus::Failed
                        } else {
                            PreconditionStatus::Passed
                        },
                        expected_paths,
                        actual_paths,
                        error: mismatch.then(|| crate::apply_report::ApplyError {
                            code: "owner-set-mismatch".to_string(),
                            message: format!(
                                "owner-set precondition '{}' did not match the current vault",
                                precondition.id()
                            ),
                            path: None,
                        }),
                    }
                }
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // A pair of creates that claim the same derived stem can both observe an
    // empty on-disk owner set. Refuse that internally contradictory plan at the
    // same barrier instead of letting operation order manufacture duplicates.
    let mut claims_by_stem: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, precondition) in plan.preconditions.iter().enumerate() {
        let PlanPrecondition::OwnerSet {
            selector:
                OwnerSelector::StemFromOperation {
                    stem_from_operation,
                },
            ..
        } = precondition
        else {
            continue;
        };
        if let Some(stem) = operation_stems.get(stem_from_operation) {
            claims_by_stem
                .entry(stem.to_ascii_lowercase())
                .or_default()
                .push(index);
        }
    }
    for (stem, indexes) in claims_by_stem {
        if indexes.len() < 2 {
            continue;
        }
        for index in indexes {
            let result = &mut results[index];
            result.status = PreconditionStatus::Failed;
            result.error = Some(crate::apply_report::ApplyError {
                code: "owner-claim-conflict".to_string(),
                message: format!(
                    "owner-set precondition '{}' conflicts with another planned create for stem '{stem}'",
                    result.id
                ),
                path: None,
            });
        }
    }

    Ok(results)
}

fn document_matches_eq(document: &crate::core::Document, eq: &[String]) -> Result<bool> {
    let Some(frontmatter) = document.frontmatter.as_ref() else {
        return Ok(false);
    };
    for predicate in eq {
        let (field, expected) = crate::filter_args::parse_field_value(predicate, "owner_set.eq")?;
        let Some(actual) = frontmatter.get(&field) else {
            return Ok(false);
        };
        let matches = match actual {
            serde_json::Value::Array(values) => values
                .iter()
                .any(|value| owner_eq_value_matches(value, &expected)),
            value => owner_eq_value_matches(value, &expected),
        };
        if !matches {
            return Ok(false);
        }
    }
    Ok(true)
}

fn owner_eq_value_matches(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual, expected) {
        (serde_json::Value::String(actual), serde_json::Value::String(expected)) => {
            strip_wikilink_brackets(actual) == strip_wikilink_brackets(expected)
        }
        (serde_json::Value::Number(actual), serde_json::Value::Number(expected)) => {
            actual.as_f64() == expected.as_f64()
        }
        _ => actual == expected,
    }
}

fn strip_wikilink_brackets(value: &str) -> String {
    value.replace("[[", "").replace("]]", "")
}

fn resolve_create_paths(
    plan: &MigrationPlan,
    index: &GraphIndex,
    changes: &mut [PlannedChange],
    provenance: &[usize],
) -> Result<HashMap<String, String>> {
    let mut stems = HashMap::new();
    let mut allocated_this_plan: Vec<Utf8PathBuf> = Vec::new();
    let mut operation_ids = BTreeSet::new();
    for operation in &plan.operations {
        if let Some(id) = operation.id.as_ref() {
            if !operation_ids.insert(id) {
                anyhow::bail!("duplicate operation id in MigrationPlan: {id}");
            }
        }
    }

    for (change, parent_index) in changes.iter_mut().zip(provenance) {
        if change.operation != "create_document" {
            continue;
        }
        let path = change.path.clone();
        let resolved = if crate::seq_alloc::has_seq(&path) {
            let dir = index
                .root
                .join(path.parent().unwrap_or_else(|| Utf8Path::new("")));
            let mut siblings = crate::seq_alloc::dir_file_names(&dir)?;
            for prior in &allocated_this_plan {
                if prior.parent() == path.parent() {
                    if let Some(name) = prior.file_name() {
                        siblings.push(name.to_string());
                    }
                }
            }
            let resolved = crate::seq_alloc::resolve_seq(&path, &siblings);
            if crate::seq_alloc::has_seq(&resolved) {
                anyhow::bail!(
                    "create_document: `{{{{seq}}}}` is only supported once, in the file name of a rule target: {path}"
                );
            }
            resolved
        } else {
            path
        };
        allocated_this_plan.push(resolved.clone());
        change.path = resolved.clone();

        let stem = resolved
            .file_stem()
            .ok_or_else(|| anyhow::anyhow!("create_document path has no file stem: {resolved}"))?;
        if let Some(id) = plan.operations[*parent_index].id.as_ref() {
            stems.insert(id.clone(), stem.to_string());
        }
    }

    Ok(stems)
}

fn build_owner_precondition_refusal_report(
    plan: &MigrationPlan,
    dry_run: bool,
    preconditions: Vec<ApplyReportPrecondition>,
) -> ApplyReport {
    let operations = plan
        .operations
        .iter()
        .enumerate()
        .map(|(index, operation)| ApplyReportOp {
            op_id: index.to_string(),
            kind: operation.kind.clone(),
            status: OpStatus::NotRun,
            from: None,
            path: None,
            stem: None,
            summary: format!("would {} {}", operation.kind, op_display_path(operation)),
            error: None,
            footnote: operation.footnote.clone(),
            cascade: None,
            link_impact: None,
        })
        .collect::<Vec<_>>();
    ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        trace_id: String::new(),
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        dry_run,
        applied: 0,
        skipped: 0,
        failed: 0,
        remaining: operations.len(),
        preconditions,
        operations,
        warnings: Vec::new(),
        outcome: crate::apply_report::ApplyOutcome::Refused,
        touched_paths: Vec::new(),
    }
}

/// Build a return-report-on-refusal (NRN-150/MMR-202) from a PRE-WRITE refusal
/// (`wrote_any == false` proves the vault is byte-identical). Every expanded
/// change becomes a `not_run` op EXCEPT the first whose path matches
/// `error_path`, which becomes `failed` carrying the structured `error`
/// envelope. `outcome = refused`. When the error carries no path (a plan-level
/// refusal, or a bare `anyhow` without a resolvable path), the first op is
/// marked failed so the code is never lost.
///
/// `envelope` is prebuilt by the caller: a typed rich `ApplyError` contributes
/// its stable `code`/`path` (`ApplyError::from_rich`); a bare `anyhow` (NRN-231
/// review F1 — create_document validation, op expansion, etc.) falls back to
/// `internal-error` + the `{e:#}` message (`ApplyError::from_anyhow`), exactly
/// what the CLI's `render_json_error_envelope` / `eprintln!("error: {e:#}")`
/// renders on the `Err` path — so a routed refusal stays byte-identical to
/// Direct's exit-2 refusal.
fn build_refusal_report(
    plan: &MigrationPlan,
    changes: &[PlannedChange],
    provenance: &[usize],
    dry_run: bool,
    envelope: crate::apply_report::ApplyError,
    error_path: Option<&Utf8Path>,
    preconditions: Vec<ApplyReportPrecondition>,
) -> ApplyReport {
    use crate::apply_report::ApplyOutcome;

    // Index of the op to mark failed: the first change whose path matches the
    // error path; else the first op (pathless plan-level refusal).
    let failed_idx = error_path
        .and_then(|ep| changes.iter().position(|c| c.path == ep))
        .unwrap_or(0);

    let ops: Vec<ApplyReportOp> = changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            let parent_idx = provenance[i];
            let parent_op = &plan.operations[parent_idx];
            let from = if is_high_level_op(parent_op) {
                Some(parent_idx.to_string())
            } else {
                None
            };
            let (status, error) = if i == failed_idx {
                (OpStatus::Failed, Some(envelope.clone()))
            } else {
                (OpStatus::NotRun, None)
            };
            ApplyReportOp {
                op_id: i.to_string(),
                kind: change.operation.clone(),
                status,
                from,
                path: None,
                stem: None,
                summary: build_summary(change, /*dry_run=*/ true, None),
                error,
                footnote: parent_op.footnote.clone(),
                cascade: None,
                // A byte-identical refusal writes nothing and renders no records
                // link-impact line — the coded error is the whole output.
                link_impact: None,
            }
        })
        .collect();

    // A refusal writes nothing: exactly one op `failed`, the rest `not_run`.
    let failed = ops.iter().filter(|o| o.status == OpStatus::Failed).count();
    let remaining = ops.iter().filter(|o| o.status == OpStatus::NotRun).count();

    ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        trace_id: String::new(),
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        dry_run,
        applied: 0,
        skipped: 0,
        failed,
        remaining,
        preconditions,
        operations: ops,
        warnings: Vec::new(),
        outcome: ApplyOutcome::Refused,
        // A refusal writes nothing, so no cache increment is needed.
        touched_paths: Vec::new(),
    }
}

/// Build a pre-write refusal report for an EXPANSION-PHASE failure (NRN-231
/// review F1), before any `PlannedChange` exists. Expansion is pure (index-only,
/// no filesystem write), so this is byte-identical: every plan operation becomes
/// a `not_run` op EXCEPT `failed_op_idx`, which is `failed` carrying the
/// structured `error` envelope. `outcome = refused` (exit 2). Mirrors
/// [`build_refusal_report`] but keys off `plan.operations` rather than expanded
/// changes, since expansion is exactly what failed.
fn build_plan_refusal_report(
    plan: &MigrationPlan,
    dry_run: bool,
    failed_op_idx: usize,
    envelope: crate::apply_report::ApplyError,
) -> ApplyReport {
    use crate::apply_report::ApplyOutcome;

    let ops: Vec<ApplyReportOp> = plan
        .operations
        .iter()
        .enumerate()
        .map(|(i, op)| {
            let (status, error) = if i == failed_op_idx {
                (OpStatus::Failed, Some(envelope.clone()))
            } else {
                (OpStatus::NotRun, None)
            };
            ApplyReportOp {
                op_id: i.to_string(),
                kind: op.kind.clone(),
                status,
                from: None,
                path: None,
                stem: None,
                summary: format!("would {} {}", op.kind, op_display_path(op)),
                error,
                footnote: op.footnote.clone(),
                cascade: None,
                link_impact: None,
            }
        })
        .collect();

    let failed = ops.iter().filter(|o| o.status == OpStatus::Failed).count();
    let remaining = ops.iter().filter(|o| o.status == OpStatus::NotRun).count();

    ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        trace_id: String::new(),
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        dry_run,
        applied: 0,
        skipped: 0,
        failed,
        remaining,
        preconditions: Vec::new(),
        operations: ops,
        warnings: Vec::new(),
        outcome: ApplyOutcome::Refused,
        // A refusal writes nothing, so no cache increment is needed.
        touched_paths: Vec::new(),
    }
}

/// Best-effort display token for a raw `MigrationOp` (an expansion-phase refusal
/// has no `PlannedChange` to summarize). Reads the common `path`/`src` fields;
/// falls back to the op kind's placeholder. Only feeds the refusal report's
/// `summary`, which the refusal renderer does not print — the coded `error` is
/// the whole output — so exact prose here is not load-bearing.
fn op_display_path(op: &MigrationOp) -> String {
    op.fields
        .get("path")
        .or_else(|| op.fields.get("src"))
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string()
}

/// Build the TRUTHFUL partial-failure report (NRN-150/183) for an apply that
/// ABORTED after at least one filesystem write already landed. Unlike
/// [`build_refusal_report`] (which asserts a byte-identical vault), this must NOT
/// imply that nothing changed: every op that emitted an `applied` op-action
/// event is `applied`, the op that raised the abort is `failed` carrying the
/// structured `error` envelope, and every op that never ran is `not_run`.
/// `outcome = failed` (exit 1) on BOTH the CLI (`report.exit_code()`) and MCP
/// (`Ok(report)`) surfaces, so a consumer sees the partial state and which op
/// failed and knows to re-read rather than blindly retry.
#[allow(clippy::too_many_arguments)]
fn build_partial_failure_report(
    plan: &MigrationPlan,
    changes: &[PlannedChange],
    provenance: &[usize],
    span_ids: &[String],
    events: &[Event],
    envelope: crate::apply_report::ApplyError,
    error_path: Option<&Utf8Path>,
    trace_id: &str,
    preconditions: Vec<ApplyReportPrecondition>,
) -> ApplyReport {
    use crate::apply_report::ApplyOutcome;

    // Per-change: did an `applied` op-action event land under this op's span?
    // Same event-driven inference `build_report_ops` uses for the Ok path.
    let applied_flags: Vec<bool> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let span = span_ids.get(i).map(|s| s.as_str());
            let name = action_event_name(&c.operation);
            span.is_some()
                && events.iter().any(|e| {
                    e.span_id.as_deref() == span
                        && e.name == name
                        && attr(e, ATTR_STATUS) == Some("applied")
                })
        })
        .collect();

    // The failing op: the first NOT-yet-applied change whose path matches the
    // error path (falls back to the first un-applied op, then op 0). Preferring
    // an un-applied match keeps an edit-then-delete on the SAME path from
    // mislabelling the already-applied edit as the failure.
    let failed_idx = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| error_path.is_none_or(|ep| c.path == ep))
        .find(|(i, _)| !applied_flags[*i])
        .map(|(i, _)| i)
        .or_else(|| applied_flags.iter().position(|a| !a))
        .unwrap_or(0);

    let ops: Vec<ApplyReportOp> = changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            let parent_op = &plan.operations[provenance[i]];
            let from = if is_high_level_op(parent_op) {
                Some(provenance[i].to_string())
            } else {
                None
            };
            let (status, error) = if i == failed_idx {
                (OpStatus::Failed, Some(envelope.clone()))
            } else if applied_flags[i] {
                (OpStatus::Applied, None)
            } else {
                (OpStatus::NotRun, None)
            };
            ApplyReportOp {
                op_id: i.to_string(),
                kind: change.operation.clone(),
                status,
                from,
                path: None,
                stem: None,
                summary: build_summary(change, /*dry_run=*/ false, None),
                error,
                footnote: parent_op.footnote.clone(),
                cascade: None,
                // A partial-failure report is the abort/error surface, not the
                // records success renderer; link-impact is unused here.
                link_impact: None,
            }
        })
        .collect();

    let applied = ops.iter().filter(|o| o.status == OpStatus::Applied).count();
    let failed = ops.iter().filter(|o| o.status == OpStatus::Failed).count();
    let remaining = ops.iter().filter(|o| o.status == OpStatus::NotRun).count();

    ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        trace_id: trace_id.to_string(),
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        // A write landed → this is not a dry run.
        dry_run: false,
        applied,
        skipped: 0,
        failed,
        remaining,
        preconditions,
        operations: ops,
        warnings: Vec::new(),
        outcome: ApplyOutcome::Failed,
        // A partial failure leaves the next read's `detect` to heal the cache.
        touched_paths: Vec::new(),
    }
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
/// Compute the [`LinkImpact`] for a `delete_document` change from the graph
/// index (NRN-237) — the single source of truth for the records renderer's
/// incoming-link inputs, shared by the direct and warm-daemon paths.
///
/// Reproduces the semantics of the former CLI-arm locals verbatim:
/// - `incoming_total` / `incoming_files`: [`backlinks`](crate::target::backlinks)
///   against the deleted doc's path, deduped + sorted through a `BTreeSet` of
///   the vault-relative source paths.
/// - fallback: when `--rewrite-to` is present but no backlink resolves to the
///   deleted doc, the files list is the `link_risk` rewrite sources (the same
///   `stem_links` + `path_qualified_wikilinks` + `markdown_links` union the CLI
///   arm used).
/// - `redirect_to`: the raw `rewrite_to` field resolved against the index the
///   same way the CLI preflight resolves it — so a stem argument renders as its
///   resolved `.md` path, byte-identical to the direct arm.
fn build_link_impact(
    change: &PlannedChange,
    parent_op: &MigrationOp,
    index: &GraphIndex,
) -> LinkImpact {
    use std::collections::BTreeSet;

    let bl = crate::target::backlinks(index, &change.path);
    let incoming_total = bl.len();

    let mut files: BTreeSet<Utf8PathBuf> = bl.iter().map(|link| link.source_path.clone()).collect();

    // The raw `--rewrite-to` argument, if any, lives on the parent MigrationOp.
    let raw_rewrite_to = parent_op.fields.get("rewrite_to").and_then(|v| v.as_str());

    // Fallback (NRN-248): `backlinks()` is resolution-keyed — it only sees links
    // with `resolved_path == Some(change.path)`. Two realizable classes of link
    // to the deleted doc have `resolved_path == None` and are therefore invisible
    // to it, yet `classify_link_risk`'s textual fallback (`link_targets_path`)
    // still matches them by comparing the raw target string: (1) an ambiguous
    // same-stem wikilink (two docs share a stem, so resolution reports Ambiguous
    // instead of picking one) and (2) a dangling relative markdown link whose raw
    // href textually coincides with the deleted path but resolves relative to a
    // different directory. When `--rewrite-to` is set and no resolved backlink
    // survived, fall back to the change's link_risk rewrite sources so these
    // links still show up in the report — the resulting shape (`incoming_total:
    // 0` with non-empty `incoming_files`) is unique to this path and is the
    // signal that a redirect reached links `backlinks()` couldn't count.
    if raw_rewrite_to.is_some() && files.is_empty() {
        if let Some(risk) = &change.link_risk {
            for affected in risk
                .stem_links
                .iter()
                .chain(risk.path_qualified_wikilinks.iter())
                .chain(risk.markdown_links.iter())
            {
                files.insert(affected.source_path.clone());
            }
        }
    }

    // Resolve the raw redirect target against the index exactly as the CLI
    // preflight does (a bare stem → its `.md` path). Preflight already validated
    // resolvability before this plan was built, so `.ok()` is the success value.
    let redirect_to = raw_rewrite_to.and_then(|raw| {
        crate::target::resolve_target_path(index, raw)
            .ok()
            .map(|p| p.to_string())
    });

    LinkImpact {
        incoming_total,
        incoming_files: files.into_iter().map(|p| p.to_string()).collect(),
        redirect_to,
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
    index: &GraphIndex,
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

            // Lockstep invariant (NRN-175 / F6): `created_documents` holds one
            // entry per create_document that actually PRODUCED a file — every
            // applied create in a real apply, and every predicted create in a
            // dry-run. Consume the iterator ONLY for such ops. If a create ever
            // reached here Skipped/Failed (not constructible today — the apply
            // loop aborts with `Err` on any create failure before build_report_ops
            // runs — but guarded defensively), consuming would misattribute a
            // sibling's created path to it. Not consuming leaves path/stem ABSENT
            // (absent-not-stale) instead.
            let create_realized = dry_run || status == OpStatus::Applied;
            let create_display = if change.operation == "create_document" && create_realized {
                created_iter.next().map(|c| c.path.as_path())
            } else {
                None
            };
            let summary = build_summary(change, dry_run, create_display);

            // NRN-175: structured, apply-time-resolved target path — the value a
            // consumer would otherwise regex out of `summary`. Populated where a
            // single natural target exists: a `create_document`'s `{{seq}}`-resolved
            // destination, a `move_document`'s destination, and body/section edit
            // targets. Left `None` for ops with no single natural path (link
            // rewrites, deletes, frontmatter field ops).
            let resolved_path: Option<&camino::Utf8Path> = match change.operation.as_str() {
                "create_document" => create_display,
                "move_document" => change.destination.as_deref(),
                "replace_body"
                | "replace_section"
                | "append_to_section"
                | "delete_section"
                | "insert_before_heading"
                | "insert_after_heading" => Some(change.path.as_path()),
                _ => None,
            };
            let path = resolved_path.map(|p| p.to_string());
            let stem = resolved_path
                .and_then(|p| p.file_stem())
                .map(str::to_string);

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

            // NRN-237: index-derived incoming-link impact for a `delete_document`
            // op, so the `--format records` renderer's inputs ride the wire report
            // and the routed path reproduces the direct path byte-for-byte. This
            // reproduces the CLI arm's former locals EXACTLY: `backlinks` for the
            // count, BTreeSet-distinct sorted source paths for the files, the
            // `link_risk`-sources fallback when `--rewrite-to` is set and no
            // backlink resolves, and the index-RESOLVED redirect target. The index
            // view is identical on dry-run and confirm, so populate on both.
            let link_impact = (change.operation == "delete_document")
                .then(|| build_link_impact(change, parent_op, index));

            ApplyReportOp {
                op_id: i.to_string(),
                kind: change.operation.clone(),
                status,
                from,
                path,
                stem,
                summary,
                error: None, // see note below
                footnote: parent_op.footnote.clone(),
                cascade,
                link_impact,
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
            schema_version: 2,
            vault_root: vault_root.clone(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
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
            refuse_as_report: false,
            owner_index_options: Default::default(),
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
            schema_version: 2,
            vault_root: vault_root.clone(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
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
            refuse_as_report: false,
            owner_index_options: Default::default(),
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

    /// NRN-248: `build_link_impact`'s fallback branch (files come from
    /// `link_risk` rewrite sources rather than `backlinks()`) fires end to end
    /// through `apply_migration_plan` for a `delete_document` op whose only
    /// incoming reference is an ambiguous same-stem wikilink — `x/b.md` and
    /// `y/b.md` share stem `b`, so `a.md`'s bare `[[b]]` resolves to
    /// `resolved_path: None` (Ambiguous) and is invisible to `backlinks()`,
    /// but `link_risk`'s textual fallback still catches it. See the
    /// integration-test pair in `tests/delete_command.rs` for the full CLI
    /// surface + observed cascade rewrite; this unit test pins the same
    /// shape at the `apply_migration_plan` boundary in dry-run (no FS
    /// mutation needed to observe the computed `LinkImpact`).
    #[test]
    fn build_link_impact_fallback_fires_on_ambiguous_stem_backlink() {
        let tmp = tempfile::Builder::new()
            .prefix("applier-nrn248-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("x")).unwrap();
        std::fs::create_dir(root.join("y")).unwrap();
        std::fs::write(root.join("x/b.md"), "---\ntype: note\n---\n# B in x\n").unwrap();
        std::fs::write(root.join("y/b.md"), "---\ntype: note\n---\n# B in y\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
        std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
        let utf8_root = Utf8Path::from_path(root).unwrap();
        let index = crate::graph::build_index(utf8_root).unwrap();

        let vault_root = root.to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![MigrationOp {
                kind: "delete_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({
                    "path": "x/b.md",
                    "rewrite_to": "c.md",
                    "allow_broken_links": false,
                }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: true,
            parents: false,
            verbose: false,
            refuse_as_report: false,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        let op = &report.operations[0];
        let li = op
            .link_impact
            .as_ref()
            .expect("link_impact must be present on delete_document op");
        assert_eq!(
            li.incoming_total, 0,
            "no resolved backlink; link_impact: {li:?}"
        );
        assert_eq!(
            li.incoming_files,
            vec!["a.md".to_string()],
            "incoming_files must come from the link_risk fallback; link_impact: {li:?}"
        );
        assert_eq!(
            li.redirect_to.as_deref(),
            Some("c.md"),
            "redirect_to must be the resolved rewrite target; link_impact: {li:?}"
        );
    }

    /// F6: the `created_documents` iterator is walked in lockstep ONLY with
    /// create ops that actually realized (applied, or predicted in dry-run). A
    /// synthetic create op that reached `build_report_ops` NOT applied (Skipped)
    /// must leave `path`/`stem` ABSENT — never consume, and never misattribute, a
    /// sibling applied create's resolved path. This pins the absent-not-stale
    /// invariant that the defensive `create_realized` guard enforces.
    #[test]
    fn build_report_ops_skipped_create_gets_absent_not_stale_path() {
        // OpStatus, RepairApplyReport, SkippedSummary, PlannedChange,
        // MigrationOp, Event, action_event_name, ATTR_STATUS, Utf8PathBuf are all
        // already in scope via `super::*`; import only what is not.
        use crate::standards::apply::{CreateDocumentResult, RepairApplyPlanContext};
        use crate::telemetry::Severity;

        // Minimal create_document PlannedChange (path/stem for a create come from
        // `created_documents`, not from the change itself, so `path` is a filler).
        fn create_change(change_id: &str) -> PlannedChange {
            PlannedChange {
                change_id: change_id.into(),
                path: Utf8PathBuf::from("filler.md"),
                document_hash: String::new(),
                finding_code: String::new(),
                finding_rule: None,
                repair_rule: String::new(),
                operation: "create_document".into(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: None,
                link_risk: None,
                warnings: vec![],
                force: false,
                parents: false,
            }
        }
        fn create_op() -> MigrationOp {
            MigrationOp {
                kind: "create_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({}),
                footnote: None,
            }
        }

        // Two create ops. Only op1 realized: `created_documents` holds exactly
        // ONE entry (op1's resolved path), and only span "s1" has an applied
        // op-action event, so op0 computes to Skipped, op1 to Applied.
        let changes = vec![create_change("c0"), create_change("c1")];
        let provenance = vec![0usize, 1usize];
        let plan_ops = vec![create_op(), create_op()];
        let span_ids = vec!["s0".to_string(), "s1".to_string()];
        let events = vec![Event {
            trace_id: String::new(),
            span_id: Some("s1".into()),
            severity: Severity::Info,
            name: action_event_name("create_document"),
            body: String::new(),
            attributes: vec![(ATTR_STATUS, "applied".into())],
            timestamp: String::new(),
        }];
        let apply_result = RepairApplyReport {
            schema_version: 1,
            dry_run: false,
            changed_files: vec![],
            applied_changes: 1,
            moved_files: vec![],
            deleted_documents: vec![],
            created_documents: vec![CreateDocumentResult {
                path: Utf8PathBuf::from("tasks/created-b.md"),
            }],
            rewritten_links: vec![],
            cascades: vec![],
            replaced_bodies: vec![],
            warnings: vec![],
            plan_context: RepairApplyPlanContext {
                skipped: SkippedSummary {
                    by_reason: Default::default(),
                    total: 0,
                },
            },
            verification: None,
        };

        // The ops under test are all create_document (no link_impact), so an empty
        // index suffices.
        let empty_index = GraphIndex {
            root: Utf8PathBuf::from("/"),
            files: vec![],
            ignored_files: vec![],
            documents: vec![],
        };
        let ops = build_report_ops(
            &changes,
            &provenance,
            &plan_ops,
            &apply_result,
            false, // not dry-run
            false, // not verbose
            &span_ids,
            &events,
            &empty_index,
        );

        assert_eq!(ops.len(), 2);
        // op0 Skipped → absent-not-stale: it must NOT have consumed op1's entry.
        assert_eq!(ops[0].status, OpStatus::Skipped);
        assert!(
            ops[0].path.is_none(),
            "a skipped create must leave path absent, not stale: {:?}",
            ops[0].path
        );
        assert!(ops[0].stem.is_none(), "skipped create: stem absent too");
        // op1 Applied → consumes the single created_documents entry.
        assert_eq!(ops[1].status, OpStatus::Applied);
        assert_eq!(ops[1].path.as_deref(), Some("tasks/created-b.md"));
        assert_eq!(ops[1].stem.as_deref(), Some("created-b"));
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
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
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
            refuse_as_report: false,
            owner_index_options: Default::default(),
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
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
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
            refuse_as_report: false,
            owner_index_options: Default::default(),
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
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
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
            refuse_as_report: false,
            owner_index_options: Default::default(),
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

    /// NRN-150/183 byte-identity-lie fix. A 2-op plan where op0 (`set_frontmatter`
    /// on X) WRITES X in Phase A2, then op1 (`delete_document` on an untracked
    /// path Y) fails a Phase-B precondition (`unknown-path`, an `is_precondition`
    /// variant re-raised AFTER the content write). The refused-vs-failed gate is
    /// the runtime write-state, not the variant flag:
    ///
    /// - BEFORE the fix: `is_precondition(unknown-path) == true` routed this to a
    ///   `refused` report with `applied == 0` — the byte-identity LIE, since X was
    ///   already mutated on disk.
    /// - AFTER the fix: a write landed → `outcome = failed` (exit 1); op0 is
    ///   `applied` (X was written), op1 is `failed` carrying `error.code`, and the
    ///   report never implies a byte-identical vault.
    ///
    /// (A hydrated apply `delete` cannot carry a *stale* hash — hydration fills
    /// its empty hash from the index, and Phase B checks against that same index
    /// snapshot — so `unknown-path` is the reachable Phase-B `is_precondition`
    /// failure that reproduces the exact re-raise-after-write shape.)
    #[test]
    fn partial_apply_reports_failed_not_refused_when_a_write_landed() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root: vault_root.clone(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![
                // op0: writes a.md in Phase A2 (type: note → task).
                MigrationOp {
                    kind: "set_frontmatter".into(),
                    id: None,
                    requires: vec![],
                    fields: serde_json::json!({
                        "path": "a.md", "field": "type",
                        "expected_old_value": "note", "new_value": "task",
                    }),
                    footnote: None,
                },
                // op1: delete of an UNTRACKED path → Phase B `unknown-path`,
                // raised AFTER op0's write.
                MigrationOp {
                    kind: "delete_document".into(),
                    id: None,
                    requires: vec![],
                    fields: serde_json::json!({ "path": "ghost.md" }),
                    footnote: None,
                },
            ],
            skipped: vec![],
            plan_footnote: None,
        };
        // The MCP report-on-refusal surface (`refuse_as_report`) — where the lie
        // was observable as `outcome: refused` with a mutated disk.
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
            refuse_as_report: true,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink())
            .expect("a partial apply must return a report, not Err");

        assert_eq!(
            report.outcome,
            crate::apply_report::ApplyOutcome::Failed,
            "a write landed before the abort — the outcome is failed, NOT refused"
        );
        assert_eq!(report.exit_code(), 1, "partial apply maps to exit 1");
        assert_eq!(report.applied, 1, "op0 wrote X");
        // op0: applied (X mutated).
        assert_eq!(
            report.operations[0].status,
            crate::apply_report::OpStatus::Applied
        );
        // op1: failed carrying the structured code.
        assert_eq!(
            report.operations[1].status,
            crate::apply_report::OpStatus::Failed
        );
        assert_eq!(
            report.operations[1].error.as_ref().map(|e| e.code.as_str()),
            Some("unknown-path"),
            "the failing op carries the machine-branchable code"
        );
        // Ground truth: X really was mutated on disk — the report must not lie.
        let written = std::fs::read_to_string(tmp.path().join("a.md")).unwrap();
        assert!(
            written.contains("type: task"),
            "op0 mutated a.md; got:\n{written}"
        );
    }

    /// NRN-150/183: the clean pre-write refusal path is PRESERVED. A single-op
    /// plan whose only op fails its Phase-A1 precondition writes nothing, so the
    /// vault stays byte-identical and the outcome is `refused` (exit 2) — never
    /// `failed` — on the report-on-refusal surface.
    #[test]
    fn clean_prewrite_refusal_stays_refused_byte_identical() {
        let (tmp, index) = synth_vault();
        let original = std::fs::read_to_string(tmp.path().join("a.md")).unwrap();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![MigrationOp {
                kind: "set_frontmatter".into(),
                id: None,
                requires: vec![],
                // Wrong expected_old_value → Phase-A1 ExpectedOldValueMismatch
                // before any write.
                fields: serde_json::json!({
                    "path": "a.md", "field": "type",
                    "expected_old_value": "WRONG", "new_value": "task",
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
            refuse_as_report: true,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink())
            .expect("a precondition refusal returns a report on the MCP surface");

        assert_eq!(report.outcome, crate::apply_report::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.applied, 0);
        assert_eq!(
            report.operations[0].status,
            crate::apply_report::OpStatus::Failed
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.md")).unwrap(),
            original,
            "a clean refusal leaves the vault byte-identical"
        );
    }

    /// NRN-231 review F1: a BARE-`anyhow` PRE-WRITE refusal (a create_document
    /// whose `new_value` has no frontmatter object) crosses as a coded,
    /// report-shaped refusal on the `refuse_as_report` surface (`internal-error`
    /// plus the `{e:#}` message), NOT a bare `Err`. This is the class that made a
    /// routed apply misreport post-send-uncertain; the vault stays byte-identical.
    #[test]
    fn bare_prewrite_refusal_returns_report_under_refuse_as_report() {
        let (tmp, index) = synth_vault();
        let before = std::fs::read_dir(tmp.path()).unwrap().count();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![MigrationOp {
                kind: "create_document".into(),
                id: None,
                requires: vec![],
                // new_value with a body but NO frontmatter object → the bare
                // `anyhow!("create_document: missing or non-object frontmatter …")`
                // raised in Phase B BEFORE any write.
                fields: serde_json::json!({
                    "path": "new.md",
                    "new_value": { "body": "# New\n" }
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
            refuse_as_report: true,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink())
            .expect("a bare pre-write refusal must return a report, not Err");

        assert_eq!(report.outcome, crate::apply_report::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2, "a clean refusal maps to exit 2");
        assert_eq!(report.applied, 0);
        let op = &report.operations[0];
        assert_eq!(op.status, crate::apply_report::OpStatus::Failed);
        let err = op.error.as_ref().expect("failed op carries an error");
        assert_eq!(
            err.code, "internal-error",
            "a bare anyhow falls back to internal-error, matching the CLI Err path"
        );
        assert!(
            err.message
                .contains("create_document: missing or non-object frontmatter"),
            "the message carries the bare error prose: {}",
            err.message
        );
        // Byte-identical: nothing was created.
        assert!(!tmp.path().join("new.md").exists());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), before);
    }

    /// NRN-231 review F1 (expansion phase): a PRE-WRITE expansion failure (an
    /// unknown op kind) likewise crosses as a coded, report-shaped refusal under
    /// `refuse_as_report`, before any `PlannedChange` exists. The CLI surface
    /// (`refuse_as_report: false`) keeps propagating the bare `Err`.
    #[test]
    fn expansion_failure_returns_report_under_refuse_as_report() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![MigrationOp {
                kind: "no_such_kind".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({ "path": "a.md" }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };

        // refuse_as_report: report-shaped refusal, exit 2.
        let report = apply_migration_plan(
            &plan,
            &index,
            ApplyContext {
                dry_run: false,
                parents: false,
                verbose: false,
                refuse_as_report: true,
                owner_index_options: Default::default(),
            },
            &mut test_sink(),
        )
        .expect("an expansion failure must return a report under refuse_as_report");
        assert_eq!(report.outcome, crate::apply_report::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.operations.len(), 1);
        assert_eq!(
            report.operations[0].status,
            crate::apply_report::OpStatus::Failed
        );
        assert!(report.operations[0]
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("unknown operation kind")));

        // CLI surface: still a bare Err (renders its own envelope, exits 2).
        let err = apply_migration_plan(
            &plan,
            &index,
            ApplyContext {
                dry_run: false,
                parents: false,
                verbose: false,
                refuse_as_report: false,
                owner_index_options: Default::default(),
            },
            &mut test_sink(),
        )
        .expect_err("the CLI surface keeps propagating the bare Err");
        assert!(err.to_string().contains("unknown operation kind"));
    }

    #[test]
    fn migrate_add_frontmatter_on_ambiguous_key_doc_is_refused_not_duplicated() {
        // V1 (NRN-141): `"\x61"` decodes to serde key `a` but the scanner reads
        // `x61`, so the span locator refuses the whole document (empty spans). A
        // MigrationPlan `add_frontmatter` for the ALREADY-present `title` then
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
            schema_version: 2,
            vault_root: root.to_string_lossy().to_string(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
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
            refuse_as_report: false,
            owner_index_options: Default::default(),
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
        // vector, a hand-authored MigrationPlan; the file must stay untouched.
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
            schema_version: 2,
            vault_root: root.to_string_lossy().to_string(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![set_status("x"), set_status("longer-value")],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
            refuse_as_report: false,
            owner_index_options: Default::default(),
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
