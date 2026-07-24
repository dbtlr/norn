//! The one applier: plan-load + validation + report assembly around the named
//! passes (ADR 0024).
//!
//! This module owns the plan-level orchestration — schema gate, expansion,
//! delete-hash + owner-set + `requires`-DAG validation, and assembling the
//! [`ApplyReport`] from the per-op outcomes the passes record — while the named
//! passes ([`crate::apply::passes::run_apply_passes`]) own the ordered
//! disk-touching work. Every document-mutation command (move, delete,
//! rewrite-wikilink, apply) builds a MigrationPlan and applies it here, emitting
//! a single ApplyReport envelope.
//!
//! # Provenance tracking
//!
//! Each ApplyOp carries a `parent_op_idx` (index into
//! `plan.operations`) so the ApplyReport can:
//! - set `from = Some(parent_idx.to_string())` for changes produced by
//!   high-level expansions (move_folder → N move_document ops)
//! - propagate the parent MigrationOp's `footnote` to each child ApplyReportOp

use crate::apply::passes::{run_apply_passes, ApplyOutcomes, CreateApplyContext, DependencyMap};
use crate::apply::preconditions::{
    build_owner_precondition_refusal_report, evaluate_owner_preconditions,
};
use crate::domain::GraphIndex;
use crate::planner::intent::{expand, HIGH_LEVEL_KINDS};
use crate::standards::apply::CascadeRecord;
use crate::standards::{
    ApplyBatch, ApplyOp, PlanWarning, RepairPlanSummary, SkippedSummary, REPAIR_PLAN_SCHEMA_VERSION,
};
use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use norn_wire::{
    ApplyReport, ApplyReportOp, ApplyReportPrecondition, ApplyWarning, CascadeFailure,
    CascadeRewrite, CascadeSkip, CascadeSummary, LinkImpact, OpStatus, PreconditionStatus,
    APPLY_REPORT_SCHEMA_VERSION,
};
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use std::collections::{BTreeSet, HashMap};

/// Context for `apply_migration_plan`.
#[derive(Default)]
pub struct ApplyContext {
    /// When true, no filesystem mutations are made; report shows what would happen.
    pub dry_run: bool,
    /// When true, create intermediate parent directories for create_document ops.
    pub parents: bool,
    /// When true, per-op cascade summaries include the full rewrite/skip lists.
    pub verbose: bool,
    /// When true, a no-op CLEAN REFUSAL (a failure
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
    /// behind a bare `Err` (CLI) or lost as a no-op `refused` (MCP). The
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
/// `ApplyOp`s; low-level ops expand to exactly one. Provenance is
/// tracked so the report can surface which parent op each change came from.
///
/// # Phase 2 — Passes
///
/// Compare-and-swap hashes arrive in the plan or not at all (ADR 0024): the
/// move/delete verbs and repair stamp `document_hash` from the index at plan
/// synthesis, content ops carry their own, and a hash-less delete is refused
/// (`delete-hash-required`) by a plan-level barrier before any write. There is no
/// apply-time live-index hydration — filling an empty hash from the same index
/// the file was read into was a tautological CAS. An `ApplyBatch` is built from
/// the expanded changes and handed to [`run_apply_passes`], which runs the named
/// passes and records each op's outcome into a tracker.
///
/// # Phase 3 — Conversion
///
/// The recorded [`ApplyOutcomes`] are assembled into an `ApplyReport` with per-op status,
/// provenance (`from`), footnote propagation, and summary lines.
/// On a clean COMMIT, the returned [`ApplyReport`] carries the changed-file set
/// on its `touched_paths` field (NRN-252 / NRN-158) — populated from
/// [`ApplyOutcomes::touched_paths`]. The MCP warm mutation tools (`move` /
/// `delete` / `rewrite_wikilink` / `apply`) feed it to their cache-increment
/// commit; the CLI ignores it (and it is `#[serde(skip)]`, so it never touches
/// the wire report). A refusal writes nothing (empty set) and a partial failure
/// leaves the next read's `detect` to heal the cache (also empty).
pub fn apply_migration_plan(
    plan: &MigrationPlan,
    index: &GraphIndex,
    ctx: ApplyContext,
    sink: &mut crate::telemetry::EventSink,
) -> Result<ApplyReport> {
    // Schema gate (audit-F3): the ENGINE is the single owner of the plan-schema
    // contract. A plan whose `schema_version` this build does not support is
    // refused here, before any work — vault-root canonicalization, expansion,
    // owner-set evaluation, or a write — so zero operations are examined. This was
    // formerly a client-side preamble check in `norn-cli`; promoting it engine-side
    // means the routed/MCP surface (which never ran the CLI preamble) is guarded
    // too, and the check exists exactly once. `unsupported-schema-version`, exit 2.
    if plan.schema_version != MIGRATION_PLAN_SCHEMA_VERSION {
        let error = anyhow::Error::from(
            crate::standards::apply::ApplyError::UnsupportedMigrationPlanSchemaVersion {
                expected: MIGRATION_PLAN_SCHEMA_VERSION,
                got: plan.schema_version,
            },
        );
        return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error);
    }
    let vault_root = Utf8PathBuf::from(&plan.vault_root);
    // Canonicalize the index root. A failure here means the vault root does not
    // exist or cannot be read — a missing/unreadable `-C`/NORN_ROOT root, or a
    // registered root that vanished after resolution. That is a USER fault
    // (NRN-414), not a norn bug: code it `vault-root-unreadable` so the refusal
    // is machine-branchable and names the root, instead of flattening to
    // `internal-error`. Like every pre-write barrier below, cross it through
    // `refuse_plan_level` so the routed/MCP surface gets a coded, report-shaped
    // refusal rather than a bare Err.
    let canonical_index_root = match index.root.as_std_path().canonicalize() {
        Ok(root) => root,
        Err(e) => {
            let error =
                anyhow::Error::from(crate::standards::apply::ApplyError::VaultRootUnreadable {
                    path: index.root.clone(),
                    detail: e.to_string(),
                });
            return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error);
        }
    };
    // The plan's `vault_root` is author-supplied: if it does not canonicalize to
    // the index root — whether it names a DIFFERENT directory or does not exist at
    // all — that is a `vault-root-mismatch` refusal. Keep the coded error even
    // when the plan root can't be canonicalized, so a bare IO failure never
    // launders it into a generic `internal-error`.
    let plan_root_matches = vault_root
        .as_std_path()
        .canonicalize()
        .is_ok_and(|canonical_plan_root| canonical_plan_root == canonical_index_root);
    if !plan_root_matches {
        let error = anyhow::anyhow!(crate::standards::apply::ApplyError::VaultRootMismatch {
            plan: vault_root.clone(),
            cwd: index.root.clone(),
        });
        return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error);
    }

    // Delete-hash REQUIRED (NRN-151, ADR 0024): a `delete_document` op with no
    // plan-time `document_hash` cannot compare-and-swap the file before removing
    // it — a delete without a CAS is a fail-open removal. Refuse it whole-plan
    // before any write (`delete-hash-required`, exit 2), keyed at the offending
    // op so the report names it. The verbs and repair always stamp a hash, so
    // only a hand-authored hash-less delete newly refuses. Move stays optional.
    if let Some((op_idx, error)) = first_delete_missing_hash(plan) {
        if ctx.refuse_as_report {
            return Ok(build_plan_refusal_report(
                plan,
                ctx.dry_run,
                op_idx,
                crate::apply::envelope::from_anyhow(&error),
            ));
        }
        return Err(error);
    }

    // ------------------------------------------------------------------
    // Phase 1: expansion + provenance tracking
    // ------------------------------------------------------------------

    // `all_changes[i]` came from `plan.operations[provenance[i]]`.
    let mut all_changes: Vec<ApplyOp> = Vec::new();
    let mut provenance: Vec<usize> = Vec::new(); // change idx → parent op idx

    for (i, op) in plan.operations.iter().enumerate() {
        let expanded = match expand(op, index) {
            Ok(expanded) => expanded,
            Err(e) => {
                // Expansion is pure (index-only, no filesystem write), so ANY
                // failure here is provably PRE-WRITE — the vault is unchanged
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
                        crate::apply::envelope::from_anyhow(&e),
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
    // Create-path resolution (`{{seq}}` allocation, vault-root containment,
    // duplicate op ids, missing stems) is pure and pre-write, so any failure is
    // provably unchanged — cross it as a coded refusal on the routed/MCP
    // surface, exactly like the expand() barrier above.
    let resolved_creates = match resolve_create_paths(
        plan,
        index,
        &canonical_index_root,
        &mut all_changes,
        &provenance,
    ) {
        Ok(resolved) => resolved,
        Err(error) => return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error),
    };
    let owner_index = if plan.preconditions.is_empty() {
        None
    } else {
        match crate::graph::build_index_with_options(&index.root, &ctx.owner_index_options) {
            Ok(owner_index) => Some(owner_index),
            Err(error) => {
                return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error.into())
            }
        }
    };
    // Owner-precondition VALIDATION errors (duplicate precondition id, empty
    // stem/eq selector, a `stem_from_operation` referencing a missing/non-create
    // op) are raised before the barrier writes anything — cross them as coded
    // refusals too. The owner-set MISMATCH case is Ok(...) and handled below.
    let preconditions = match evaluate_owner_preconditions(
        plan,
        owner_index.as_ref().unwrap_or(index),
        &resolved_creates.stems_by_operation,
        &resolved_creates.changes_by_stem,
    ) {
        Ok(preconditions) => preconditions,
        Err(error) => return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error),
    };
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

    // Ordering is a constrained DAG (ADR 0024): validate `requires` before any
    // write. An edge referencing an unknown op id, or a cycle, is a `malformed-plan`
    // whole-plan refusal — crossed like every other pre-write barrier so the routed/
    // MCP surface reconstructs the exit-2 refusal too. A plan declaring no `requires`
    // passes trivially.
    if let Err(error) = validate_requires_dag(plan) {
        return refuse_plan_level(plan, ctx.dry_run, ctx.refuse_as_report, error);
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

    // change_id -> op span id, zipped 1:1 from `all_changes` (the span_ids source).
    let spans: std::collections::HashMap<String, String> = all_changes
        .iter()
        .zip(span_ids.iter())
        .map(|(c, s)| (c.change_id.clone(), s.clone()))
        .collect();

    // ------------------------------------------------------------------
    // Phase 2: the one applier (ADR 0024)
    // ------------------------------------------------------------------
    // Compare-and-swap hashes arrive IN THE PLAN or not at all (ADR 0024): the
    // move/delete verbs and the repair planner stamp `document_hash` from the
    // index at plan-synthesis time, and content ops carry their own. There is no
    // apply-time live-index hydration — filling an empty hash from the very index
    // the file was just read into was a tautological CAS (it could never detect
    // drift within a single command). A hash-less delete is refused above
    // (`delete-hash-required`); a hash-less move / content op simply skips the CAS.

    let batch = ApplyBatch {
        schema_version: REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.clone(),
        summary: RepairPlanSummary {
            findings: all_changes.len(),
            planned_changes: all_changes.len(),
            skipped: SkippedSummary::default(),
        },
        changes: all_changes.clone(),
    };

    let create_ctx = CreateApplyContext {
        parents: ctx.parents,
        // NRN-265: every `{{seq}}` create was already resolved to a concrete
        // path by `resolve_create_paths` above (which rewrote `change.path`
        // under the owner-set barrier). Declare it so the create pass's seq
        // branch — unreachable on this path — fails closed if a `{{seq}}` token
        // ever survives.
        creates_preresolved: true,
        // NRN-138 ignore re-check applies to `new`-synthesized create_document
        // changes; the migration-plan create ops routed through here have no such
        // guard to backstop, so leave this empty.
        ..Default::default()
    };

    // The `requires` DAG, projected onto the expanded interior change ids.
    let deps = build_dependency_map(plan, &all_changes, &provenance);

    // Run the one applier. Per-op failures are recorded in `outcomes.tracker`;
    // an `Err` here is a plan-level barrier (schema, vault-root, vacate,
    // duplicate-field, containment) — a no-op whole-plan refusal, raised
    // before the first write.
    let mut outcomes = match run_apply_passes(
        &vault_root,
        index,
        &batch,
        ctx.dry_run,
        &create_ctx,
        sink,
        &spans,
        &deps,
    ) {
        Ok(o) => o,
        Err(e) => {
            // A barrier refusal is pre-write: nothing landed, the vault is
            // unchanged. Return-report-on-refusal when the
            // caller opted in; otherwise (CLI) propagate the `Err`.
            if ctx.refuse_as_report {
                let rich = e.downcast_ref::<crate::standards::apply::ApplyError>();
                let envelope = rich
                    .map(crate::apply::envelope::from_rich)
                    .unwrap_or_else(|| crate::apply::envelope::from_anyhow(&e));
                let error_path = rich.and_then(|r| r.path().map(|p| p.to_path_buf()));
                return Ok(build_refusal_report(
                    plan,
                    &all_changes,
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

    // ------------------------------------------------------------------
    // Refused-vs-partial (ADR 0024): the runtime write-state, tracked truly.
    // ------------------------------------------------------------------
    // A per-op failure with NO write landed is a CLEAN REFUSAL (a single-op
    // precondition, a stale-hash delete, a create guard): the vault is
    // unchanged. Once any write has landed, a failure is a truthful
    // PARTIAL FAILURE assembled below (`outcome = failed`, exit 1), never a
    // clean `refused`.
    if outcomes.tracker.failed_count() > 0 && !outcomes.tracker.wrote_any() {
        let representative = outcomes
            .tracker
            .first_failure()
            .expect("failed_count > 0 guarantees a representative failure");
        let rich = representative.downcast_ref::<crate::standards::apply::ApplyError>();
        let envelope = rich
            .map(crate::apply::envelope::from_rich)
            .unwrap_or_else(|| crate::apply::envelope::from_anyhow(representative));
        let error_path = rich.and_then(|r| r.path().map(|p| p.to_path_buf()));
        if ctx.refuse_as_report {
            return Ok(build_refusal_report(
                plan,
                &all_changes,
                &provenance,
                ctx.dry_run,
                envelope,
                error_path.as_deref(),
                preconditions.clone(),
            ));
        }
        // CLI: re-raise the representative rich error so the arm renders its own
        // structured envelope and exits 2 — identical to the pre-fold path.
        return Err(outcomes
            .tracker
            .take_first_failure()
            .expect("first_failure was Some"));
    }

    // ------------------------------------------------------------------
    // Phase 4: assemble the ApplyReport from the recorded per-op outcomes.
    // ------------------------------------------------------------------

    let ops = build_report_ops(
        &all_changes,
        &provenance,
        &plan.operations,
        &outcomes,
        ctx.dry_run,
        ctx.verbose,
        index,
    );

    // Outcome keys off the runtime op-failure count; the applied/skipped/remaining
    // tallies are computed once, canonically, inside `assemble_report`.
    let failed = ops
        .iter()
        .filter(|o| matches!(o.status, OpStatus::Failed))
        .count();

    // A clean commit carries the touched-file set for the caller's increment
    // commit (NRN-252 / NRN-158); a partial failure carries none (the next read's
    // `detect` heals the cache).
    let touched_paths = if failed > 0 {
        Vec::new()
    } else {
        outcomes.touched_paths()
    };

    let warnings: Vec<ApplyWarning> = outcomes
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

    // NRN-183 / NRN-161: collapse the exit states into one machine-readable
    // field. A runtime op-failure (`failed > 0`) maps to `failed` (exit 1); a
    // clean dry-run preview to `forecast` (exit 0, wrote nothing); a clean
    // confirmed write to `applied` (exit 0). The `refused` outcome (exit 2) is
    // produced only by `build_refusal_report` on a precondition refusal — a
    // dry-run that WOULD refuse still reaches that path (any tracked failure with
    // nothing written diverts above), so `forecast` here means "would apply
    // cleanly", never "would refuse".
    let outcome = if failed > 0 {
        norn_wire::ApplyOutcome::Failed
    } else if ctx.dry_run {
        norn_wire::ApplyOutcome::Forecast
    } else {
        norn_wire::ApplyOutcome::Applied
    };

    // Dry-runs persist no log, so the trace_id correlates to nothing — emit
    // empty for symmetry with SetReport/NewReport dry-run output.
    let trace_id = if ctx.dry_run {
        String::new()
    } else {
        sink.trace_id().to_string()
    };
    // A dry-run never touches the sink's durable writer, so it is never
    // "degraded" in a way worth surfacing; a confirmed apply mirrors the
    // sink's own degraded flag straight through (NRN-400 review: the operator
    // advisory).
    let telemetry_degraded = !ctx.dry_run && sink.degraded();
    Ok(assemble_report(
        plan,
        ctx.dry_run,
        trace_id,
        telemetry_degraded,
        ops,
        preconditions,
        warnings,
        outcome,
        touched_paths,
    ))
}

/// The single canonical [`ApplyReport`] constructor (one-obvious-way, NRN-386).
///
/// Computes the `applied` / `skipped` / `failed` / `remaining` tallies from `ops`
/// so the four former hand-synced builder sites — the clean-commit success path,
/// the pre-write refusal, the plan-level refusal, and the partial-failure report —
/// cannot drift on the count arithmetic or the common envelope fields
/// (`schema_version` / `plan_hash` / `vault_root`). Callers supply only what
/// varies between them: the op vec, the outcome, the trace id, the warnings, and
/// the touched-path set. Dry-run/apply parity holds by construction because every
/// site tallies through this one function.
#[allow(clippy::too_many_arguments)]
fn assemble_report(
    plan: &MigrationPlan,
    dry_run: bool,
    trace_id: String,
    telemetry_degraded: bool,
    ops: Vec<ApplyReportOp>,
    preconditions: Vec<ApplyReportPrecondition>,
    warnings: Vec<ApplyWarning>,
    outcome: norn_wire::ApplyOutcome,
    touched_paths: Vec<Utf8PathBuf>,
) -> ApplyReport {
    let count = |status: OpStatus| ops.iter().filter(|o| o.status == status).count();
    let applied = count(OpStatus::Applied);
    let skipped = count(OpStatus::Skipped);
    let failed = count(OpStatus::Failed);
    let remaining = count(OpStatus::NotRun);
    ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        trace_id,
        telemetry_degraded,
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        dry_run,
        applied,
        skipped,
        failed,
        remaining,
        preconditions,
        operations: ops,
        warnings,
        outcome,
        touched_paths,
    }
}

struct ResolvedCreatePaths {
    stems_by_operation: HashMap<String, String>,
    changes_by_stem: HashMap<String, BTreeSet<usize>>,
}

fn resolve_create_paths(
    plan: &MigrationPlan,
    index: &GraphIndex,
    canonical_root: &std::path::Path,
    changes: &mut [ApplyOp],
    provenance: &[usize],
) -> Result<ResolvedCreatePaths> {
    let mut stems = HashMap::new();
    let mut create_changes_by_stem: HashMap<String, BTreeSet<usize>> = HashMap::new();
    let mut allocated_this_plan: Vec<Utf8PathBuf> = Vec::new();
    let mut operation_ids = BTreeSet::new();
    for operation in &plan.operations {
        if let Some(id) = operation.id.as_ref() {
            if !operation_ids.insert(id) {
                return Err(
                    crate::standards::apply::PlanStructureError::DuplicateOperationId {
                        id: id.clone(),
                    }
                    .into(),
                );
            }
        }
    }

    for (change_index, (change, parent_index)) in changes.iter_mut().zip(provenance).enumerate() {
        if change.operation != "create_document" {
            continue;
        }
        let path = change.path.clone();
        crate::apply::fsops::ensure_within_vault(&index.root, canonical_root, &path)?;
        let resolved = if crate::seq_alloc::has_seq(&path) {
            // A `{{seq}}` outside the file name, or a second occurrence, is the
            // author's plan-structure fault — refuse typed (`malformed-plan`)
            // rather than letting seq_alloc's backstop launder it to
            // `internal-error`.
            if crate::seq_alloc::seq_misplaced(&path) {
                return Err(crate::standards::apply::PlanStructureError::SeqMisplaced {
                    path: path.clone(),
                }
                .into());
            }
            crate::seq_alloc::resolve_seq_create(&index.root, &path, &allocated_this_plan)?
        } else {
            path
        };
        allocated_this_plan.push(resolved.clone());
        change.path = resolved.clone();

        let stem = resolved.file_stem().ok_or_else(|| {
            anyhow::Error::from(
                crate::standards::apply::PlanStructureError::CreatePathNoStem {
                    path: resolved.clone(),
                },
            )
        })?;
        create_changes_by_stem
            .entry(stem.to_ascii_lowercase())
            .or_default()
            .insert(change_index);
        if let Some(id) = plan.operations[*parent_index].id.as_ref() {
            stems.insert(id.clone(), stem.to_string());
        }
    }

    Ok(ResolvedCreatePaths {
        stems_by_operation: stems,
        changes_by_stem: create_changes_by_stem,
    })
}

/// Build a return-report-on-refusal from a PRE-WRITE refusal
/// (`wrote_any == false` proves the vault is unchanged). Every expanded
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
/// renders on the `Err` path — so a routed refusal renders identically to
/// Direct's exit-2 refusal.
fn build_refusal_report(
    plan: &MigrationPlan,
    changes: &[ApplyOp],
    provenance: &[usize],
    dry_run: bool,
    envelope: norn_wire::ApplyError,
    error_path: Option<&Utf8Path>,
    preconditions: Vec<ApplyReportPrecondition>,
) -> ApplyReport {
    use norn_wire::ApplyOutcome;

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
                // A clean refusal writes nothing and renders no records
                // link-impact line — the coded error is the whole output.
                link_impact: None,
                // Finding linkage echoes on the ACTIONED report (build_report_ops);
                // a refusal carries only its coded error, no provenance.
                finding_code: None,
                repair_rule: None,
            }
        })
        .collect();

    // A refusal writes nothing: exactly one op `failed`, the rest `not_run`, no
    // touched paths. Tallies and the common envelope come from `assemble_report`.
    // No sink was ever consulted for a refusal, so `telemetry_degraded` stays
    // false — mirroring the empty `trace_id`.
    assemble_report(
        plan,
        dry_run,
        String::new(),
        false,
        ops,
        preconditions,
        Vec::new(),
        ApplyOutcome::Refused,
        Vec::new(),
    )
}

/// Build a pre-write refusal report for an EXPANSION-PHASE failure (NRN-231
/// review F1), before any `ApplyOp` exists. Expansion is pure (index-only,
/// no filesystem write), so this is a pure no-op: every plan operation becomes
/// a `not_run` op EXCEPT `failed_op_idx`, which is `failed` carrying the
/// structured `error` envelope. `outcome = refused` (exit 2). Mirrors
/// [`build_refusal_report`] but keys off `plan.operations` rather than expanded
/// changes, since expansion is exactly what failed.
/// Cross a PRE-WRITE, plan-level refusal (vault-root containment, create-path
/// resolution, owner-precondition validation) the same way the expand() barrier
/// does: under `refuse_as_report` (the daemon/MCP surface, ADR 0011) return a
/// coded, report-shaped refusal so a routed apply reconstructs the exact exit-2
/// refusal the direct arm renders; otherwise (the CLI) propagate the bare `Err`
/// so the arm renders the structured envelope itself and exits 2. `error` already
/// carries its stable coded form (`ApplyError::from_anyhow` recovers the typed
/// `vault-root-mismatch` / `containment-*` codes; anything else is
/// `internal-error` + the `{e:#}` message, exactly what the CLI's `Err` path
/// renders).
fn refuse_plan_level(
    plan: &MigrationPlan,
    dry_run: bool,
    refuse_as_report: bool,
    error: anyhow::Error,
) -> Result<ApplyReport> {
    if refuse_as_report {
        Ok(build_plan_refusal_report(
            plan,
            dry_run,
            0,
            crate::apply::envelope::from_anyhow(&error),
        ))
    } else {
        Err(error)
    }
}

fn build_plan_refusal_report(
    plan: &MigrationPlan,
    dry_run: bool,
    failed_op_idx: usize,
    envelope: norn_wire::ApplyError,
) -> ApplyReport {
    use norn_wire::ApplyOutcome;

    let mut ops: Vec<ApplyReportOp> = plan
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
                finding_code: None,
                repair_rule: None,
            }
        })
        .collect();

    // A refused report MUST carry a coded error (reconstruct_wire_report enforces
    // it). A preconditions-only plan (zero operations, e.g. a vault-root or
    // owner-validation refusal) — or an out-of-range `failed_op_idx` — would
    // otherwise mark no op failed and lose the code, so synthesize one failed op.
    if !ops.iter().any(|op| op.status == OpStatus::Failed) {
        ops.push(ApplyReportOp {
            op_id: ops.len().to_string(),
            kind: "apply".to_string(),
            status: OpStatus::Failed,
            from: None,
            path: envelope.path.clone(),
            stem: None,
            summary: envelope.message.clone(),
            error: Some(envelope.clone()),
            footnote: None,
            cascade: None,
            link_impact: None,
            finding_code: None,
            repair_rule: None,
        });
    }

    // A refusal writes nothing; tallies + envelope come from `assemble_report`.
    // No sink was ever consulted, so `telemetry_degraded` stays false.
    assemble_report(
        plan,
        dry_run,
        String::new(),
        false,
        ops,
        Vec::new(),
        Vec::new(),
        ApplyOutcome::Refused,
        Vec::new(),
    )
}

/// Finding-provenance linkage echoed verbatim from an op onto its apply-report
/// record (ADR 0022): `(finding_code, repair_rule)` read from the resolved
/// [`TypedOp`] — the authoritative per-op declaration of what the op is resolving.
/// Repair-generated ops carry the real codes; verb-synthesized and authored ops
/// declare none, yielding `(None, None)` so their report bytes are unchanged.
///
/// Every op kind that can carry linkage now models it in the typed vocabulary —
/// change ops in [`ChangeFields`](norn_wire::ChangeFields) and, since ADR 0024,
/// the structural [`MoveDocumentFields`](norn_wire::MoveDocumentFields) /
/// [`DeleteDocumentFields`](norn_wire::DeleteDocumentFields) — so the report reads
/// the TYPED op uniformly rather than indexing raw `fields`. A decode failure
/// (an op refused at expansion) never reaches the report path; the defensive
/// `Err` arm yields no linkage. Edit / move-folder / rewrite-wikilink ops carry
/// no linkage.
fn op_finding_linkage(op: &MigrationOp) -> (Option<String>, Option<String>) {
    use norn_wire::TypedOp;
    match TypedOp::try_from(op) {
        Ok(TypedOp::Change(c)) => (c.fields.finding_code, c.fields.repair_rule),
        Ok(TypedOp::MoveDocument(f)) => (f.finding_code, f.repair_rule),
        Ok(TypedOp::DeleteDocument(f)) => (f.finding_code, f.repair_rule),
        _ => (None, None),
    }
}

/// Best-effort display token for a raw `MigrationOp` (an expansion-phase refusal
/// has no `ApplyOp` to summarize). Reads the common `path`/`src` fields;
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

/// The first `delete_document` op (by plan order) carrying no plan-time
/// `document_hash`, paired with the `delete-hash-required` refusal (NRN-151, ADR
/// 0024). A delete's hash is the compare-and-swap precondition that guarantees the
/// file removed is the file reviewed; without it the delete is fail-open, so the
/// whole plan is refused before any write. A `null`, absent, OR empty-string hash
/// all count as missing — an empty hash would silently skip the CAS
/// (`fingerprint_vacate` treats `""` as "no check"), the exact fail-open hole this
/// barrier closes. An op whose `fields` do not even decode is left for the
/// expansion phase to refuse with its own coded error.
fn first_delete_missing_hash(plan: &MigrationPlan) -> Option<(usize, anyhow::Error)> {
    use norn_wire::TypedOp;
    for (i, op) in plan.operations.iter().enumerate() {
        if op.kind != "delete_document" {
            continue;
        }
        if let Ok(TypedOp::DeleteDocument(f)) = TypedOp::try_from(op) {
            if f.document_hash.as_deref().unwrap_or("").is_empty() {
                let error = anyhow::Error::from(
                    crate::standards::apply::PlanStructureError::DeleteHashRequired {
                        path: Utf8PathBuf::from(f.path),
                    },
                );
                return Some((i, error));
            }
        }
    }
    None
}

/// Validate the plan's `requires` DAG (ADR 0024) before any write: every edge
/// must reference an existing op id, and the edges must be acyclic. Either fault
/// is a `malformed-plan` whole-plan refusal ([`PlanStructureError`] family).
/// `requires` constrains outcome propagation within the fixed kind-ordered passes;
/// this only rejects a structurally broken DAG — it does not reorder anything.
fn validate_requires_dag(plan: &MigrationPlan) -> Result<()> {
    use crate::standards::apply::PlanStructureError;
    let ids: BTreeSet<&str> = plan
        .operations
        .iter()
        .filter_map(|op| op.id.as_deref())
        .collect();
    // Unknown-reference check across every op (named or not).
    for op in &plan.operations {
        for req in &op.requires {
            if !ids.contains(req.as_str()) {
                return Err(PlanStructureError::RequiresUnknownOp {
                    op: op.id.clone().unwrap_or_default(),
                    requires: req.clone(),
                }
                .into());
            }
        }
    }
    // Cycle detection over the op-id graph (op -> each of its requires), via
    // iterative DFS with white/gray/black coloring.
    let by_id: HashMap<&str, &MigrationOp> = plan
        .operations
        .iter()
        .filter_map(|op| op.id.as_deref().map(|id| (id, op)))
        .collect();
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: HashMap<&str, Color> = by_id.keys().map(|k| (*k, Color::White)).collect();
    for start in by_id.keys().copied() {
        if color[start] != Color::White {
            continue;
        }
        color.insert(start, Color::Gray);
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        while let Some(&(node, idx)) = stack.last() {
            let reqs = &by_id[node].requires;
            if idx < reqs.len() {
                stack.last_mut().unwrap().1 += 1;
                let next = reqs[idx].as_str();
                match color.get(next).copied().unwrap_or(Color::Black) {
                    Color::White => {
                        color.insert(next, Color::Gray);
                        stack.push((next, 0));
                    }
                    Color::Gray => {
                        return Err(PlanStructureError::RequiresCycle {
                            op: next.to_string(),
                        }
                        .into());
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
            }
        }
    }
    Ok(())
}

/// Project the plan's `requires` DAG onto the expanded interior change ids so the
/// passes propagate outcomes: `change_requires[cid]` is the op ids the change's
/// parent op requires; `op_changes[op_id]` is the change ids that op expanded to.
fn build_dependency_map(
    plan: &MigrationPlan,
    changes: &[ApplyOp],
    provenance: &[usize],
) -> DependencyMap {
    let mut change_requires: HashMap<String, Vec<String>> = HashMap::new();
    let mut op_changes: HashMap<String, Vec<String>> = HashMap::new();
    for (i, change) in changes.iter().enumerate() {
        let parent = &plan.operations[provenance[i]];
        if let Some(id) = parent.id.as_ref() {
            op_changes
                .entry(id.clone())
                .or_default()
                .push(change.change_id.clone());
        }
        if !parent.requires.is_empty() {
            change_requires.insert(change.change_id.clone(), parent.requires.clone());
        }
    }
    DependencyMap::new(change_requires, op_changes)
}

/// Build a one-liner summary for an `ApplyReportOp`.
///
/// `create_display` overrides the path shown for a `create_document` op — used
/// to surface the apply-time-resolved `{{seq}}` id (NRN-101) instead of the
/// unresolved template `change.path`.
fn build_summary(
    change: &ApplyOp,
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
/// multiple `ApplyOp`s). Used to set the `from` field in ApplyReportOp.
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
///   resolved `.md` path, identical to the direct arm.
fn build_link_impact(change: &ApplyOp, parent_op: &MigrationOp, index: &GraphIndex) -> LinkImpact {
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

fn build_report_ops(
    changes: &[ApplyOp],
    provenance: &[usize],
    plan_ops: &[MigrationOp],
    outcomes: &ApplyOutcomes,
    dry_run: bool,
    verbose: bool,
    index: &GraphIndex,
) -> Vec<ApplyReportOp> {
    // NRN-101: create_document ops are recorded in `created_documents` in the
    // same order they appear here, each carrying its apply-time-resolved
    // `{{seq}}` path. Walk that list in lockstep so summaries show the real
    // (or dry-run predicted) id, not the unresolved template.
    let mut created_iter = outcomes.created_documents.iter();
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

            // True per-op status (ADR 0024): read directly from the tracker the
            // passes recorded into, not reconstructed from the event log. A
            // dry-run previews nothing, so every op renders `not_run`.
            let status = if dry_run {
                OpStatus::NotRun
            } else {
                outcomes.tracker.status(&change.change_id)
            };

            // A failed op carries its coded error envelope; the representative op
            // of a multi-op file failure holds the rich error, its siblings none.
            let error = if status == OpStatus::Failed {
                outcomes.tracker.error(&change.change_id).map(|e| {
                    e.downcast_ref::<crate::standards::apply::ApplyError>()
                        .map(crate::apply::envelope::from_rich)
                        .unwrap_or_else(|| crate::apply::envelope::from_anyhow(e))
                })
            } else {
                None
            };

            // Lockstep invariant (NRN-175 / F6): `created_documents` holds one
            // entry per create_document that actually PRODUCED a file — every
            // applied create in a real apply, and every predicted create in a
            // dry-run. Consume the iterator ONLY for such ops; a create that
            // failed or was not_run leaves path/stem ABSENT (absent-not-stale)
            // rather than misattributing a sibling's created path to it.
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

            // Cascades are DERIVED at apply and recorded per source path (ADR
            // 0024): the summary folds directly from the recorded `CascadeRecord`
            // — its post-retry settled `rewritten`/`skipped`/`failed` — on both
            // dry-run (forecast) and apply, never from the event log. A move/delete
            // with no cascade record (no backlinks) folds to an all-zero summary.
            let cascade = match change.operation.as_str() {
                "move_document" | "delete_document" => {
                    let rec = outcomes
                        .cascades
                        .iter()
                        .find(|c| c.source_path == change.path);
                    Some(build_cascade_summary(rec, verbose))
                }
                _ => None,
            };

            // NRN-237: index-derived incoming-link impact for a `delete_document`
            // op, so the `--format records` renderer's inputs ride the wire report
            // and the routed path reproduces the direct path exactly. This
            // reproduces the CLI arm's former locals EXACTLY: `backlinks` for the
            // count, BTreeSet-distinct sorted source paths for the files, the
            // `link_risk`-sources fallback when `--rewrite-to` is set and no
            // backlink resolves, and the index-RESOLVED redirect target. The index
            // view is identical on dry-run and confirm, so populate on both.
            let link_impact = (change.operation == "delete_document")
                .then(|| build_link_impact(change, parent_op, index));

            // ADR 0022: echo the op's finding-provenance linkage verbatim. Absent
            // on verb-synthesized/authored ops (→ None → omitted), present on
            // repair-sourced ops; provenance only, never read for apply behavior.
            let (finding_code, repair_rule) = op_finding_linkage(parent_op);

            ApplyReportOp {
                op_id: i.to_string(),
                kind: change.operation.clone(),
                status,
                from,
                path,
                stem,
                summary,
                error,
                footnote: parent_op.footnote.clone(),
                cascade,
                link_impact,
                finding_code,
                repair_rule,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{Clock, EventSink, IdGen};
    use camino::Utf8Path;
    use norn_wire::{MigrationOp, MigrationPlan};

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
    fn apply_against_a_vanished_root_refuses_vault_root_unreadable_not_internal_error() {
        // NRN-414: the index root vanished after resolution (a bad -C/NORN_ROOT,
        // or a TOCTOU delete between resolution and apply). The canonicalize
        // barrier classifies it as a USER fault carrying `vault-root-unreadable`,
        // NOT the bare-anyhow `internal-error` it used to flatten to — and the
        // routed/MCP surface gets it as a coded, report-shaped refusal (nothing
        // written) rather than a bare Err.
        let (tmp, mut index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        // Point the index at a root that does not exist on any host.
        index.root = camino::Utf8PathBuf::from(format!("{vault_root}/nrn414-vanished-root"));
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
            // The routed/MCP surface: refusals cross as a report, not a bare Err.
            refuse_as_report: true,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        let failed = report
            .operations
            .iter()
            .find(|op| op.status == norn_wire::OpStatus::Failed)
            .expect("a vanished root must produce a failed op carrying the coded envelope");
        let error = failed.error.as_ref().expect("the failed op carries a code");
        assert_eq!(error.code, "vault-root-unreadable");
        // Nothing was written: the original file is untouched.
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
            norn_wire::OpStatus::Applied
        ));
        // Apply: file moved
        assert!(!tmp.path().join("a.md").exists());
        assert!(tmp.path().join("renamed.md").exists());
    }

    #[test]
    fn apply_report_echoes_finding_linkage_from_op_only_when_present() {
        // ADR 0022: an op carrying finding-provenance linkage echoes it verbatim
        // onto its report record; a sibling op without linkage leaves the fields
        // absent (None), so verb-driven report bytes are unchanged.
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![
                MigrationOp {
                    kind: "add_frontmatter".into(),
                    id: None,
                    requires: vec![],
                    fields: serde_json::json!({
                        "path": "a.md",
                        "field": "priority",
                        "new_value": "high",
                        "finding_code": "missing-required-field",
                        "repair_rule": "add-default"
                    }),
                    footnote: None,
                },
                MigrationOp {
                    kind: "add_frontmatter".into(),
                    id: None,
                    requires: vec![],
                    fields: serde_json::json!({
                        "path": "b.md",
                        "field": "priority",
                        "new_value": "low"
                    }),
                    footnote: None,
                },
            ],
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
        assert_eq!(report.operations.len(), 2);
        // The repair-provenance op echoes the linkage verbatim.
        assert_eq!(
            report.operations[0].finding_code.as_deref(),
            Some("missing-required-field")
        );
        assert_eq!(
            report.operations[0].repair_rule.as_deref(),
            Some("add-default")
        );
        // The linkage-free op carries None → the JSON omits the fields entirely.
        assert_eq!(report.operations[1].finding_code, None);
        assert_eq!(report.operations[1].repair_rule, None);
        let op1_json = serde_json::to_value(&report.operations[1]).unwrap();
        assert!(op1_json.get("finding_code").is_none());
        assert!(op1_json.get("repair_rule").is_none());
    }

    #[test]
    fn deserialized_plan_still_skips_unrepresentable_target_backlink() {
        // NRN-424 serialization-trust: the cascade's `link_risk` (and its
        // `#[serde(skip)]` `unrepresentable` flag) is NEVER carried on the wire —
        // the MigrationPlan's move_document op holds only src/dst, and `expand()`
        // RE-DERIVES `link_risk` via `classify_link_risk` against the live index at
        // apply time (cascades are DERIVED at apply, NRN-406). So even a plan that
        // is serialized to JSON and read back must still skip an unrepresentable
        // rename rather than corrupt the backlink. Round-trip the plan to prove the
        // recompute path holds and the persisted flag is irrelevant.
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
                // `a|b` is not a representable wikilink target (the `|` begins an
                // alias), so rewriting `[[a]]` to it would corrupt the backlink.
                fields: serde_json::json!({"src": "a.md", "dst": "a|b.md"}),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        // The round-trip: serialize to the wire JSON and read it back. If the
        // `unrepresentable` decision rode on the (skipped) serialized flag, it
        // would be lost here and the backlink would be corrupted below.
        let wire = serde_json::to_string(&plan).unwrap();
        let plan: MigrationPlan = serde_json::from_str(&wire).unwrap();
        assert!(
            !wire.contains("unrepresentable") && !wire.contains("link_risk"),
            "cascade link data must not appear on the wire plan"
        );

        let ctx = ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
            refuse_as_report: false,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        assert_eq!(report.applied, 1, "the move itself still applies");
        // The file moved to the (odd but legal) destination.
        assert!(!tmp.path().join("a.md").exists());
        assert!(tmp.path().join("a|b.md").exists());
        // The backlink is left INTACT (`[[a]]`), NOT corrupted to `[[a|b]]`.
        let b = std::fs::read_to_string(tmp.path().join("b.md")).unwrap();
        assert!(
            b.contains("[[a]]") && !b.contains("[[a|b]]"),
            "the unrepresentable-target backlink must be skipped, not corrupted: {b}"
        );
    }

    #[test]
    fn applier_preresolves_seq_create_before_delegate() {
        // NRN-265: the MigrationPlan applier resolves `{{seq}}` in
        // `resolve_create_paths` and rewrites `change.path` before delegating,
        // so the create lands at the concrete `logs/1.md` and the delegate's
        // `creates_preresolved` guard never fires on the normal path (a fired
        // guard would surface as an Err here instead of a clean apply).
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root: vault_root.clone(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![MigrationOp {
                kind: "create_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({
                    "path": "logs/{{seq}}.md",
                    "new_value": { "frontmatter": {"type": "note"}, "body": "# L\n" }
                }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };
        let ctx = ApplyContext {
            dry_run: false,
            parents: true,
            verbose: false,
            refuse_as_report: false,
            owner_index_options: Default::default(),
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        assert_eq!(report.applied, 1, "seq create must apply cleanly");
        assert!(
            tmp.path().join("logs/1.md").exists(),
            "seq must resolve to logs/1.md via the applier pre-resolver"
        );
        assert!(
            report.operations[0].summary.contains("logs/1.md"),
            "op summary must reflect the resolved path, got: {}",
            report.operations[0].summary
        );
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
        // Delete now requires a plan-time hash (NRN-151); stamp the real one so
        // the barrier + CAS pass and the link_impact assertions are exercised.
        let bhash = index
            .documents
            .iter()
            .find(|d| d.path == "x/b.md")
            .unwrap()
            .hash
            .clone();
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
                    "document_hash": bhash,
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
        // OpStatus, ApplyOutcomes, ApplyOp, MigrationOp, Utf8PathBuf are all in
        // scope via `super::*`; import only what is not.
        use crate::standards::apply::CreateDocumentResult;

        // Minimal create_document ApplyOp (path/stem for a create come from
        // `created_documents`, not from the change itself, so `path` is a filler).
        fn create_change(change_id: &str) -> ApplyOp {
            ApplyOp {
                change_id: change_id.into(),
                path: Utf8PathBuf::from("filler.md"),
                document_hash: String::new(),
                finding_code: None,
                finding_rule: None,
                repair_rule: None,
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

        // Two create ops. Only op1 (change_id "c1") realized: the tracker records
        // c0 `skipped` and c1 `applied`, and `created_documents` holds exactly ONE
        // entry (op1's resolved path).
        let changes = vec![create_change("c0"), create_change("c1")];
        let provenance = vec![0usize, 1usize];
        let plan_ops = vec![create_op(), create_op()];

        let mut tracker = crate::apply::passes::OpTracker::default();
        tracker.skipped("c0");
        tracker.applied("c1");
        let outcomes = ApplyOutcomes {
            created_documents: vec![CreateDocumentResult {
                path: Utf8PathBuf::from("tasks/created-b.md"),
            }],
            tracker,
            ..Default::default()
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
            &outcomes,
            false, // not dry-run
            false, // not verbose
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
    ///   report never implies an unchanged vault.
    ///
    /// (The delete targets an UNTRACKED path — `ghost.md` — which fails Phase B's
    /// `ensure_known_path` (`unknown-path`) BEFORE its bytes are fingerprinted, so
    /// a barrier-satisfying placeholder hash never gets CAS'd. `unknown-path` is
    /// the reachable Phase-B `is_precondition` failure that reproduces the exact
    /// re-raise-after-write shape.)
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
                    fields: serde_json::json!({ "path": "ghost.md", "document_hash": "0000000000000000000000000000000000000000000000000000000000000000" }),
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
            norn_wire::ApplyOutcome::Failed,
            "a write landed before the abort — the outcome is failed, NOT refused"
        );
        assert_eq!(report.exit_code(), 1, "partial apply maps to exit 1");
        assert_eq!(report.applied, 1, "op0 wrote X");
        // op0: applied (X mutated).
        assert_eq!(report.operations[0].status, norn_wire::OpStatus::Applied);
        // op1: failed carrying the structured code.
        assert_eq!(report.operations[1].status, norn_wire::OpStatus::Failed);
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
    /// vault stays unchanged and the outcome is `refused` (exit 2) — never
    /// `failed` — on the report-on-refusal surface.
    #[test]
    fn clean_prewrite_refusal_leaves_the_vault_untouched() {
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

        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.applied, 0);
        assert_eq!(report.operations[0].status, norn_wire::OpStatus::Failed);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.md")).unwrap(),
            original,
            "a clean refusal leaves the vault unchanged"
        );
    }

    /// NRN-231 review F1: a BARE-`anyhow` PRE-WRITE refusal (a create_document
    /// whose `new_value` has no frontmatter object) crosses as a coded,
    /// report-shaped refusal on the `refuse_as_report` surface (`internal-error`
    /// plus the `{e:#}` message), NOT a bare `Err`. This is the class that made a
    /// routed apply misreport post-send-uncertain; the vault stays unchanged.
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
                // new_value with a body but NO frontmatter object → the typed
                // `ApplyError::CreateFrontmatterMalformed` (code `malformed-plan`,
                // NRN-436) raised in Phase B BEFORE any write.
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

        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2, "a clean refusal maps to exit 2");
        assert_eq!(report.applied, 0);
        let op = &report.operations[0];
        assert_eq!(op.status, norn_wire::OpStatus::Failed);
        let err = op.error.as_ref().expect("failed op carries an error");
        assert_eq!(
            err.code, "malformed-plan",
            "a non-object frontmatter is a typed malformed-plan fault (NRN-436), not internal-error"
        );
        assert!(
            err.message
                .contains("create_document: missing or non-object frontmatter"),
            "the message carries the (unchanged) error prose: {}",
            err.message
        );
        // Unchanged: nothing was created.
        assert!(!tmp.path().join("new.md").exists());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), before);
    }

    /// NRN-231 review F1 (expansion phase): a PRE-WRITE expansion failure (an
    /// unknown op kind) likewise crosses as a coded, report-shaped refusal under
    /// `refuse_as_report`, before any `ApplyOp` exists. The CLI surface
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
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.operations.len(), 1);
        assert_eq!(report.operations[0].status, norn_wire::OpStatus::Failed);
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
        // gate must refuse before any write, leaving the file unchanged.
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
            "file must be unchanged after the refusal"
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
            "file must be unchanged after the refusal"
        );
    }

    // ------------------------------------------------------------------
    // NRN-264 re-review FIX #1: the three PRE-WRITE barriers between expansion
    // and the owner-set evaluation (vault-root containment, create-path
    // resolution, owner-precondition validation) must ALSO honor
    // `refuse_as_report`, crossing as coded, report-shaped refusals on the
    // routed/MCP surface — not escaping as bare transport errors — while the CLI
    // surface (`refuse_as_report: false`) keeps propagating the bare `Err`.
    // ------------------------------------------------------------------

    fn refuse_report_ctx(refuse_as_report: bool) -> ApplyContext {
        ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
            refuse_as_report,
            owner_index_options: Default::default(),
        }
    }

    /// (i) A `create_document` path with a `..` component is a pre-write
    /// containment refusal. Routed: `Ok(refused)` carrying `containment-parent-
    /// traversal`. CLI: bare `Err`.
    #[test]
    fn create_path_parent_traversal_refuses_as_report_under_refuse_as_report() {
        let (tmp, index) = synth_vault();
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
                fields: serde_json::json!({
                    "path": "../escape.md",
                    "new_value": { "frontmatter": { "type": "note" }, "body": "# X\n" }
                }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };

        let report = apply_migration_plan(&plan, &index, refuse_report_ctx(true), &mut test_sink())
            .expect("a pre-write create-path refusal must return a report, not Err");
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refused report carries a coded error");
        assert_eq!(err.code, "containment-parent-traversal");
        // Unchanged: nothing escaped the vault.
        assert!(!tmp.path().join("escape.md").exists());
        assert!(!tmp.path().parent().unwrap().join("escape.md").exists());

        // CLI surface: bare Err.
        let err = apply_migration_plan(&plan, &index, refuse_report_ctx(false), &mut test_sink())
            .expect_err("the CLI surface keeps propagating the bare Err");
        assert_eq!(
            crate::apply::envelope::from_anyhow(&err).code,
            "containment-parent-traversal"
        );
    }

    /// (ii) A `{{seq}}`-twice create template is a pre-write create-path
    /// resolution refusal (bare anyhow → `internal-error`). Routed: `Ok(refused)`;
    /// CLI: bare `Err`.
    #[test]
    fn create_path_double_seq_refuses_as_report_under_refuse_as_report() {
        let (tmp, index) = synth_vault();
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
                fields: serde_json::json!({
                    "path": "MMR-{{seq}}-{{seq}}.md",
                    "new_value": { "frontmatter": { "type": "note" }, "body": "# X\n" }
                }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };

        let report = apply_migration_plan(&plan, &index, refuse_report_ctx(true), &mut test_sink())
            .expect("a `{{seq}}`-twice refusal must return a report, not Err");
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refused report carries a coded error");
        assert_eq!(
            err.code, "malformed-plan",
            "a misplaced/doubled {{{{seq}}}} is the author's plan-structure fault (NRN-436)"
        );
        assert!(
            err.message.contains("only supported once"),
            "message text preserved from the old bare error: {}",
            err.message
        );

        // CLI surface: bare Err with the same prose.
        let err = apply_migration_plan(&plan, &index, refuse_report_ctx(false), &mut test_sink())
            .expect_err("the CLI surface keeps propagating the bare Err");
        assert!(err.to_string().contains("only supported once"));
    }

    /// (iii) A `stem_from_operation` selector referencing an op id no create
    /// operation carries is an owner-precondition VALIDATION refusal (typed
    /// `PreconditionError` → `invalid-precondition`, NRN-436). Routed:
    /// `Ok(refused)`; CLI: bare `Err`. (The owner-set MISMATCH path is already
    /// `Ok(refused)` on both surfaces.)
    #[test]
    fn owner_precondition_bad_op_ref_refuses_as_report_under_refuse_as_report() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root,
            generator: None,
            generated_at: None,
            preconditions: vec![norn_wire::PlanPrecondition::OwnerSet {
                id: "owner".into(),
                selector: norn_wire::OwnerSelector::StemFromOperation {
                    stem_from_operation: "does-not-exist".into(),
                },
                expected_paths: vec![],
            }],
            operations: vec![MigrationOp {
                kind: "create_document".into(),
                id: Some("create-real".into()),
                requires: vec![],
                fields: serde_json::json!({
                    "path": "made.md",
                    "new_value": { "frontmatter": { "type": "note" }, "body": "# X\n" }
                }),
                footnote: None,
            }],
            skipped: vec![],
            plan_footnote: None,
        };

        let report = apply_migration_plan(&plan, &index, refuse_report_ctx(true), &mut test_sink())
            .expect("an owner-precondition validation refusal must return a report, not Err");
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refused report carries a coded error");
        assert_eq!(err.code, "invalid-precondition");
        assert!(
            err.message.contains("missing or non-create operation"),
            "message carries the (unchanged) error prose: {}",
            err.message
        );
        // Unchanged: no doc was created.
        assert!(!tmp.path().join("made.md").exists());

        // CLI surface: bare Err.
        let err = apply_migration_plan(&plan, &index, refuse_report_ctx(false), &mut test_sink())
            .expect_err("the CLI surface keeps propagating the bare Err");
        assert!(err.to_string().contains("missing or non-create operation"));
    }

    /// (iv) A plan whose `vault_root` does not resolve to the index root is a
    /// pre-write `vault-root-mismatch` refusal. Routed: `Ok(refused)` KEEPING the
    /// `vault-root-mismatch` code even for a nonexistent plan root (a bare
    /// canonicalize failure must not launder it to `internal-error`). CLI: bare
    /// `Err`.
    #[test]
    fn vault_root_mismatch_refuses_as_report_under_refuse_as_report() {
        let (tmp, index) = synth_vault();
        // A plan root that does not exist on disk (so canonicalize() itself fails).
        let bogus_root = tmp.path().join("nonexistent-vault");
        let plan = MigrationPlan {
            schema_version: 2,
            vault_root: bogus_root.to_string_lossy().to_string(),
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

        let report = apply_migration_plan(&plan, &index, refuse_report_ctx(true), &mut test_sink())
            .expect("a vault-root-mismatch must return a report, not Err");
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refused report carries a coded error");
        assert_eq!(
            err.code, "vault-root-mismatch",
            "a nonexistent plan root keeps the vault-root-mismatch code"
        );
        // Unchanged: the real vault is untouched.
        assert!(tmp.path().join("a.md").exists());
        assert!(!tmp.path().join("renamed.md").exists());

        // CLI surface: bare Err carrying the same code.
        let err = apply_migration_plan(&plan, &index, refuse_report_ctx(false), &mut test_sink())
            .expect_err("the CLI surface keeps propagating the bare Err");
        assert_eq!(
            crate::apply::envelope::from_anyhow(&err).code,
            "vault-root-mismatch"
        );
    }

    /// NRN-264 re-review FIX #5: the expected-vs-actual owner path-set comparison
    /// folds ASCII case to stay consistent with the `eq_ignore_ascii_case` stem
    /// selection — an on-disk `Foo.md` with a `foo` stem selector and an
    /// author-supplied expected `foo.md` MATCHES (passes), rather than spuriously
    /// refusing over the case difference.
    #[test]
    fn owner_set_mixed_case_expected_path_refuses_fail_safe() {
        // The stem scan folds ASCII case (`eq_ignore_ascii_case`), but the owner
        // path-set comparison is exact by design (`owner_paths_mismatch`): a
        // mixed-case author-supplied expected path REFUSES rather than risk a
        // case-colliding owner silently passing on a case-sensitive filesystem.
        // Fail-safe — a spurious refuse is recoverable; a missed owner change is
        // not. A filesystem-case-aware policy is tracked as NRN-266.
        let tmp = tempfile::Builder::new()
            .prefix("applier-nrn264-case-")
            .tempdir()
            .unwrap();
        let root = tmp.path();
        std::fs::write(root.join("Foo.md"), "---\ntype: note\n---\n# Foo\n").unwrap();
        let index = crate::graph::build_index(Utf8Path::from_path(root).unwrap()).unwrap();

        let plan = MigrationPlan {
            schema_version: 2,
            vault_root: root.to_string_lossy().to_string(),
            generator: None,
            generated_at: None,
            preconditions: vec![norn_wire::PlanPrecondition::OwnerSet {
                id: "foo-owner".into(),
                selector: norn_wire::OwnerSelector::Stem { stem: "foo".into() },
                // Author-supplied lowercase path vs the on-disk `Foo.md`.
                expected_paths: vec!["foo.md".into()],
            }],
            operations: Vec::new(),
            skipped: vec![],
            plan_footnote: None,
        };

        let report = apply_migration_plan(&plan, &index, refuse_report_ctx(true), &mut test_sink())
            .expect("apply returns a report");
        assert_eq!(
            report.outcome,
            norn_wire::ApplyOutcome::Refused,
            "an exact-comparison case mismatch must refuse (fail-safe); report: {report:?}"
        );
        assert_eq!(
            report.preconditions[0].status,
            norn_wire::PreconditionStatus::Failed
        );
    }

    // ── NRN-406 (ADR 0024): partial apply + requires DAG ──────────────────────

    /// A create op whose `path` parent dir is missing and no `-p` — a create that
    /// fails at apply. `id`/`requires` are author-declared.
    fn create_op(path: &str, id: Option<&str>, requires: &[&str]) -> MigrationOp {
        MigrationOp {
            kind: "create_document".into(),
            id: id.map(str::to_string),
            requires: requires.iter().map(|s| s.to_string()).collect(),
            fields: serde_json::json!({
                "path": path,
                "new_value": { "frontmatter": { "type": "note" }, "body": "# x\n" },
            }),
            footnote: None,
        }
    }

    fn confirm_ctx() -> ApplyContext {
        ApplyContext {
            dry_run: false,
            parents: false,
            verbose: false,
            refuse_as_report: true,
            owner_index_options: Default::default(),
        }
    }

    fn plan_of(vault_root: &str, operations: Vec<MigrationOp>) -> MigrationPlan {
        MigrationPlan {
            schema_version: 2,
            vault_root: vault_root.to_string(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations,
            skipped: vec![],
            plan_footnote: None,
        }
    }

    /// Independent files proceed (the ONE deliberate behavior change, ADR 0024):
    /// op0 writes, op1 fails (missing parent), and the INDEPENDENT op2 still
    /// applies — where the old abort-at-first-failure left it not_run.
    #[test]
    fn independent_op_failure_does_not_abort_remaining_plan() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![
                create_op("first.md", None, &[]),
                create_op("missing/second.md", None, &[]), // parent missing → fails
                create_op("third.md", None, &[]),          // independent → proceeds
            ],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();

        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Failed);
        assert_eq!(report.exit_code(), 1, "a write landed → partial failure");
        assert_eq!(report.operations[0].status, OpStatus::Applied);
        assert_eq!(report.operations[1].status, OpStatus::Failed);
        assert_eq!(
            report.operations[1].error.as_ref().map(|e| e.code.as_str()),
            Some("create-parent-missing")
        );
        assert_eq!(
            report.operations[2].status,
            OpStatus::Applied,
            "the independent op proceeds past the failure"
        );
        assert_eq!(report.applied, 2);
        assert_eq!(report.remaining, 0);
        assert!(tmp.path().join("first.md").exists());
        assert!(tmp.path().join("third.md").exists());
    }

    /// A `requires` edge to a FAILED op propagates: the dependent records
    /// not_run (never runs), while the plan otherwise proceeds — cross-kind (a
    /// create requiring a delete).
    #[test]
    fn failed_requirement_propagates_not_run_cross_kind() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![
                create_op("landed.md", None, &[]), // writes → wrote_any = true
                MigrationOp {
                    kind: "delete_document".into(),
                    id: Some("del".into()),
                    requires: vec![],
                    fields: serde_json::json!({ "path": "ghost.md", "document_hash": "0000000000000000000000000000000000000000000000000000000000000000" }), // unknown-path → fails
                    footnote: None,
                },
                create_op("dependent.md", None, &["del"]), // requires the failed delete
            ],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();

        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Failed);
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.operations[0].status, OpStatus::Applied);
        assert_eq!(report.operations[1].status, OpStatus::Failed);
        assert_eq!(
            report.operations[2].status,
            OpStatus::NotRun,
            "the create requiring the failed delete must not run"
        );
        assert!(
            !tmp.path().join("dependent.md").exists(),
            "a not_run op writes nothing"
        );
    }

    /// A requirement on a LATER-pass op conservatively blocks the dependent
    /// (the required op is unrecorded at evaluation time, reading as not_run) —
    /// fail-safe: nothing writes against the author's declared dependency.
    /// Deliberate, pinned behavior: no planner emits `requires` today; if
    /// forward references should ever be honored, the DAG must gain
    /// cross-pass ordering, not silently weaker gating.
    #[test]
    fn forward_requirement_conservatively_blocks_the_dependent() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![
                // A delete (delete pass) requiring a create that only runs in
                // the LATER create pass: unsatisfiable forward reference.
                MigrationOp {
                    kind: "delete_document".into(),
                    id: Some("del".into()),
                    requires: vec!["mk".into()],
                    fields: serde_json::json!({ "path": "alpha.md", "document_hash": "0000000000000000000000000000000000000000000000000000000000000000" }),
                    footnote: None,
                },
                create_op("later.md", Some("mk"), &[]),
            ],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();

        assert_eq!(
            report.operations[0].status,
            OpStatus::NotRun,
            "a forward requirement reads as unsatisfied and blocks the delete"
        );
        assert_eq!(report.operations[1].status, OpStatus::Applied);
        assert!(
            tmp.path().join("a.md").exists(),
            "the blocked delete must not remove its target"
        );
    }

    /// A `requires` edge to an unknown op id refuses the WHOLE plan before any
    /// write: `malformed-plan`, exit 2, every op not_run except the offender.
    #[test]
    fn requires_unknown_op_refuses_whole_plan() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![create_op("a-new.md", Some("mk"), &["does-not-exist"])],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refusal carries a coded error");
        assert_eq!(err.code, "malformed-plan");
        assert!(err.message.contains("unknown operation id"));
        assert!(
            !tmp.path().join("a-new.md").exists(),
            "refusal writes nothing"
        );
    }

    /// A `requires` CYCLE refuses the whole plan: `malformed-plan`, exit 2.
    #[test]
    fn requires_cycle_refuses_whole_plan() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![
                create_op("cyc-a.md", Some("a"), &["b"]),
                create_op("cyc-b.md", Some("b"), &["a"]),
            ],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refusal carries a coded error");
        assert_eq!(err.code, "malformed-plan");
        assert!(err.message.contains("cycle"));
        assert!(!tmp.path().join("cyc-a.md").exists());
        assert!(!tmp.path().join("cyc-b.md").exists());
    }

    /// Refusal is still TOTAL: a plan-level validation failure (a duplicate op id)
    /// leaves every op not_run except the offender.
    #[test]
    fn plan_level_refusal_is_total() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![
                create_op("dup-a.md", Some("dup"), &[]),
                create_op("dup-b.md", Some("dup"), &[]),
            ],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.applied, 0);
        assert_eq!(
            report
                .operations
                .iter()
                .filter(|o| o.status == OpStatus::Failed)
                .count(),
            1,
            "exactly the offending op is failed"
        );
        assert!(!tmp.path().join("dup-a.md").exists());
        assert!(!tmp.path().join("dup-b.md").exists());
    }

    // ── delete-hash required + structural CAS (NRN-151, ADR 0024) ──────────────

    fn hash_of(index: &GraphIndex, path: &str) -> String {
        index
            .documents
            .iter()
            .find(|d| d.path == path)
            .unwrap_or_else(|| panic!("{path} not in index"))
            .hash
            .clone()
    }

    fn move_op(src: &str, dst: &str, document_hash: Option<&str>) -> MigrationOp {
        let mut fields = serde_json::Map::new();
        fields.insert("src".into(), serde_json::Value::String(src.into()));
        fields.insert("dst".into(), serde_json::Value::String(dst.into()));
        if let Some(h) = document_hash {
            fields.insert("document_hash".into(), serde_json::Value::String(h.into()));
        }
        MigrationOp {
            kind: "move_document".into(),
            id: None,
            requires: vec![],
            fields: serde_json::Value::Object(fields),
            footnote: None,
        }
    }

    /// A hand-authored `delete_document` op with NO `document_hash` is refused
    /// whole-plan before any write: `delete-hash-required`, exit 2, file untouched.
    #[test]
    fn delete_without_document_hash_refuses_delete_hash_required() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![MigrationOp {
                kind: "delete_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({ "path": "a.md" }),
                footnote: None,
            }],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        let err = report
            .operations
            .iter()
            .find_map(|o| o.error.as_ref())
            .expect("refusal carries a coded error");
        assert_eq!(err.code, "delete-hash-required");
        assert_eq!(err.path.as_deref(), Some("a.md"));
        assert!(tmp.path().join("a.md").exists(), "refusal writes nothing");
    }

    /// An explicit empty-string `document_hash` counts as missing (it would
    /// silently skip the CAS), so it refuses `delete-hash-required` too.
    #[test]
    fn delete_with_empty_document_hash_also_refuses() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![MigrationOp {
                kind: "delete_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({ "path": "a.md", "document_hash": "" }),
                footnote: None,
            }],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(
            report
                .operations
                .iter()
                .find_map(|o| o.error.as_ref())
                .map(|e| e.code.as_str()),
            Some("delete-hash-required")
        );
        assert!(tmp.path().join("a.md").exists());
    }

    /// A delete carrying the correct plan-time hash applies (b.md has no
    /// incoming links, so no cascade). Proves the barrier + CAS accept a match.
    #[test]
    fn delete_with_matching_hash_applies() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(
            &vault_root,
            vec![MigrationOp {
                kind: "delete_document".into(),
                id: None,
                requires: vec![],
                fields: serde_json::json!({
                    "path": "b.md",
                    "document_hash": hash_of(&index, "b.md"),
                }),
                footnote: None,
            }],
        );
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Applied);
        assert!(!tmp.path().join("b.md").exists(), "b.md was deleted");
    }

    /// Move CAS is OPTIONAL: a move op with NO `document_hash` applies (no check).
    #[test]
    fn move_without_hash_applies() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let plan = plan_of(&vault_root, vec![move_op("b.md", "moved.md", None)]);
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Applied);
        assert!(tmp.path().join("moved.md").exists());
        assert!(!tmp.path().join("b.md").exists());
    }

    /// Move CAS present-and-matching applies (pre-rename fingerprint check passes).
    #[test]
    fn move_with_matching_hash_applies() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let hash = hash_of(&index, "b.md");
        let plan = plan_of(&vault_root, vec![move_op("b.md", "moved.md", Some(&hash))]);
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Applied);
        assert!(tmp.path().join("moved.md").exists());
    }

    /// Move CAS present-but-stale refuses `stale-document-hash` before the rename —
    /// where the pre-0024 applier proceeded (move was unchecked).
    /// A pre-write single-op failure surfaces as a clean refusal (exit 2).
    #[test]
    fn move_with_stale_hash_refuses() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        let stale = "0000000000000000000000000000000000000000000000000000000000000000";
        let plan = plan_of(&vault_root, vec![move_op("b.md", "moved.md", Some(stale))]);
        let report = apply_migration_plan(&plan, &index, confirm_ctx(), &mut test_sink()).unwrap();
        assert_eq!(report.outcome, norn_wire::ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(
            report
                .operations
                .iter()
                .find_map(|o| o.error.as_ref())
                .map(|e| e.code.as_str()),
            Some("stale-document-hash")
        );
        assert!(tmp.path().join("b.md").exists(), "refusal writes nothing");
        assert!(!tmp.path().join("moved.md").exists());
    }

    /// A repair-emitted structural op's finding linkage decodes through the typed
    /// vocabulary and echoes onto the report op — read uniformly from the TYPED
    /// op, exactly like a change op (ADR 0024 item 5). A verb-synthesized move
    /// (no linkage) echoes `None`, so its report bytes are unchanged.
    #[test]
    fn structural_op_linkage_echoes_through_typed_path() {
        let (tmp, index) = synth_vault();
        let vault_root = tmp.path().to_string_lossy().to_string();
        // A move op carrying linkage (as repair emits it); no hash → CAS opt-out.
        let mut linked = move_op("b.md", "moved.md", None);
        let obj = linked.fields.as_object_mut().unwrap();
        obj.insert(
            "finding_code".into(),
            serde_json::Value::String("frontmatter-required-field-missing".into()),
        );
        obj.insert(
            "repair_rule".into(),
            serde_json::Value::String("set-default".into()),
        );
        let plan = plan_of(&vault_root, vec![linked]);
        let ctx = ApplyContext {
            dry_run: true,
            ..confirm_ctx()
        };
        let report = apply_migration_plan(&plan, &index, ctx, &mut test_sink()).unwrap();
        let op = &report.operations[0];
        assert_eq!(
            op.finding_code.as_deref(),
            Some("frontmatter-required-field-missing")
        );
        assert_eq!(op.repair_rule.as_deref(), Some("set-default"));

        // A verb-shaped move omits linkage → None (omitted, not fabricated).
        let plan2 = plan_of(&vault_root, vec![move_op("a.md", "moved-a.md", None)]);
        let ctx2 = ApplyContext {
            dry_run: true,
            ..confirm_ctx()
        };
        let report2 = apply_migration_plan(&plan2, &index, ctx2, &mut test_sink()).unwrap();
        assert_eq!(report2.operations[0].finding_code, None);
        assert_eq!(report2.operations[0].repair_rule, None);
    }
}
