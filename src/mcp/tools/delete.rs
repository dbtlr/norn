//! `vault.delete` — delete a document, optionally redirecting incoming links.
//!
//! A DESTRUCTIVE cascading mutation. Copies the mutation-safety contract from
//! `vault.set` / `vault.new` / `vault.move`, where the DRY-RUN-by-default
//! property is paramount: a call WITHOUT `confirm: true` MUST NOT delete the
//! file.
//!
//! - **Default DRY-RUN.** A call WITHOUT `confirm: true` runs the delete preflight
//!   (backlink-policy refusal, exit on Err) and the applier in `dry_run = true`
//!   mode — which forecasts the cascade WITHOUT removing the file or rewriting any
//!   link — and returns the `ApplyReport` with `dry_run = true` / `applied = 0`.
//!   No mutation lock, no event sink, NOTHING removed from disk.
//! - **`confirm: true` WRITES.** Acquires the per-vault mutation lock, opens a
//!   real file-backed event sink (audited like the CLI), and applies the plan
//!   with `dry_run = false` — deleting the document and (if `rewrite_to` is set)
//!   redirecting incoming links to the alternate target.
//!
//! ## How it mirrors the CLI `norn delete` (non-TTY / `--format json` path)
//!
//! The CLI's `Command::Delete` dispatch in `main.rs`:
//! 1. Acquires the mutation lock.
//! 2. Loads config + graph index.
//! 3. Runs `delete_doc::preflight_and_plan` (exit 2 on refusal — e.g. incoming
//!    links present without `--allow-broken-links` or `--rewrite-to`).
//! 4. Builds a one-op `delete_document` `MigrationPlan` with the same
//!    `{path, rewrite_to, allow_broken_links}` fields.
//! 5. Opens an event sink, emits `invocation_started`, applies via
//!    `applier::apply_migration_plan`, emits `invocation_finished`.
//!
//! The MCP `confirm` flag drives apply-vs-dry-run (the CLI's TTY/`--yes`/
//! `--format` knobs are inert for an always-non-TTY MCP client).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::mcp::context::VaultContext;
use crate::mcp::mutation_result::MutationResult;

/// Parameters for `vault.delete`.
///
/// `target` is the document to delete. `rewrite_to` redirects every incoming
/// link to an alternate document before deleting (mutually exclusive with
/// `allow_broken_links`). `allow_broken_links` acknowledges that incoming links
/// will break — required when the doc has incoming links and `rewrite_to` is not
/// given, matching `norn delete`'s refusal policy.
///
/// NOTE (mirrors a CLI quirk): the `delete_document` op resolves `target` as a
/// **vault-relative path** (`doc.md`), not a bare stem — the CLI's
/// stem-resolving preflight does not feed the apply plan, so `norn delete doc`
/// also no-ops on the apply side. `vault.delete` mirrors that exactly.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct DeleteParams {
    /// Document to delete: vault-relative path (e.g. `notes/doc.md`).
    pub target: String,

    /// Redirect every incoming link to this alternate document before deleting.
    /// Mirrors `norn delete --rewrite-to <ALT_DOC>`. Mutually exclusive with
    /// `allow_broken_links` (the preflight rejects both).
    #[serde(default)]
    pub rewrite_to: Option<String>,

    /// Acknowledge that incoming links will break and surface as
    /// link-target-missing findings. Required when the doc has incoming links and
    /// `rewrite_to` is not provided. Mirrors `norn delete --allow-broken-links`.
    #[serde(default)]
    pub allow_broken_links: bool,

    /// Apply the deletion. **Defaults to `false` (dry-run): the call returns the
    /// planned delete (with the forecast cascade) and removes NOTHING.** Pass
    /// `true` to acquire the vault mutation lock and delete the file (and redirect
    /// incoming links when `rewrite_to` is set).
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.delete`.
///
/// Wraps the [`crate::apply_report::ApplyReport`] as a generic `serde_json::Value`
/// inside this typed root struct (the `SetOutput` / `MoveOutput` pattern). The
/// JSON is byte-for-byte what `norn delete --format json` emits.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeleteOutput {
    /// The `ApplyReport` JSON: `dry_run`, the applied/skipped/failed tallies, the
    /// per-op delete record with its incoming-link cascade summary, and (on
    /// confirm) the trace id. Byte-for-byte the shape `norn delete --format json`
    /// emits.
    pub report: serde_json::Value,
}

impl DeleteOutput {
    fn from_report(report: &crate::apply_report::ApplyReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.delete`.
pub fn handle_output(ctx: &VaultContext, p: DeleteParams) -> Result<MutationResult<DeleteOutput>> {
    let report = handle(ctx, p)?;
    // BUG-3 / NRN-219: `isError` derived from the report's outcome. See
    // `apply::handle_output` and `MutationResult::from_apply_report`.
    Ok(MutationResult::from_apply_report(
        DeleteOutput::from_report(&report)?,
        &report,
    ))
}

/// Pure handler for `vault.delete`.
///
/// Returns the `ApplyReport` (same as `norn delete --format json`).
///
/// DRY-RUN (`!confirm`): load config + index → delete preflight (refuse on Err) →
/// build one-op `delete_document` `MigrationPlan` → `apply_migration_plan` with
/// `dry_run = true`, no lock, no real sink. The applier's dry-run forecasts the
/// cascade WITHOUT removing the file.
///
/// CONFIRM (`confirm`): acquire the mutation lock FIRST — before the index load
/// and preflight — then run the same plan, open a real event sink, and apply
/// with `dry_run = false`.
pub fn handle(ctx: &VaultContext, p: DeleteParams) -> Result<crate::apply_report::ApplyReport> {
    use crate::applier::{apply_migration_plan, ApplyContext};
    use crate::migration_plan::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};

    let cwd = ctx.vault_root.clone();

    // The MCP contract: `confirm` drives apply vs dry-run.
    let dry_run = !p.confirm;

    // CONFIRM acquires the per-vault mutation lock BEFORE the preflight read —
    // matching `norn delete` (main.rs), which locks before loading the graph
    // index. The lock must span the index load + preflight + apply so a
    // concurrent norn writer can't drift the vault in the read→apply window.
    // The DRY-RUN path is read-only and takes NO lock.
    let _mutation_lock = if dry_run {
        None
    } else {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    };

    // Load the graph index honoring files.ignore, exactly like the CLI delete path.
    // Warm-connection reuse under the daemon; fresh open in cold mode (NRN-130).
    let index = ctx.load_graph_index()?;

    // Preflight: resolve the doc + enforce the backlinks policy. Refuse early
    // (the CLI exits 2): incoming-links-present without --rewrite-to /
    // --allow-broken-links, ambiguous/missing target, bad rewrite_to, etc.
    let cfg = crate::delete_doc::PreflightConfig {
        doc: &p.target,
        allow_broken_links: p.allow_broken_links,
        rewrite_to: p.rewrite_to.as_deref(),
        vault_root: &cwd,
        index: &index,
    };
    if let Err(e) = crate::delete_doc::preflight_and_plan(cfg) {
        anyhow::bail!("{e}");
    }

    // Build the one-op MigrationPlan, matching the CLI's fields exactly.
    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: cwd.to_string(),
        generator: None,
        generated_at: None,
        operations: vec![MigrationOp {
            kind: "delete_document".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({
                "path": p.target,
                "rewrite_to": p.rewrite_to.as_ref(),
                "allow_broken_links": p.allow_broken_links,
            }),
            footnote: None,
        }],
        skipped: vec![],
        plan_footnote: None,
    };

    let apply_ctx = ApplyContext {
        dry_run,
        parents: false,
        verbose: false,
        refuse_as_report: true,
    };

    // ── DRY-RUN (default): no lock, discard sink, applier in dry-run mode ───────
    // The applier's dry-run path performs the forecast WITHOUT removing the file —
    // this is the destructive-safety guarantee. Verified by a read-back test that
    // asserts the file still exists after a confirm:false call.
    if dry_run {
        let mut sink = crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::new(),
            crate::telemetry::Clock::System,
        );
        // Propagate the original error so `to_mcp_error` recovers the structured
        // `{ code, message, path? }` envelope (NRN-150).
        let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)?;
        return Ok(report);
    }

    // ── CONFIRM: the mutation lock was already acquired above, before the
    // index load — open the real sink and apply.

    // Open a real, file-backed event sink — the same audit trail `norn delete`
    // writes via `open_event_sink`. `apply_migration_plan` emits the per-op
    // spans + `action` events itself; we frame it with the lifecycle events.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "delete",
        &cwd,
        &plan.vault_root,
        /*dry_run=*/ false,
        &["delete".to_string(), p.target.clone()],
    );

    let report = apply_migration_plan(&plan, &index, apply_ctx, &mut sink)?;

    crate::emit_invocation_finished(&mut sink, "delete", report.exit_code(), &report);

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with `doc.md`, `alt.md` (a redirect target), and
    /// `linker.md` linking `[[doc]]`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-delete-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("doc.md"), "---\ntype: note\n---\nDoc body\n").unwrap();
        std::fs::write(root.join("alt.md"), "---\ntype: note\n---\nAlt body\n").unwrap();
        std::fs::write(
            root.join("linker.md"),
            "---\ntype: note\n---\nLinks to [[doc]] here.\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// THE critical destructive-safety property: a `confirm: false` delete
    /// reports `dry_run = true` AND `doc.md` STILL EXISTS on disk.
    #[test]
    fn dry_run_default_does_not_delete() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            DeleteParams {
                target: "doc.md".into(),
                rewrite_to: None,
                allow_broken_links: true, // doc has an incoming link; ack to pass preflight
                confirm: false,
            },
        )
        .expect("handle (dry-run) should succeed");

        assert!(report.dry_run, "dry-run report must have dry_run == true");
        assert_eq!(report.applied, 0, "dry-run must report 0 applied");

        // CRITICAL: the file must STILL EXIST after the dry-run.
        assert!(
            root.join("doc.md").exists(),
            "dry-run must NOT delete doc.md"
        );
        // And the linker is untouched.
        assert!(std::fs::read_to_string(root.join("linker.md"))
            .unwrap()
            .contains("[[doc]]"));
    }

    /// `confirm: true` with `allow_broken_links` deletes `doc.md` and leaves the
    /// incoming link broken (no redirect).
    #[test]
    fn confirm_deletes_with_allow_broken_links() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            DeleteParams {
                target: "doc.md".into(),
                rewrite_to: None,
                allow_broken_links: true,
                confirm: true,
            },
        )
        .expect("handle (confirm) should succeed");

        assert!(!report.dry_run, "confirm report must have dry_run == false");
        assert!(report.applied >= 1, "confirm must report >= 1 applied");

        // The file is GONE.
        assert!(
            !root.join("doc.md").exists(),
            "confirm must delete doc.md from disk"
        );
        // The incoming link is left as-is (broken), since allow_broken_links.
        assert!(std::fs::read_to_string(root.join("linker.md"))
            .unwrap()
            .contains("[[doc]]"));
    }

    /// `confirm: true` with `rewrite_to` deletes `doc.md` AND redirects the
    /// incoming link in `linker.md` to the alternate target.
    #[test]
    fn confirm_deletes_with_rewrite_to_redirects_links() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            DeleteParams {
                target: "doc.md".into(),
                rewrite_to: Some("alt.md".into()),
                allow_broken_links: false,
                confirm: true,
            },
        )
        .expect("handle (confirm + rewrite_to) should succeed");

        assert!(!report.dry_run);
        assert!(
            !root.join("doc.md").exists(),
            "confirm must delete doc.md from disk"
        );

        // The incoming link was redirected to [[alt]].
        let linker = std::fs::read_to_string(root.join("linker.md")).unwrap();
        assert!(
            linker.contains("[[alt]]") && !linker.contains("[[doc]]"),
            "confirm + rewrite_to must redirect the incoming link to [[alt]]:\n{linker}"
        );
    }
}
