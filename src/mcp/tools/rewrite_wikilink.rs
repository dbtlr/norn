//! `vault.rewrite_wikilink` — retarget all occurrences of a wikilink, no file move.
//!
//! A graph-wide cascading mutation: rewrites every `[[OLD]]` / `[[OLD|display]]`
//! body wikilink AND every frontmatter field carrying OLD as a wikilink value,
//! retargeting them to NEW — without moving any file. Copies the mutation-safety
//! contract from `vault.set` / `vault.new` / `vault.move` / `vault.delete`:
//!
//! - **Default DRY-RUN.** A call WITHOUT `confirm: true` runs the applier in
//!   `dry_run = true` mode (which forecasts the rewrite WITHOUT touching any file)
//!   and returns the `ApplyReport` with `dry_run = true` / `applied = 0`. No
//!   mutation lock, no event sink, NOTHING written.
//! - **`confirm: true` WRITES.** Acquires the per-vault mutation lock, opens a
//!   real file-backed event sink (audited like the CLI), and applies the plan
//!   with `dry_run = false`.
//!
//! ## How it mirrors the CLI `norn rewrite-wikilink` (non-TTY path)
//!
//! The CLI's `rewrite_wikilink_cmd::run`:
//! 1. Acquires the mutation lock.
//! 2. Loads config + graph index.
//! 3. Builds a one-op `rewrite_wikilink` `MigrationPlan` (`{old, new}`).
//! 4. Opens an event sink, emits `invocation_started`, applies via
//!    `applier::apply_migration_plan` (the planner's `rewrite_wikilink` expander
//!    fans the one op out into per-file body + frontmatter changes), emits
//!    `invocation_finished`. Pre-flight refusal (exit 2) when OLD is unresolvable
//!    surfaces as an `Err` from `apply_migration_plan`.
//!
//! The MCP `confirm` flag drives apply-vs-dry-run. (The CLI additionally treats
//! `--format json` as an implicit non-interactive APPLY; that knob is a CLI-TTY
//! affordance and is deliberately NOT replicated — an MCP client gets explicit
//! dry-run-by-default via `confirm`.)

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mcp::context::VaultContext;
use crate::mcp::mutation_result::MutationResult;

/// Parameters for `vault.rewrite_wikilink`.
///
/// `from` is the old wikilink target (stem, path, or alias) and `to` is the new
/// target — the same `OLD` / `NEW` arguments `norn rewrite-wikilink` takes. No
/// file is moved; only the link *values* across the vault change.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct RewriteWikilinkParams {
    /// Old wikilink target to find and rewrite (stem, path, or alias).
    /// Mirrors `norn rewrite-wikilink OLD`.
    pub from: String,

    /// New wikilink target to replace `from` with.
    /// Mirrors `norn rewrite-wikilink NEW`.
    pub to: String,

    /// Apply the rewrite. **Defaults to `false` (dry-run): the call returns the
    /// planned rewrite (body + frontmatter occurrences) and writes nothing.**
    /// Pass `true` to acquire the vault mutation lock and rewrite every
    /// occurrence on disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.rewrite_wikilink`.
///
/// Wraps the [`crate::apply_report::ApplyReport`] as a generic `serde_json::Value`
/// inside this typed root struct (the `SetOutput` / `MoveOutput` pattern). The
/// JSON is byte-for-byte what `norn rewrite-wikilink --format json` emits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RewriteWikilinkOutput {
    /// The `ApplyReport` JSON: `dry_run`, the applied/skipped/failed tallies, one
    /// `rewrite_link` / `set_frontmatter` op per rewritten occurrence, and (on
    /// confirm) the trace id. Byte-for-byte the shape
    /// `norn rewrite-wikilink --format json` emits.
    pub report: serde_json::Value,
}

impl RewriteWikilinkOutput {
    fn from_report(report: &crate::apply_report::ApplyReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.rewrite_wikilink`.
pub fn handle_output(
    ctx: &VaultContext,
    p: RewriteWikilinkParams,
) -> Result<MutationResult<RewriteWikilinkOutput>> {
    let report = handle(ctx, p)?;
    // BUG-3 / NRN-219: `isError` derived from the report's outcome. See
    // `apply::handle_output` and `MutationResult::from_apply_report`.
    Ok(MutationResult::from_apply_report(
        RewriteWikilinkOutput::from_report(&report)?,
        &report,
    ))
}

/// Pure handler for `vault.rewrite_wikilink`.
///
/// Returns the `ApplyReport` (same as `norn rewrite-wikilink --format json`).
///
/// DRY-RUN (`!confirm`): load config + index → build one-op `rewrite_wikilink`
/// `MigrationPlan` → `apply_migration_plan` with `dry_run = true`, no lock, no
/// real sink. The applier's dry-run forecasts the body + frontmatter rewrites
/// WITHOUT touching any file (and still refuses, via `Err`, when OLD is
/// unresolvable).
///
/// CONFIRM (`confirm`): same plan, but acquire the mutation lock, open a real
/// event sink, and apply with `dry_run = false`.
pub fn handle(
    ctx: &VaultContext,
    p: RewriteWikilinkParams,
) -> Result<crate::apply_report::ApplyReport> {
    use crate::applier::{apply_migration_plan, ApplyContext};
    use crate::migration_plan::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};

    let cwd = ctx.vault_root.clone();

    // Load the graph index honoring files.ignore, exactly like the CLI path.
    // Warm-connection reuse under the daemon; fresh open in cold mode (NRN-130).
    let index = ctx.load_graph_index()?;

    // Build the one-op MigrationPlan, matching the CLI's fields exactly.
    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: cwd.to_string(),
        generator: None,
        generated_at: None,
        operations: vec![MigrationOp {
            kind: "rewrite_wikilink".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({ "old": p.from, "new": p.to }),
            footnote: None,
        }],
        skipped: vec![],
        plan_footnote: None,
    };

    // The MCP contract: `confirm` drives apply vs dry-run.
    let dry_run = !p.confirm;

    let apply_ctx = ApplyContext {
        dry_run,
        parents: false,
        verbose: false,
        refuse_as_report: true,
    };

    // ── DRY-RUN (default): no lock, discard sink, applier in dry-run mode ───────
    if dry_run {
        let mut sink = crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::new(),
            crate::telemetry::Clock::System,
        );
        // Pre-flight refusal (OLD unresolvable) surfaces as Err here, same as CLI.
        // Propagate the original error so `to_mcp_error` recovers the structured
        // `{ code, message, path? }` envelope (NRN-150).
        let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)?;
        return Ok(report);
    }

    // ── CONFIRM: acquire mutation lock, open real sink, apply ──────────────────
    let _mutation_lock = crate::mcp::mutate::acquire_mutation_lock(&cwd)?;

    // Open a real, file-backed event sink — the same audit trail
    // `norn rewrite-wikilink` writes via `open_event_sink`. `apply_migration_plan`
    // emits the per-op spans + `action` events itself.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "rewrite-wikilink",
        &cwd,
        &plan.vault_root,
        /*dry_run=*/ false,
        &["rewrite-wikilink".to_string(), p.from.clone(), p.to.clone()],
    );

    let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)?;

    crate::emit_invocation_finished(&mut sink, "rewrite-wikilink", report.exit_code(), &report);

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with `old-target.md`, `new-target.md`, and several docs
    /// linking `[[old-target]]` in body and frontmatter.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-rewrite-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("old-target.md"), "---\ntype: note\n---\nold\n").unwrap();
        std::fs::write(root.join("new-target.md"), "---\ntype: note\n---\nnew\n").unwrap();
        std::fs::write(
            root.join("a.md"),
            "---\nrel: \"[[old-target]]\"\n---\nBody [[old-target]] and [[old-target|disp]].\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md"),
            "---\ntype: note\n---\nAlso [[old-target]] here.\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn count_occurrences(root: &Utf8PathBuf, file: &str, needle: &str) -> usize {
        std::fs::read_to_string(root.join(file))
            .unwrap()
            .matches(needle)
            .count()
    }

    /// Core mutation-safety contract: dry-run (default `confirm: false`) reports
    /// `dry_run = true` AND no `[[old-target]]` occurrence changes on disk.
    #[test]
    fn dry_run_default_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            RewriteWikilinkParams {
                from: "old-target".into(),
                to: "new-target".into(),
                confirm: false,
            },
        )
        .expect("handle (dry-run) should succeed");

        assert!(report.dry_run, "dry-run report must have dry_run == true");
        assert_eq!(report.applied, 0, "dry-run must report 0 applied");

        // CRITICAL: every old-target occurrence is intact (2 body + 1 frontmatter
        // = 3 `[[old-target` substrings), and none of new-target's appear.
        assert_eq!(
            count_occurrences(&root, "a.md", "[[old-target"),
            3,
            "dry-run must leave a.md's two body links + one frontmatter wikilink intact"
        );
        assert!(
            !std::fs::read_to_string(root.join("a.md"))
                .unwrap()
                .contains("new-target"),
            "dry-run must NOT introduce any new-target occurrence in a.md"
        );
        assert_eq!(
            count_occurrences(&root, "b.md", "[[old-target]]"),
            1,
            "dry-run must leave b.md's [[old-target]] intact"
        );
    }

    /// `confirm: true` retargets every `[[old-target]]` occurrence to
    /// `[[new-target]]` across body AND frontmatter.
    #[test]
    fn confirm_rewrites_body_and_frontmatter() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            RewriteWikilinkParams {
                from: "old-target".into(),
                to: "new-target".into(),
                confirm: true,
            },
        )
        .expect("handle (confirm) should succeed");

        assert!(!report.dry_run, "confirm report must have dry_run == false");
        assert!(report.applied >= 1, "confirm must report >= 1 applied");

        // a.md: body links + frontmatter wikilink all retargeted.
        let a = std::fs::read_to_string(root.join("a.md")).unwrap();
        assert!(
            !a.contains("old-target"),
            "confirm must remove every old-target occurrence from a.md:\n{a}"
        );
        assert!(
            a.contains("[[new-target]]")
                && a.contains("[[new-target|disp]]")
                && a.contains("rel: \"[[new-target]]\""),
            "confirm must retarget a.md's body AND frontmatter wikilinks:\n{a}"
        );

        // b.md: body link retargeted.
        let b = std::fs::read_to_string(root.join("b.md")).unwrap();
        assert!(
            b.contains("[[new-target]]") && !b.contains("[[old-target]]"),
            "confirm must retarget b.md's body link:\n{b}"
        );
    }
}
