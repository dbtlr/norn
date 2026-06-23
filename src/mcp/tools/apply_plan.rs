//! `vault.apply_plan` — apply a `MigrationPlan` inline (as JSON) to the vault.
//!
//! This tool is the MCP counterpart of `norn migrate`: it accepts the same
//! `MigrationPlan` structure that `vault.repair_plan` (Task 7) emits, and is
//! the natural second step in the repair-plan → apply-plan composition.
//!
//! The mutation-safety contract mirrors every other MCP mutation tool:
//!
//! - **Default DRY-RUN (`confirm: false` / absent).** Deserializes the plan,
//!   validates schema_version, runs `apply_migration_plan` with `dry_run = true`
//!   (which forecasts the apply without writing), and returns the `ApplyReport`
//!   with `dry_run = true`. Acquires NO lock, opens NO event sink.
//! - **`confirm: true` WRITES.** Same validation, then acquires the per-vault
//!   mutation lock, opens a real file-backed event sink (audited exactly like the
//!   CLI), and calls `apply_migration_plan` with `dry_run = false`.
//!
//! ## How it mirrors `norn migrate` (non-TTY / `--format json` path)
//!
//! `migrate_cmd::run`:
//! 1. Reads + parses the plan from a file.
//! 2. Validates `plan.schema_version == MIGRATION_PLAN_SCHEMA_VERSION` → exit 2
//!    on mismatch. (No hash check — migrate doesn't check the plan hash.)
//! 3. Acquires the per-vault mutation lock (apply only).
//! 4. Loads config + graph index.
//! 5. Calls `apply_migration_plan`, emitting `invocation_started` /
//!    `invocation_finished` lifecycle events.
//!
//! The MCP tool replicates steps 2–5 exactly: `confirm = false` maps to the
//! non-TTY, non-`--yes`, non-`--format json` implicit dry-run path, and
//! `confirm = true` maps to the `--yes` apply path.
//!
//! ## Plan source: inline JSON
//!
//! Unlike `norn migrate` (which reads from a file), the MCP tool accepts the
//! plan as an inline `serde_json::Value` field.  This is the natural shape for
//! an MCP caller that received the plan from `vault.repair_plan` — it can pass
//! `result.structuredContent.plan` directly into `vault.apply_plan` without
//! writing it to a temporary file.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mcp::context::VaultContext;
use crate::migration_plan::MIGRATION_PLAN_SCHEMA_VERSION;

/// Parameters for `vault.apply_plan`.
///
/// `plan` is the `MigrationPlan` as a JSON object — exactly the value returned
/// in `vault.repair_plan`'s `result.structuredContent.plan`. Pass it through
/// unchanged (or supply any hand-crafted valid `MigrationPlan`).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct ApplyPlanParams {
    /// The `MigrationPlan` JSON object to apply.  Must have
    /// `schema_version: 1`.  The plan emitted by `vault.repair_plan` satisfies
    /// this structure and can be fed here directly.
    pub plan: serde_json::Value,

    /// Apply the plan. **Defaults to `false` (dry-run): the call forecasts the
    /// apply (validation + expansion) and writes NOTHING.** Pass `true` to
    /// acquire the vault mutation lock and execute every operation in the plan.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.apply_plan`.
///
/// Wraps the [`crate::apply_report::ApplyReport`] as a generic
/// `serde_json::Value` inside this typed root struct (the same
/// `MoveOutput` / `DeleteOutput` pattern). The JSON is byte-for-byte identical
/// to what `norn migrate --format json` emits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ApplyPlanOutput {
    /// The `ApplyReport` JSON: `dry_run`, the applied/skipped/failed tallies,
    /// per-op records, and (on confirm) the trace id.  Byte-for-byte the shape
    /// `norn migrate --format json` emits.
    pub report: serde_json::Value,
}

impl ApplyPlanOutput {
    fn from_report(report: &crate::apply_report::ApplyReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.apply_plan`.
pub fn handle_output(ctx: &VaultContext, p: ApplyPlanParams) -> Result<ApplyPlanOutput> {
    let report = handle(ctx, p)?;
    ApplyPlanOutput::from_report(&report)
}

/// Pure handler for `vault.apply_plan`.
///
/// Returns the `ApplyReport` (same as `norn migrate --format json`).
///
/// **Validation (mirrors migrate exactly):** deserialize `p.plan` into a
/// `MigrationPlan`; if deserialization fails or `schema_version !=
/// MIGRATION_PLAN_SCHEMA_VERSION`, return an `Err` — the plan is rejected
/// before any lock is acquired or any file is touched.
///
/// **DRY-RUN (`!confirm`):** no lock, discard sink, applier in `dry_run = true`
/// mode — forecasts the apply (expansion + preflight) without writing.
///
/// **CONFIRM (`confirm`):** acquire mutation lock, open real event sink, apply
/// with `dry_run = false` — the same path `norn migrate --yes` takes.
pub fn handle(ctx: &VaultContext, p: ApplyPlanParams) -> Result<crate::apply_report::ApplyReport> {
    use crate::applier::{apply_migration_plan, ApplyContext};
    use crate::migration_plan::MigrationPlan;

    let cwd = ctx.vault_root.clone();

    // ── Step 1: deserialize the inline plan value → MigrationPlan ──────────────
    // A `serde_json::Error` here means the caller sent a structurally invalid
    // plan (missing required fields, wrong types, etc.).  Mirror migrate's parse
    // failure: return an Err (caller sees an MCP error), apply nothing.
    let plan: MigrationPlan = serde_json::from_value(p.plan.clone())
        .map_err(|e| anyhow::anyhow!("failed to parse MigrationPlan: {e}"))?;

    // ── Step 2: validate schema_version — mirror migrate's exit-2 check ────────
    // `norn migrate` rejects any plan whose schema_version != MIGRATION_PLAN_SCHEMA_VERSION.
    // The MCP tool does the same: return Err, apply nothing.
    if plan.schema_version != MIGRATION_PLAN_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported plan schema_version {}; this norn build supports v{}",
            plan.schema_version,
            MIGRATION_PLAN_SCHEMA_VERSION
        );
    }

    // ── Step 3: load graph index (same entry point migrate uses) ────────────────
    let index = crate::cache_cmd::load_graph_index(&cwd, &ctx.config.index_options, false)?;

    let dry_run = !p.confirm;

    let apply_ctx = ApplyContext {
        dry_run,
        parents: false,
        verbose: false,
    };

    // ── DRY-RUN (default): no lock, discard sink, applier in dry-run mode ───────
    if dry_run {
        let mut sink = crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::new(),
            crate::telemetry::Clock::System,
        );
        let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(report);
    }

    // ── CONFIRM: acquire mutation lock, open real sink, apply ──────────────────
    let _mutation_lock = crate::mcp::mutate::acquire_mutation_lock(&cwd)?;

    // Open a real, file-backed event sink — the same audit trail `norn migrate`
    // writes. `apply_migration_plan` emits the per-op spans and action events
    // itself; we frame them with `invocation_started` / `invocation_finished`.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "migrate",
        &cwd,
        &plan.vault_root,
        /*dry_run=*/ false,
        &["apply_plan".to_string()],
    );

    let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let exit = if report.failed > 0 { 1 } else { 0 };
    crate::emit_invocation_finished(&mut sink, "migrate", exit, &report);

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Vault with a fixable broken wikilink:
    /// - `target-note.md` exists (stem: `target-note`)
    /// - `source.md` links to `[[target-not]]` (one-char edit → closest-match proposal)
    ///
    /// Mirrors the `repair_plan` test vault exactly so the two tools compose.
    fn vault_with_fixable_link() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-apply-plan-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        std::fs::write(
            root.join("target-note.md"),
            "---\ntype: note\ntitle: Target Note\n---\n\nI am the target.\n",
        )
        .unwrap();
        std::fs::write(
            root.join("source.md"),
            "---\ntype: note\ntitle: Source\n---\n\nSee [[target-not]] for details.\n",
        )
        .unwrap();

        (tmp, root)
    }

    /// Compose repair_plan → apply_plan (dry-run): get a real plan from the
    /// repair_plan handler, pass it to apply_plan with confirm:false, verify
    /// the report is dry_run=true and disk is unchanged.
    #[test]
    fn compose_repair_plan_dry_run_applies_nothing() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Produce a real MigrationPlan via the repair_plan handler.
        let repair_out =
            crate::mcp::tools::repair_plan::handle(&ctx, Default::default()).expect("repair_plan");
        let plan_value = repair_out.plan;

        // The plan must have ≥1 operation (sanity check — mirrors repair_plan's tests).
        assert!(
            plan_value["operations"]
                .as_array()
                .is_some_and(|ops| !ops.is_empty()),
            "expected ≥1 operation in the repair plan: {plan_value:?}"
        );

        let source_before =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md");

        // Apply with confirm:false → dry-run.
        let report = handle(
            &ctx,
            ApplyPlanParams {
                plan: plan_value,
                confirm: false,
            },
        )
        .expect("apply_plan (dry-run) should succeed");

        assert!(
            report.dry_run,
            "dry-run report must have dry_run == true: {report:?}"
        );
        assert_eq!(
            report.applied, 0,
            "dry-run must report 0 applied: {report:?}"
        );

        // CRITICAL: disk unchanged — the broken link is still there.
        let source_after =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md after");
        assert_eq!(
            source_before, source_after,
            "dry-run must leave source.md byte-identical"
        );
        assert!(
            source_after.contains("[[target-not]]"),
            "dry-run must not rewrite the broken link in source.md:\n{source_after}"
        );
    }

    /// Compose repair_plan → apply_plan (confirm:true): the rewrite is applied
    /// on disk.
    #[test]
    fn compose_repair_plan_confirm_applies_fix() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Produce the plan.
        let repair_out =
            crate::mcp::tools::repair_plan::handle(&ctx, Default::default()).expect("repair_plan");
        let plan_value = repair_out.plan;

        // Apply with confirm:true → the rewrite is executed.
        let report = handle(
            &ctx,
            ApplyPlanParams {
                plan: plan_value,
                confirm: true,
            },
        )
        .expect("apply_plan (confirm) should succeed");

        assert!(
            !report.dry_run,
            "confirm report must have dry_run == false: {report:?}"
        );
        assert!(
            report.applied >= 1,
            "confirm must report >= 1 applied: {report:?}"
        );

        // The broken link must be fixed on disk.
        let source_after =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md after");
        assert!(
            source_after.contains("[[target-note]]"),
            "confirm must rewrite [[target-not]] → [[target-note]] in source.md:\n{source_after}"
        );
        assert!(
            !source_after.contains("[[target-not]]"),
            "confirm must not leave the old broken link:\n{source_after}"
        );
    }

    /// A malformed plan (garbage JSON) must return Err, applying nothing.
    #[test]
    fn malformed_plan_is_rejected() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let garbage = serde_json::json!({
            "not_a_plan": true,
            "operations": "wrong type"
        });

        let result = handle(
            &ctx,
            ApplyPlanParams {
                plan: garbage,
                confirm: false,
            },
        );
        assert!(
            result.is_err(),
            "malformed plan must return Err, but got Ok"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("MigrationPlan"),
            "error message must mention MigrationPlan, got: {err}"
        );
    }

    /// A plan with wrong schema_version must be rejected (mirrors migrate's exit-2).
    #[test]
    fn wrong_schema_version_is_rejected() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // A structurally valid plan but wrong schema_version.
        let bad_version = serde_json::json!({
            "schema_version": 99,
            "vault_root": root.to_string(),
            "operations": [],
        });

        let result = handle(
            &ctx,
            ApplyPlanParams {
                plan: bad_version,
                confirm: false,
            },
        );
        assert!(
            result.is_err(),
            "wrong schema_version must return Err, but got Ok"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("schema_version"),
            "error message must mention schema_version, got: {err}"
        );
        assert!(
            err.to_string().contains("99"),
            "error message must mention the rejected version 99, got: {err}"
        );
    }

    /// A null / missing plan field (empty JSON object) must be rejected because
    /// `schema_version` and `operations` are required by `MigrationPlan`.
    #[test]
    fn empty_json_plan_is_rejected() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle(
            &ctx,
            ApplyPlanParams {
                plan: serde_json::json!({}),
                confirm: false,
            },
        );
        assert!(
            result.is_err(),
            "empty JSON plan must return Err (missing required fields)"
        );
    }
}
