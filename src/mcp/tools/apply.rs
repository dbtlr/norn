//! `vault.apply` — apply a `MigrationPlan` inline (as JSON) to the vault.
//!
//! This tool is the MCP counterpart of `norn apply`: it accepts the same
//! `MigrationPlan` structure that `vault.repair` (Task 7) emits, and is
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
//! ## How it mirrors `norn apply` (non-TTY / `--format json` path)
//!
//! `apply_cmd::run`:
//! 1. Reads + parses the plan from a file.
//! 2. Validates `plan.schema_version == MIGRATION_PLAN_SCHEMA_VERSION` → exit 2
//!    on mismatch. (No hash check — apply doesn't check the plan hash.)
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
//! Unlike `norn apply` (which reads from a file), the MCP tool accepts the
//! plan as an inline `serde_json::Value` field.  This is the natural shape for
//! an MCP caller that received the plan from `vault.repair` — it can pass
//! `result.structuredContent.plan` directly into `vault.apply` without
//! writing it to a temporary file.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mcp::context::VaultContext;
use crate::mcp::mutation_result::MutationResult;
use crate::migration_plan::MIGRATION_PLAN_SCHEMA_VERSION;

/// Parameters for `vault.apply`.
///
/// `plan` is the `MigrationPlan` as a JSON object — exactly the value returned
/// in `vault.repair`'s `result.structuredContent.plan`. Pass it through
/// unchanged (or supply any hand-crafted valid `MigrationPlan`).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct ApplyParams {
    /// The `MigrationPlan` JSON object to apply.  Must have
    /// `schema_version: 1`.  The plan emitted by `vault.repair` satisfies
    /// this structure and can be fed here directly.
    pub plan: serde_json::Value,

    /// Apply the plan. **Defaults to `false` (dry-run): the call forecasts the
    /// apply (validation + expansion) and writes NOTHING.** Pass `true` to
    /// acquire the vault mutation lock and execute every operation in the plan.
    #[serde(default)]
    pub confirm: bool,

    /// Auto-create missing parent directories for `create_document` ops
    /// (`mkdir -p` style). Defaults to `false` (the prior behavior: an op whose
    /// destination directory does not exist refuses). Directories are created
    /// inside the apply and only for ops that proceed — the same discipline
    /// `vault.move`/`vault.new` follow. Mirrors `norn apply --parents`.
    #[serde(default)]
    pub parents: bool,
}

/// Structured output for `vault.apply`.
///
/// Wraps the [`crate::apply_report::ApplyReport`] as a generic
/// `serde_json::Value` inside this typed root struct (the same
/// `MoveOutput` / `DeleteOutput` pattern). The JSON is byte-for-byte identical
/// to what `norn apply --format json` emits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ApplyOutput {
    /// The `ApplyReport` JSON: `dry_run`, the applied/skipped/failed tallies,
    /// per-op records, and (on confirm) the trace id.  Byte-for-byte the shape
    /// `norn apply --format json` emits.
    pub report: serde_json::Value,
}

impl ApplyOutput {
    fn from_report(report: &crate::apply_report::ApplyReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.apply`.
pub fn handle_output(ctx: &VaultContext, p: ApplyParams) -> Result<MutationResult<ApplyOutput>> {
    let dry_run = !p.confirm;
    let vault_root = ctx.vault_root.to_string();
    // NRN-229: a mutation-lock timeout (the one refusal `apply` raises before the
    // applier's own `refuse_as_report` path takes over) becomes a coded,
    // structured `mutation-lock-timeout` refusal + `isError` instead of a bare
    // protocol error — the same idiom every mutation tool shares. A malformed /
    // wrong-schema plan is NOT a coded refusal and still propagates as a bare
    // `Err` (unrecognized → `None`).
    let report = match handle(ctx, p) {
        Ok(report) => report,
        Err(e) => match crate::mcp::mutate::refusal_from_error(&e) {
            Some(err) => {
                crate::apply_report::ApplyReport::refused(vault_root, dry_run, "apply", err)
            }
            None => return Err(e),
        },
    };
    // BUG-3 / NRN-219: `isError` is derived from the report's outcome by
    // `from_apply_report` (a not-applied confirm → true; a dry-run forecast →
    // false), while the structured report is preserved so a consumer branches on
    // `operations[].error.code`.
    Ok(MutationResult::from_apply_report(
        ApplyOutput::from_report(&report)?,
        &report,
    ))
}

/// Pure handler for `vault.apply`.
///
/// Returns the `ApplyReport` (same as `norn apply --format json`).
///
/// **Validation (mirrors apply exactly):** deserialize `p.plan` into a
/// `MigrationPlan`; if deserialization fails or `schema_version !=
/// MIGRATION_PLAN_SCHEMA_VERSION`, return an `Err` — the plan is rejected
/// before any lock is acquired or any file is touched.
///
/// **DRY-RUN (`!confirm`):** no lock, discard sink, applier in `dry_run = true`
/// mode — forecasts the apply (expansion + preflight) without writing.
///
/// **CONFIRM (`confirm`):** acquire the mutation lock BEFORE loading the graph
/// index, open a real event sink, apply with `dry_run = false` — the same
/// path `norn apply --yes` takes.
pub fn handle(ctx: &VaultContext, p: ApplyParams) -> Result<crate::apply_report::ApplyReport> {
    use crate::applier::{apply_migration_plan, ApplyContext};
    use crate::migration_plan::MigrationPlan;

    let cwd = ctx.vault_root.clone();

    // ── Step 1: deserialize the inline plan value → MigrationPlan ──────────────
    // A `serde_json::Error` here means the caller sent a structurally invalid
    // plan (missing required fields, wrong types, etc.).  Mirror apply's parse
    // failure: return an Err (caller sees an MCP error), apply nothing.
    let plan: MigrationPlan = serde_json::from_value(p.plan.clone())
        .map_err(|e| anyhow::anyhow!("failed to parse MigrationPlan: {e}"))?;

    // ── Step 2: validate schema_version — mirror apply's exit-2 check ────────
    // `norn apply` rejects any plan whose schema_version != MIGRATION_PLAN_SCHEMA_VERSION.
    // The MCP tool does the same: return Err, apply nothing.
    if plan.schema_version != MIGRATION_PLAN_SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported plan schema_version {}; this norn build supports v{}",
            plan.schema_version,
            MIGRATION_PLAN_SCHEMA_VERSION
        );
    }

    let dry_run = !p.confirm;

    // CONFIRM locks BEFORE any read that feeds the write (plan parsing above
    // stays pre-lock — it reads no vault state); dry-run never locks. See
    // `crate::mcp::mutate::acquire_mutation_lock` for the invariant.
    let _mutation_lock = if p.confirm {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    } else {
        None
    };

    // ── Step 3: load graph index (same entry point apply uses) ────────────────
    // Warm-connection reuse under the daemon; fresh open in cold mode (NRN-130).
    let index = ctx.load_graph_index()?;

    let apply_ctx = ApplyContext {
        dry_run,
        parents: p.parents,
        verbose: false,
        refuse_as_report: true,
    };

    // ── DRY-RUN (default): no lock, discard sink, applier in dry-run mode ───────
    if dry_run {
        let mut sink = crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::new(),
            crate::telemetry::Clock::System,
        );
        // Propagate the original error (NOT rewrapped) so `to_mcp_error` can
        // downcast it to the rich `ApplyError` and attach the `{ code, message,
        // path? }` structured envelope (NRN-150). A precondition refusal instead
        // returns `Ok(report)` (report-on-refusal) via `refuse_as_report`.
        let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)?;
        return Ok(report);
    }

    // ── CONFIRM: the mutation lock was already acquired above, before the
    // index load — open the real sink and apply.

    // Open a real, file-backed event sink — the same audit trail `norn apply`
    // writes. `apply_migration_plan` emits the per-op spans and action events
    // itself; we frame them with `invocation_started` / `invocation_finished`.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "apply",
        &cwd,
        &plan.vault_root,
        /*dry_run=*/ false,
        &["vault.apply".to_string()],
    );

    // Propagate the original error (see the dry-run branch) so the structured
    // envelope survives to `to_mcp_error`.
    let mut touched: Vec<camino::Utf8PathBuf> = Vec::new();
    let report = crate::applier::apply_migration_plan_collecting_touched(
        &plan,
        &index,
        apply_ctx,
        &mut sink,
        Some(&mut touched),
    )?;

    crate::emit_invocation_finished(&mut sink, "apply", report.exit_code(), &report);

    // Warm mode: commit the plan's cache increments (every touched path) as a
    // chunked writer-queue op, awaited; no-op in cold mode (NRN-252 / NRN-158).
    crate::mcp::mutate::commit_apply_increments(ctx, &touched);

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
    /// Mirrors the `repair` test vault exactly so the two tools compose.
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

    /// Compose repair → apply (dry-run): get a real plan from the
    /// repair handler, pass it to apply with confirm:false, verify
    /// the report is dry_run=true and disk is unchanged.
    #[test]
    fn compose_repair_dry_run_applies_nothing() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Produce a real MigrationPlan via the repair handler.
        let repair_out =
            crate::mcp::tools::repair::handle(&ctx, Default::default()).expect("repair");
        let plan_value = repair_out.plan;

        // The plan must have ≥1 operation (sanity check — mirrors repair's tests).
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
            ApplyParams {
                plan: plan_value,
                confirm: false,
                parents: false,
            },
        )
        .expect("apply (dry-run) should succeed");

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

    /// Compose repair → apply (confirm:true): the rewrite is applied
    /// on disk.
    #[test]
    fn compose_repair_confirm_applies_fix() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // Produce the plan.
        let repair_out =
            crate::mcp::tools::repair::handle(&ctx, Default::default()).expect("repair");
        let plan_value = repair_out.plan;

        // Apply with confirm:true → the rewrite is executed.
        let report = handle(
            &ctx,
            ApplyParams {
                plan: plan_value,
                confirm: true,
                parents: false,
            },
        )
        .expect("apply (confirm) should succeed");

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

    /// NRN-98 parity: an `append_to_section` edit op applies via `vault.apply`
    /// exactly as it does via `norn apply` (same applier path).
    #[test]
    fn edit_op_applies_via_apply() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-edit-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        let note = root.join("note.md");
        std::fs::write(&note, "---\nt: 1\n---\n## Notes\nline one\n").unwrap();
        let hash = blake3::hash(&std::fs::read(&note).unwrap())
            .to_hex()
            .to_string();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [{
                "kind": "append_to_section",
                "fields": { "path": "note.md", "document_hash": hash, "heading": "Notes", "content": "line two" }
            }]
        });

        let report = handle(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("apply (confirm) should succeed");
        assert!(!report.dry_run);
        assert!(report.applied >= 1, "expected >= 1 applied: {report:?}");

        let after = std::fs::read_to_string(&note).unwrap();
        assert!(
            after.contains("line one") && after.contains("line two"),
            "edit must apply via MCP; got:\n{after}"
        );
    }

    /// NRN-100 (H2): a `create_document` + `replace_body` composed into ONE plan
    /// applies as a single batch via `vault.apply` — the MCP counterpart of
    /// the `apply` CLI composition test. Confirms the composed write path is
    /// reachable off-filesystem (MCP-only client parity).
    #[test]
    fn composed_create_and_replace_body_applies_via_apply() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-compose-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        let existing = root.join("a.md");
        std::fs::write(&existing, "---\ntype: note\n---\n# A\n").unwrap();
        let a_hash = blake3::hash(&std::fs::read(&existing).unwrap())
            .to_hex()
            .to_string();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [
                {
                    "kind": "create_document",
                    "fields": {
                        "path": "c.md",
                        "new_value": { "frontmatter": {"type": "note"}, "body": "# C\n" }
                    }
                },
                {
                    "kind": "replace_body",
                    "fields": {
                        "path": "a.md",
                        "document_hash": a_hash,
                        "new_value": "# A (rewritten)\n"
                    }
                }
            ]
        });

        let report = handle(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("apply (confirm) should succeed");
        assert!(!report.dry_run);
        assert!(report.applied >= 2, "expected >= 2 applied: {report:?}");

        let c = std::fs::read_to_string(root.join("c.md")).unwrap();
        assert!(
            c.contains("type: note") && c.contains("# C"),
            "create landed via MCP; got:\n{c}"
        );
        let a = std::fs::read_to_string(&existing).unwrap();
        assert!(
            a.contains("type: note") && a.contains("# A (rewritten)"),
            "replace_body landed via MCP with frontmatter preserved; got:\n{a}"
        );
    }

    /// NRN-150 / MMR-202: a VALIDATION-PHASE precondition refusal (here a
    /// `stale-document-hash` CAS drift) is returned to an MCP caller as the
    /// ApplyReport — the offending op `failed` carrying `error.code`, the rest
    /// `not_run`, `outcome = refused` — NOT a bare `internal_error`. A consumer
    /// branches on the CODE (retryable CAS drift vs terminal refusal) without
    /// string-matching prose, and the vault is byte-identical.
    #[test]
    fn precondition_refusal_returns_report_with_failed_op_code() {
        use crate::apply_report::{ApplyOutcome, OpStatus};

        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-refusal-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        let doc = "---\ntype: note\n---\n# A\n";
        std::fs::write(root.join("a.md"), doc).unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        // Non-empty WRONG document_hash: hydration only fills empty hashes, so
        // this survives to the CAS check and refuses with stale-document-hash.
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [{
                "kind": "add_frontmatter",
                "fields": {
                    "path": "a.md",
                    "field": "status",
                    "new_value": "done",
                    "document_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            }]
        });

        // confirm:true (the apply path) — the refusal must still return a report,
        // never a bare Err, so an MCP client sees structuredContent not isError.
        let report = handle(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("a precondition refusal must return a report, not Err");

        assert_eq!(report.outcome, ApplyOutcome::Refused);
        assert_eq!(report.failed, 1, "exactly the offending op failed");
        assert_eq!(report.applied, 0);
        assert_eq!(report.exit_code(), 2, "refusal maps to exit 2");
        let op = &report.operations[0];
        assert_eq!(op.status, OpStatus::Failed);
        let err = op
            .error
            .as_ref()
            .expect("failed op carries an error envelope");
        assert_eq!(
            err.code, "stale-document-hash",
            "consumer branches on the stable kebab code"
        );
        assert_eq!(err.path.as_deref(), Some("a.md"));

        // Byte-identical: a validation-phase refusal wrote nothing.
        assert_eq!(std::fs::read_to_string(root.join("a.md")).unwrap(), doc);
    }

    /// NRN-150/183 byte-identity-lie fix, MCP surface. A 2-op plan where op0
    /// (`set_frontmatter`) WRITES on disk in Phase A2, then op1
    /// (`delete_document` of an untracked path) fails a Phase-B precondition
    /// AFTER that write. The prior contract returned `outcome: refused` /
    /// `applied: 0` (documented "vault byte-identical, nothing written") while op0
    /// had already mutated the vault — the lie. It now returns `outcome: failed`
    /// (exit 1): op0 `applied`, op1 `failed` + `error.code`, and the mutated doc
    /// is visibly changed on disk, so a consumer knows to re-read.
    #[test]
    fn partial_apply_after_write_returns_failed_report_not_refused() {
        use crate::apply_report::{ApplyOutcome, OpStatus};

        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-partial-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [
                { "kind": "set_frontmatter", "fields": {
                    "path": "a.md", "field": "type",
                    "expected_old_value": "note", "new_value": "task" } },
                { "kind": "delete_document", "fields": { "path": "ghost.md" } }
            ]
        });

        let report = handle(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("a partial apply must return a report, not Err");

        assert_eq!(
            report.outcome,
            ApplyOutcome::Failed,
            "op0 wrote a.md before op1 failed — outcome is failed, NOT refused: {report:?}"
        );
        assert_eq!(report.exit_code(), 1, "partial apply maps to exit 1");
        assert_eq!(report.applied, 1);
        assert_eq!(report.operations[0].status, OpStatus::Applied);
        assert_eq!(report.operations[1].status, OpStatus::Failed);
        assert_eq!(
            report.operations[1].error.as_ref().map(|e| e.code.as_str()),
            Some("unknown-path")
        );
        // Ground truth: the vault WAS mutated — the report cannot claim otherwise.
        let a = std::fs::read_to_string(root.join("a.md")).unwrap();
        assert!(a.contains("type: task"), "op0 mutated a.md; got:\n{a}");
    }

    /// BUG-3 / NRN-219: the MCP result's `isError` bit MUST agree with the
    /// report's outcome, while the structured report is preserved on BOTH paths.
    /// A validation-phase refusal (byte-identical, nothing written) is `isError:
    /// true` — a consumer trusting the protocol-native bit no longer reads a
    /// no-write as success — yet `structuredContent.report` still carries
    /// `outcome: refused` and the op's `error.code` for retry classification.
    #[test]
    fn refusal_maps_to_iserror_true_preserving_report() {
        use rmcp::handler::server::tool::IntoCallToolResult;

        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-refusal-iserror-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [{
                "kind": "add_frontmatter",
                "fields": {
                    "path": "a.md", "field": "status", "new_value": "done",
                    "document_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            }]
        });

        let mr = handle_output(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("a refusal returns a MutationResult, not Err");
        assert!(mr.is_error(), "a refused apply must map to isError:true");

        let result = mr.into_call_tool_result().expect("render CallToolResult");
        assert_eq!(
            result.is_error,
            Some(true),
            "protocol-native isError reflects the no-write"
        );
        let sc = result
            .structured_content
            .expect("structured report must survive on the error path");
        assert_eq!(
            sc["report"]["outcome"], "refused",
            "consumer still reads the outcome"
        );
        assert_eq!(
            sc["report"]["operations"][0]["error"]["code"], "stale-document-hash",
            "the machine-readable code is NOT laundered back to prose"
        );
    }

    /// The complement: a clean apply is `isError: false` (unchanged from before),
    /// so success is not spuriously flagged. Guards against over-erroring.
    #[test]
    fn clean_apply_maps_to_iserror_false() {
        use rmcp::handler::server::tool::IntoCallToolResult;

        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan_value = crate::mcp::tools::repair::handle(&ctx, Default::default())
            .expect("repair")
            .plan;

        let mr = handle_output(
            &ctx,
            ApplyParams {
                plan: plan_value,
                confirm: true,
                parents: false,
            },
        )
        .expect("a clean apply returns a MutationResult");
        assert!(!mr.is_error(), "a clean apply must map to isError:false");

        let result = mr.into_call_tool_result().expect("render CallToolResult");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result.structured_content.expect("structured content")["report"]["outcome"],
            "applied"
        );
    }

    /// A partial-apply failure (a write landed, then an op failed) is the sharp
    /// case: NOT byte-identical, yet must still surface `isError: true` so a naive
    /// consumer does not double-apply the ops that already succeeded.
    #[test]
    fn partial_failure_maps_to_iserror_true() {
        use rmcp::handler::server::tool::IntoCallToolResult;

        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-partial-iserror-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [
                { "kind": "set_frontmatter", "fields": {
                    "path": "a.md", "field": "type",
                    "expected_old_value": "note", "new_value": "task" } },
                { "kind": "delete_document", "fields": { "path": "ghost.md" } }
            ]
        });

        let mr = handle_output(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("a partial apply returns a MutationResult, not Err");
        assert!(
            mr.is_error(),
            "a partial-apply failure must map to isError:true (not byte-identical)"
        );

        let result = mr.into_call_tool_result().expect("render CallToolResult");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.expect("structured content")["report"]["outcome"],
            "failed"
        );
    }

    /// NRN-219 dry-run carve-out: a `confirm:false` PREVIEW that forecasts a
    /// refusal is `isError:false`, not true — a dry-run attempts no write, so it
    /// cannot misreport one, and an SDK that raises on `isError` must not throw on
    /// a preview. The forecasted refusal is carried in the (preserved) structured
    /// report as `outcome:refused` / `dry_run:true` instead. This is the seam the
    /// code review flagged; without the `!report.dry_run` guard this returns true.
    #[test]
    fn dryrun_forecast_of_refusal_maps_to_iserror_false() {
        use rmcp::handler::server::tool::IntoCallToolResult;

        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-dryrun-refusal-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [{
                "kind": "add_frontmatter",
                "fields": {
                    "path": "a.md", "field": "status", "new_value": "done",
                    "document_hash": "0000000000000000000000000000000000000000000000000000000000000000"
                }
            }]
        });

        let mr = handle_output(
            &ctx,
            ApplyParams {
                plan,
                confirm: false, // PREVIEW — no write attempted
                parents: false,
            },
        )
        .expect("a dry-run forecast returns a MutationResult");
        assert!(
            !mr.is_error(),
            "a dry-run forecast (even of a refusal) must be isError:false — it wrote nothing"
        );

        let result = mr.into_call_tool_result().expect("render CallToolResult");
        assert_eq!(result.is_error, Some(false));
        // The forecast is still fully visible in the structured report.
        let report = &result.structured_content.expect("structured content")["report"];
        assert_eq!(
            report["outcome"], "refused",
            "the forecast still says refused"
        );
        assert_eq!(report["dry_run"], true, "and flags it as a dry-run");
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
            ApplyParams {
                plan: garbage,
                confirm: false,
                parents: false,
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

    /// A plan with wrong schema_version must be rejected (mirrors apply's exit-2).
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
            ApplyParams {
                plan: bad_version,
                confirm: false,
                parents: false,
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
            ApplyParams {
                plan: serde_json::json!({}),
                confirm: false,
                parents: false,
            },
        );
        assert!(
            result.is_err(),
            "empty JSON plan must return Err (missing required fields)"
        );
    }

    /// NRN-174: a create_document into a missing subdirectory refuses when
    /// `parents` is omitted (the prior behavior) and succeeds — creating the
    /// intermediate dirs — when `parents: true`. Directory creation happens
    /// inside apply, only for the op that proceeds.
    #[test]
    fn parents_true_creates_missing_dirs_for_create_op() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-parents-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = || {
            serde_json::json!({
                "schema_version": 1,
                "vault_root": root.to_string(),
                "operations": [{
                    "kind": "create_document",
                    "fields": {
                        "path": "sub/dir/new.md",
                        "new_value": { "frontmatter": {"type": "note"}, "body": "# New\n" }
                    }
                }]
            })
        };

        // parents omitted → the missing parent dir is a PRE-WRITE refusal. NRN-231
        // review F1: this bare-`anyhow` refusal now crosses as a coded,
        // report-shaped refusal (`outcome: refused`, exit 2) rather than a bare
        // `Err`, so a routed apply reconstructs the exit-2 refusal instead of a
        // false post-send-uncertain. `wrote_any == false` proves the vault is
        // byte-identical, so nothing is written.
        use crate::apply_report::{ApplyOutcome, OpStatus};
        let report = handle(
            &ctx,
            ApplyParams {
                plan: plan(),
                confirm: true,
                parents: false,
            },
        )
        .expect("a pre-write refusal returns a report, not Err");
        assert_eq!(
            report.outcome,
            ApplyOutcome::Refused,
            "create into a missing dir must refuse without parents: {report:?}"
        );
        assert_eq!(report.exit_code(), 2, "a clean refusal maps to exit 2");
        assert_eq!(report.applied, 0);
        assert_eq!(report.operations[0].status, OpStatus::Failed);
        assert!(
            report.operations[0]
                .error
                .as_ref()
                .is_some_and(|e| e.message.contains("parent directory does not exist")),
            "the failing op carries the coded pre-write error: {report:?}"
        );
        assert!(
            !root.join("sub/dir/new.md").as_std_path().exists(),
            "refusal must leave nothing on disk"
        );

        // parents: true → intermediate dirs created, the doc written.
        let report = handle(
            &ctx,
            ApplyParams {
                plan: plan(),
                confirm: true,
                parents: true,
            },
        )
        .expect("apply with parents:true should succeed");
        assert!(!report.dry_run);
        assert!(report.applied >= 1, "expected >= 1 applied: {report:?}");
        let created = std::fs::read_to_string(root.join("sub/dir/new.md"))
            .expect("create_document wrote the doc into the auto-created dir");
        assert!(created.contains("# New"), "created body present: {created}");
    }

    /// NRN-175: a create_document with a `{{seq}}` template surfaces the
    /// apply-time-resolved path (and its stem) as structured fields on the op —
    /// the value a consumer would otherwise regex out of `summary`. Serialization
    /// carries both.
    #[test]
    fn create_with_seq_carries_resolved_path_and_stem_on_op() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-seq-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::create_dir_all(root.join(".norn")).unwrap();
        std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let plan = serde_json::json!({
            "schema_version": 1,
            "vault_root": root.to_string(),
            "operations": [{
                "kind": "create_document",
                "fields": {
                    "path": "task-{{seq}}.md",
                    "new_value": { "frontmatter": {"type": "note"}, "body": "# T\n" }
                }
            }]
        });

        let report = handle(
            &ctx,
            ApplyParams {
                plan,
                confirm: true,
                parents: false,
            },
        )
        .expect("apply should succeed");
        assert!(report.applied >= 1, "expected >= 1 applied: {report:?}");

        // No sibling task-*.md → the empty-prefix counter starts at 1.
        let op = &report.operations[0];
        assert_eq!(op.path.as_deref(), Some("task-1.md"), "resolved path on op");
        assert_eq!(op.stem.as_deref(), Some("task-1"), "resolved stem on op");

        // Serialization carries the resolved fields.
        let json = serde_json::to_value(op).unwrap();
        assert_eq!(json["path"], "task-1.md");
        assert_eq!(json["stem"], "task-1");
    }
}
