//! `vault.move` — move/rename a document, cascading backlink rewrites.
//!
//! Copies the mutation-safety contract established by `vault.set` (Task 9) and
//! `vault.new` (Task 10):
//!
//! - **Default DRY-RUN.** A call WITHOUT `confirm: true` builds the one-op
//!   `MigrationPlan`, runs the applier with `dry_run = true` (which performs the
//!   preflight/expansion and reports the forecast cascade WITHOUT writing), and
//!   returns the `ApplyReport` with `dry_run = true` / `applied = 0`. It acquires
//!   NO mutation lock and opens NO event sink.
//! - **`confirm: true` WRITES.** Acquires the per-vault mutation lock, opens a
//!   real file-backed event sink (so the mutation is audited exactly like the
//!   CLI), and applies the plan with `dry_run = false` — moving the file and
//!   cascading the backlink rewrites across the vault.
//!
//! ## How it mirrors the CLI `norn move` (non-TTY / `--format json` path)
//!
//! The CLI's `Command::Move` dispatch in `main.rs`:
//! 1. Acquires the mutation lock (`is_apply` driven by TTY / `--yes`).
//! 2. Loads config + graph index.
//! 3. Runs `r#move::preflight_and_plan` for the single-file path (exit 2 on
//!    refusal).
//! 4. Builds a one-op `MigrationPlan` (kind `move_document` or `move_folder`).
//! 5. Opens an event sink, emits `invocation_started`, applies via
//!    `applier::apply_migration_plan`, emits `invocation_finished`.
//!
//! Every MCP call is non-TTY, so the CLI's "non-TTY + `--format json` = implicit
//! dry-run" semantics map onto `confirm = false`, and the `--yes` apply path maps
//! onto `confirm = true`. We replicate the same pieces: the same preflight, the
//! same `MigrationPlan` shape, the same `apply_migration_plan`, the same event
//! emit helpers — so `vault.move` and `norn move` cannot drift.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mcp::context::{RequestScope, VaultContext};
use crate::mcp::mutation_result::MutationResult;

/// Parameters for `vault.move`.
///
/// `from` is the source and `to` is the destination — the same `SRC` / `DST`
/// arguments `norn move` takes. `recursive` and `parents` mirror `-r` / `-p`.
///
/// `from` resolves exactly like the CLI's stem-resolving preflight (NRN-239):
/// an exact vault-relative path match first, then a case-insensitive stem
/// match, refusing with the coded `target-ambiguous` when more than one
/// document shares the stem. The plan is built from the RESOLVED path, so a
/// bare stem (`a`) applies to the same document `norn move a dst` would.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct MoveParams {
    /// Source document: a vault-relative path (`notes/a.md`) or a bare stem
    /// (`a`). A bare stem resolves like `norn move SRC` does — exact path
    /// first, then case-insensitive stem match, refusing `target-ambiguous`
    /// on more than one match.
    pub from: String,

    /// Destination: vault-relative path (`norn move DST`).
    pub to: String,

    /// When `from` and `to` are directories, recursively move all `.md` files
    /// preserving structure (one cascade pass). Mirrors `norn move -r`.
    #[serde(default)]
    pub recursive: bool,

    /// Auto-create missing destination parent directories before moving.
    /// Mirrors `norn move -p` / `--parents`.
    #[serde(default)]
    pub parents: bool,

    /// Overwrite the destination if it already exists. Without this, a move onto
    /// an existing path is refused. Mirrors `norn move --force`. (Single-file
    /// moves only — like the CLI, it is inert on a folder move.)
    #[serde(default)]
    pub force: bool,

    /// Move the file but skip the cascading backlink rewrites — incoming
    /// `[[wikilinks]]` are left pointing at the old target. Mirrors
    /// `norn move --no-link-rewrite`. (Single-file moves only, like the CLI.)
    #[serde(default)]
    pub no_link_rewrite: bool,

    /// Apply the move. **Defaults to `false` (dry-run): the call returns the
    /// planned move (with the forecast backlink cascade) and writes nothing.**
    /// Pass `true` to acquire the vault mutation lock and move the file +
    /// rewrite backlinks on disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.move`.
///
/// Wraps the [`crate::apply_report::ApplyReport`] as a generic `serde_json::Value`
/// inside this typed root struct, mirroring the `SetOutput` / `NewOutput` pattern:
/// rmcp requires a `type: object` root `outputSchema`, and `ApplyReport` (with its
/// nested cascade/op structs) is left generic so we don't have to derive
/// `JsonSchema` across the whole tree. The JSON is byte-for-byte what
/// `norn move --format json` emits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MoveOutput {
    /// The `ApplyReport` JSON: `dry_run`, the applied/skipped/failed tallies, the
    /// per-op records with their backlink cascade summaries, and (on confirm) the
    /// trace id. Byte-for-byte the same shape `norn move --format json` emits.
    pub report: serde_json::Value,
}

impl MoveOutput {
    fn from_report(report: &crate::apply_report::ApplyReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.move`.
pub fn handle_output(
    ctx: &VaultContext,
    scope: &RequestScope,
    p: MoveParams,
) -> Result<MutationResult<MoveOutput>> {
    let dry_run = !p.confirm;
    // Folder vs single-file — for the refused-report op label only (cheap; a
    // refusal moved nothing so `from` is unchanged on disk).
    let op_kind = if p.recursive || ctx.vault_root.join(&p.from).as_std_path().is_dir() {
        "move_folder"
    } else {
        "move_document"
    };
    let vault_root = ctx.vault_root.to_string();
    // Capture a coded refusal (NRN-220/229): a recognized preflight refusal
    // (`target-not-found`, `destination-exists`, …) or a mutation-lock timeout
    // becomes a structured `refused` report + `isError` (on confirm) instead of a
    // bare MCP `Err` with the code laundered to prose. Others still propagate.
    let report = match handle(ctx, scope, p) {
        Ok(report) => report,
        Err(e) => match crate::mcp::mutate::refusal_from_error(&e) {
            Some(err) => {
                crate::apply_report::ApplyReport::refused(vault_root, dry_run, op_kind, err)
            }
            None => return Err(e),
        },
    };
    // BUG-3 / NRN-219: `isError` derived from the report's outcome. See
    // `apply::handle_output` and `MutationResult::from_apply_report`.
    Ok(MutationResult::from_apply_report(
        MoveOutput::from_report(&report)?,
        &report,
    ))
}

/// Pure handler for `vault.move`.
///
/// Returns the `ApplyReport` (same as `norn move --format json`).
///
/// DRY-RUN (`!confirm`): load config + index → single-file preflight (exit on
/// refusal) → build one-op `MigrationPlan` → `apply_migration_plan` with
/// `dry_run = true`, no lock, no real sink. The applier's dry-run forecasts the
/// cascade without writing.
///
/// CONFIRM (`confirm`): acquire the mutation lock FIRST — before the index
/// load and preflight — then run the same plan, open a real event sink, and
/// apply with `dry_run = false`. Destination parent-directory creation
/// (`--parents`) happens inside `apply_migration_plan`, not here (NRN-234).
pub fn handle(
    ctx: &VaultContext,
    scope: &RequestScope,
    p: MoveParams,
) -> Result<crate::apply_report::ApplyReport> {
    use crate::applier::{apply_migration_plan, ApplyContext};
    use crate::migration_plan::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};

    let cwd = ctx.vault_root.clone();

    // CONFIRM locks BEFORE any read that feeds the write; dry-run never locks.
    // See `crate::mcp::mutate::acquire_mutation_lock` for the invariant.
    let _mutation_lock = if p.confirm {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    } else {
        None
    };

    // Load the graph index honoring files.ignore, exactly like the CLI move path.
    // Warm-connection reuse under the daemon; fresh open in cold mode (NRN-130).
    let index = ctx.load_graph_index(scope)?;

    // Folder vs single-file detection, matching the CLI: explicit `-r` OR `from`
    // is a directory on disk routes through `move_folder`.
    let src_full = cwd.join(&p.from);
    let src_is_dir = src_full.as_std_path().is_dir();
    let is_folder = p.recursive || src_is_dir;

    // Single-file preflight: refuse early (the CLI exits 2). `parents: p.parents`
    // (NRN-234) tells preflight to skip its missing-destination-parent refusal
    // when `--parents` was requested — the parent is created inside the applier
    // as part of the audited apply step (the same shared containment gate that
    // already handles a not-yet-existing `-p`/`--parents` subtree), identically
    // on dry-run and confirm, matching the CLI.
    //
    // NRN-239: capture the RESOLVED plan (mirrors the CLI direct arm's
    // `Command::Move` handling in `src/lib.rs`) instead of discarding it.
    // `preflight_and_plan`
    // resolves a bare stem (e.g. "a") to its full vault-relative path (e.g.
    // "a.md") via `resolve_src`; the raw `from` may not match a real filesystem
    // path at all. Building `MigrationOp.fields` from `p.from` verbatim (the old
    // behavior) let a resolvable stem pass THIS preflight while the apply plan
    // still carried the raw stem, so the applier either no-op'd or acted on the
    // wrong on-disk entry. The destination stays the RAW `p.to` — the CLI never
    // stem-resolves destinations either.
    let resolved_src = if !is_folder {
        let cfg = crate::r#move::PreflightConfig {
            src: &p.from,
            dst: &p.to,
            force: p.force,
            no_link_rewrite: p.no_link_rewrite,
            parents: p.parents,
            vault_root: &cwd,
            index: &index,
        };
        // NRN-229: propagate the TYPED `MovePreflightError` (not a bail'd string)
        // so `handle_output` recovers its `.code()` via `refusal_from_error` and
        // returns a coded, structured refusal instead of laundering to
        // `internal-error`. The `Display` prose is unchanged.
        let plan = crate::r#move::preflight_and_plan(cfg)?;
        Some(plan.expect_change("move_document").path.to_string())
    } else {
        None
    };

    // Build the one-op MigrationPlan, matching the CLI's fields exactly.
    let op_kind = if is_folder {
        "move_folder"
    } else {
        "move_document"
    };
    // Folder moves have no preflight plan (a folder path isn't stem-resolved) —
    // fall back to the raw `p.from`, matching the CLI's folder-move arm.
    let mut fields = serde_json::json!({
        "src": resolved_src.unwrap_or_else(|| p.from.clone()),
        "dst": p.to.clone(),
        "parents": p.parents,
    });
    // Mirror the CLI: `force` / `no_link_rewrite` are single-file op fields, added
    // only when set. Folder moves route through the expander and ignore them.
    if !is_folder && p.force {
        fields["force"] = serde_json::Value::Bool(true);
    }
    if !is_folder && p.no_link_rewrite {
        fields["no_link_rewrite"] = serde_json::Value::Bool(true);
    }
    let migration_plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: cwd.to_string(),
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations: vec![MigrationOp {
            kind: op_kind.into(),
            id: None,
            requires: vec![],
            fields,
            footnote: None,
        }],
        skipped: vec![],
        plan_footnote: None,
    };

    // The MCP contract: `confirm` drives apply vs dry-run (the CLI's TTY/--yes/
    // --format knobs are inert for an always-non-TTY MCP client).
    let dry_run = !p.confirm;

    let apply_ctx = ApplyContext {
        dry_run,
        parents: p.parents,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: Default::default(),
    };

    // ── DRY-RUN (default): no lock, discard sink, applier in dry-run mode ───────
    if dry_run {
        let mut sink = crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::new(),
            crate::telemetry::Clock::System,
        );
        // Propagate the original error so `to_mcp_error` recovers the structured
        // `{ code, message, path? }` envelope (NRN-150).
        let report = apply_migration_plan(&migration_plan, &index, apply_ctx, &mut sink)?;
        return Ok(report);
    }

    // ── CONFIRM: the mutation lock was already acquired above, before the
    // index load — open the real sink and apply.

    // Open a real, file-backed event sink — the same audit trail `norn move`
    // writes via `open_event_sink`. `apply_migration_plan` emits the per-op
    // `op_planned` spans and `action` events itself, so we only frame it with
    // `invocation_started` / `invocation_finished`.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx, scope);
    crate::emit_invocation_started(
        &mut sink,
        "move",
        &cwd,
        &migration_plan.vault_root,
        /*dry_run=*/ false,
        &["move".to_string(), p.from.clone(), p.to.clone()],
    );

    let report =
        crate::applier::apply_migration_plan(&migration_plan, &index, apply_ctx, &mut sink)?;

    crate::emit_invocation_finished(&mut sink, "move", report.exit_code(), &report);

    // After a live folder move, clean up empty source directories — mirrors the
    // CLI's `remove_empty_dirs` call on the move_folder apply path.
    if is_folder && report.exit_code() == 0 {
        crate::remove_empty_dirs(src_full.as_std_path());
    }

    // Warm mode: commit the move's cache increments (source + destination +
    // backlink cascade) as a chunked writer-queue op, awaited; no-op in cold mode
    // (NRN-252 / NRN-158).
    ctx.commit_apply_increments(scope, &report.touched_paths, index);

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::mcp::tools::scoped_shim! {
        fn handle(MoveParams) -> crate::apply_report::ApplyReport;
        fn handle_output(MoveParams) -> crate::mcp::mutation_result::MutationResult<MoveOutput>;
    }
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with `a.md` and `b.md`, where `b.md` links to `[[a]]`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-move-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("a.md"), "---\ntype: note\n---\nA body\n").unwrap();
        std::fs::write(
            root.join("b.md"),
            "---\ntype: note\n---\nLinks to [[a]] here.\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Core mutation-safety contract: dry-run (default `confirm: false`) reports
    /// `dry_run = true` AND leaves both files untouched on disk — `a.md` still at
    /// its original path and `b.md`'s `[[a]]` link unchanged.
    #[test]
    fn dry_run_default_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "renamed.md".into(),
                recursive: false,
                parents: false,
                force: false,
                no_link_rewrite: false,
                confirm: false,
            },
        )
        .expect("handle (dry-run) should succeed");

        assert!(report.dry_run, "dry-run report must have dry_run == true");
        assert_eq!(report.applied, 0, "dry-run must report 0 applied");

        // CRITICAL: the source file is still at its original path, the
        // destination does not exist, and b.md's backlink is unchanged.
        assert!(
            root.join("a.md").exists(),
            "dry-run must NOT move a.md off its path"
        );
        assert!(
            !root.join("renamed.md").exists(),
            "dry-run must NOT create the destination"
        );
        let b = std::fs::read_to_string(root.join("b.md")).unwrap();
        assert!(
            b.contains("[[a]]"),
            "dry-run must leave b.md's backlink unchanged:\n{b}"
        );
    }

    /// `confirm: true` moves `a.md` to the new path AND cascades the backlink
    /// rewrite in `b.md`.
    #[test]
    fn confirm_moves_and_rewrites_backlink() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "renamed.md".into(),
                recursive: false,
                parents: false,
                force: false,
                no_link_rewrite: false,
                confirm: true,
            },
        )
        .expect("handle (confirm) should succeed");

        assert!(!report.dry_run, "confirm report must have dry_run == false");
        assert!(report.applied >= 1, "confirm must report >= 1 applied");

        // The file moved.
        assert!(
            !root.join("a.md").exists(),
            "confirm must move a.md off its original path"
        );
        assert!(
            root.join("renamed.md").exists(),
            "confirm must create the destination renamed.md"
        );

        // The backlink in b.md was rewritten to the new stem.
        let b = std::fs::read_to_string(root.join("b.md")).unwrap();
        assert!(
            b.contains("[[renamed]]"),
            "confirm must rewrite b.md's backlink to the new target:\n{b}"
        );
        assert!(
            !b.contains("[[a]]"),
            "confirm must not leave the old backlink:\n{b}"
        );
    }

    /// NRN-180: `no_link_rewrite` moves the file but leaves incoming backlinks
    /// untouched — `b.md` still points at `[[a]]` after the move.
    #[test]
    fn confirm_no_link_rewrite_leaves_backlink() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "renamed.md".into(),
                no_link_rewrite: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (no_link_rewrite) should succeed");

        assert!(report.applied >= 1, "the move itself must still apply");
        // File moved…
        assert!(
            root.join("renamed.md").exists(),
            "the file must still move even with no_link_rewrite"
        );
        // …but the backlink was deliberately NOT rewritten.
        let b = std::fs::read_to_string(root.join("b.md")).unwrap();
        assert!(
            b.contains("[[a]]"),
            "no_link_rewrite must leave b.md's [[a]] backlink unchanged:\n{b}"
        );
        assert!(
            !b.contains("[[renamed]]"),
            "no_link_rewrite must NOT rewrite the backlink to the new stem:\n{b}"
        );
    }

    /// NRN-180: without `force`, a move onto an existing destination is refused;
    /// with `force`, it overwrites.
    #[test]
    fn force_overwrites_existing_destination() {
        let (_tmp, root) = seeded_vault();
        // `b.md` already exists; moving `a.md` onto it must be refused by default.
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let refused = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "b.md".into(),
                confirm: true,
                ..Default::default()
            },
        );
        assert!(
            refused.is_err(),
            "a move onto an existing destination must be refused without force"
        );

        // With force, the same move is allowed (destination overwritten).
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let report = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "b.md".into(),
                force: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (force) should succeed");
        assert!(report.applied >= 1, "force move must apply");
        assert!(
            !root.join("a.md").exists(),
            "force move must remove the source"
        );
        assert!(
            root.join("b.md").exists(),
            "force move must leave the (overwritten) destination in place"
        );
    }

    /// NRN-234: `confirm: false` (dry-run) with `parents: true` must not create
    /// the missing destination parent directory — the applier, not the MCP
    /// handler, owns parent creation, and only as part of a real apply.
    #[test]
    fn dry_run_parents_creates_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "deep/nested/renamed.md".into(),
                parents: true,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run + parents) should succeed");

        assert!(report.dry_run, "dry-run report must have dry_run == true");
        assert!(
            !root.join("deep").exists(),
            "dry-run with parents:true must not create the destination parent directory"
        );
        assert!(
            root.join("a.md").exists(),
            "dry-run must not move the source"
        );
    }

    /// NRN-229: a CONFIRM preflight refusal (destination exists, no force) is a
    /// structured `refused` report + `isError:true` carrying the stable
    /// `destination-exists` code — NOT a bare MCP `Err` laundered to prose.
    #[test]
    fn confirm_refusal_is_structured_and_coded() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle_output(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "b.md".into(), // b.md exists → destination-exists
                confirm: true,
                ..Default::default()
            },
        )
        .expect("a coded refusal must be Ok(structured), not Err");

        assert!(
            result.is_error(),
            "a confirmed refusal maps to isError:true"
        );
        let report = &result.value().report;
        assert_eq!(report["outcome"], "refused");
        assert_eq!(
            report["operations"][0]["error"]["code"],
            "destination-exists"
        );
        // A refusal writes nothing: the source is untouched.
        assert!(
            root.join("a.md").exists(),
            "a refused move must not move the source"
        );
    }

    /// NRN-229: the SAME refusal on a `confirm: false` forecast still carries the
    /// coded `outcome: refused` report but stays `isError:false` — a forecasted
    /// refusal is not a failed tool call (the edit/new/set dry-run contract).
    #[test]
    fn dry_run_refusal_is_structured_but_not_is_error() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle_output(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "b.md".into(),
                confirm: false,
                ..Default::default()
            },
        )
        .expect("a coded refusal must be Ok(structured), not Err");

        assert!(
            !result.is_error(),
            "a dry-run refusal forecast stays isError:false"
        );
        assert_eq!(result.value().report["outcome"], "refused");
        assert_eq!(
            result.value().report["operations"][0]["error"]["code"],
            "destination-exists"
        );
    }

    /// NRN-234: `confirm: true` with `parents: true` creates the missing
    /// destination parent directory via the applier and moves the file — the
    /// same applier path `norn move --parents --yes` uses.
    #[test]
    fn confirm_parents_creates_missing_dst_dirs() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            MoveParams {
                from: "a.md".into(),
                to: "deep/nested/renamed.md".into(),
                parents: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm + parents) should succeed");

        assert!(!report.dry_run, "confirm report must have dry_run == false");
        assert!(report.applied >= 1, "confirm must report >= 1 applied");
        assert!(
            root.join("deep/nested/renamed.md").exists(),
            "confirm with parents:true must create the destination parent directories and move the file"
        );
        assert!(
            !root.join("a.md").exists(),
            "confirm must move a.md off its original path"
        );
    }

    /// NRN-239: `from` given as a bare stem resolves through preflight exactly
    /// like the CLI — the RESOLVED `a.md` is what gets planned and moved, not
    /// the raw stem string (which previously reached the applier verbatim and
    /// either no-op'd or moved the wrong on-disk entry).
    #[test]
    fn confirm_bare_stem_resolves_and_moves() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            MoveParams {
                from: "a".into(), // bare stem, not "a.md"
                to: "renamed.md".into(),
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (bare stem) should succeed");

        assert!(!report.dry_run, "confirm report must have dry_run == false");
        assert!(report.applied >= 1, "bare-stem move must apply");
        assert!(
            !root.join("a.md").exists(),
            "bare-stem move must move the RESOLVED a.md off its path"
        );
        assert!(
            root.join("renamed.md").exists(),
            "bare-stem move must create the destination"
        );
    }

    /// NRN-239: an ambiguous bare stem (two docs sharing the same stem) is
    /// refused with the coded `target-ambiguous` — not a silent move of the
    /// wrong file and not a bare-`Err` laundered to prose.
    #[test]
    fn confirm_ambiguous_stem_is_refused() {
        let (_tmp, root) = seeded_vault();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/a.md"), "---\ntype: note\n---\nAnother A\n").unwrap();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let result = handle_output(
            &ctx,
            MoveParams {
                from: "a".into(), // ambiguous: a.md AND sub/a.md share the stem "a"
                to: "renamed.md".into(),
                confirm: true,
                ..Default::default()
            },
        )
        .expect("a coded refusal must be Ok(structured), not Err");

        assert!(
            result.is_error(),
            "a confirmed ambiguous-stem refusal maps to isError:true"
        );
        let report = &result.value().report;
        assert_eq!(report["outcome"], "refused");
        assert_eq!(report["operations"][0]["error"]["code"], "target-ambiguous");
        // Nothing moved: both candidates are untouched.
        assert!(root.join("a.md").exists());
        assert!(root.join("sub/a.md").exists());
    }
}
