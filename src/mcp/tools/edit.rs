//! `vault.edit` — sub-document partial edits over MCP. Dry-run by default;
//! `confirm:true` acquires the lock, applies, and audits to the event stream.
//! Mirrors `norn edit`'s dispatch via the shared `edit::synth` preflight.
//!
//! This is a faithful sibling of `vault.set`: same default-DRY-RUN contract,
//! same per-vault mutation lock on the confirm path, same applier
//! (`repair_apply::apply_repair_plan_with_context`), same trace-id source. The
//! only difference is the payload — an ordered `edits` array routed through the
//! pure `edit::transform::apply_edits` body transform rather than a frontmatter
//! field map. The new body is stamped as a single `replace_body` op via the
//! shared `set::synth::inject_body_change` seam, so `vault.edit` and `vault.set`
//! cannot drift on resolution, lock, or apply semantics.

use crate::edit::ops::EditOp;
use crate::edit::report::EditReport;
use crate::mcp::context::{RequestScope, VaultContext};
use crate::mcp::mutation_result::MutationResult;
use anyhow::Result;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

/// Parameters for `vault.edit`.
///
/// The `edits` array is the ordered op list applied all-or-nothing — each op
/// against the result of the prior. Ops are internally tagged by `op`
/// (`str_replace`, `replace_section`, `append_to_section`, `delete_section`,
/// `insert_before_heading`, `insert_after_heading`), identical to what
/// `norn edit` accepts via `--edits-json` / stdin.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct EditParams {
    /// Target document (stem or path), as `norn edit` accepts.
    pub target: String,

    /// Ordered edit ops, applied all-or-nothing. Each op is tagged by `op`.
    pub edits: Vec<EditOp>,

    /// Optional compare-and-swap precondition: the document's expected current
    /// content hash (blake3 hex of the full file — the `document_hash` plan ops
    /// carry). When present, the edit is refused if the document has drifted
    /// from it; absent = read-modify-write. Mirrors `norn edit --expected-hash`.
    #[serde(default)]
    pub expected_hash: Option<String>,

    /// Apply the edits. **Defaults to false (dry-run): returns the plan with
    /// `applied = false` and writes nothing.** Pass true to acquire the vault
    /// mutation lock and write.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.edit`.
///
/// rmcp requires a tool's advertised `outputSchema` to have a root `type:
/// object`. [`EditReport`] carries a `camino::Utf8PathBuf` target field, which
/// has no `schemars::JsonSchema` impl, so the report cannot derive `JsonSchema`
/// directly. We wrap it as a generic `serde_json::Value` inside this typed
/// envelope (the same pattern `vault.set` uses): the full report structure
/// travels faithfully in the JSON; only the inner schema is left generic.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EditOutput {
    /// The `EditReport` JSON: the planned (or applied) per-op edits, the
    /// `applied` flag, body-change sizing, and (on apply) the trace id.
    /// Byte-for-byte the same shape `norn edit --format json` emits.
    pub report: serde_json::Value,
}

impl EditOutput {
    fn from_report(report: &EditReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.edit`: run the pure handler, then
/// project the report into the typed [`EditOutput`]. The single function the
/// `#[tool]` wrapper calls.
pub fn handle_output(
    ctx: &VaultContext,
    scope: &RequestScope,
    p: EditParams,
) -> Result<MutationResult<EditOutput>> {
    let dry_run = !p.confirm;
    let target = p.target.clone();
    // Capture a coded refusal (NRN-220): the `expected_hash` CAS drift and the
    // anchor family (`anchor-not-found`, …) become a structured `refused` report
    // + `isError:true` instead of a bare MCP `Err`. Others still propagate.
    let report = match handle(ctx, scope, p) {
        Ok(report) => report,
        Err(e) => match crate::mcp::mutate::refusal_from_error(&e) {
            // Prefer the error's resolved path (present for a `stale-document-hash`
            // CAS drift) so `report.target` matches the success path; fall back to
            // the raw target for an anchor miss, which carries no path.
            Some(err) => {
                let report_target = err
                    .path
                    .clone()
                    .map(Utf8PathBuf::from)
                    .unwrap_or_else(|| Utf8PathBuf::from(target));
                EditReport::refused(report_target, err)
            }
            None => return Err(e),
        },
    };
    let outcome = report.outcome;
    Ok(MutationResult::from_outcome(
        EditOutput::from_report(&report)?,
        dry_run,
        outcome,
    ))
}

/// Pure handler for `vault.edit`.
///
/// Mirrors `norn edit`'s dispatch (see `main.rs` `Command::Edit`): load config →
/// load the graph index (honoring `files.ignore`) → open a query cache →
/// `edit::synth::preflight_and_plan` → DRY-RUN unless `confirm`. On `confirm`,
/// acquire the per-vault mutation lock and apply via the shared repair applier.
///
/// **Safety invariant:** when `!confirm`, this acquires NO lock and never calls
/// the applier — it returns `build_report(.., applied = false, ..)` and leaves
/// the file untouched.
pub fn handle(ctx: &VaultContext, scope: &RequestScope, p: EditParams) -> Result<EditReport> {
    let cwd = ctx.vault_root.clone();

    if p.edits.is_empty() {
        anyhow::bail!("edits array is empty");
    }

    // CONFIRM locks BEFORE any read that feeds the write (NRN-99); dry-run
    // never locks. See `crate::mcp::mutate::acquire_mutation_lock` for the
    // invariant.
    let _mutation_lock = if p.confirm {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    } else {
        None
    };

    // ONE query_cache call serves both needs: the graph index is built from the
    // same handle used for target resolution, so the pipeline (ground-shift,
    // freshness refresh) runs once and index + cache are one consistent snapshot.
    // Warm-connection reuse under the daemon; fresh open in cold mode (NRN-130).
    let config = scope.config();
    let cache = ctx.query_cache(scope)?;
    let index = cache.load_graph_index()?;
    let vault_cfg = &config.vault_config;

    let pre = crate::edit::synth::preflight_and_plan(
        &cwd,
        &cache,
        &index,
        vault_cfg,
        &p.target,
        &p.edits,
        p.expected_hash.as_deref(),
    )?;

    // DRY-RUN (default): no lock, no apply, no write.
    if !p.confirm {
        return Ok(crate::edit::report::build_report(
            &pre.outcome,
            &pre.descriptors,
            false,
            "",
        ));
    }

    // Open a REAL, file-backed event sink on the apply path — the same audit
    // trail `norn edit` writes. The sink also owns the trace id stamped into the
    // report.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx, scope);
    crate::emit_invocation_started(
        &mut sink,
        "edit",
        &cwd,
        pre.outcome.plan.vault_root.as_str(),
        /*dry_run=*/ false,
        &["edit".to_string(), p.target.clone()],
    );
    let spans = crate::repair_apply::build_op_spans(&mut sink, &pre.outcome.plan.changes);
    let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
        &cwd,
        &index,
        &pre.outcome.plan,
        /*dry_run=*/ false,
        &crate::repair_apply::CreateApplyContext::default(),
        &mut sink,
        &spans,
        None,
    );
    let trace_id = sink.trace_id().to_string();
    let exit = if apply_outcome.is_ok() { 0 } else { 2 };
    crate::emit_single_op_finished(&mut sink, "edit", exit, apply_outcome.is_ok());
    let apply_report = apply_outcome?;

    // Warm mode: commit the apply's cache increments (awaited) so the next read
    // stays cheap; a no-op in cold mode (NRN-252 / NRN-158).
    ctx.commit_apply_increments(scope, &apply_report.touched_paths());

    Ok(crate::edit::report::build_report(
        &pre.outcome,
        &pre.descriptors,
        true,
        &trace_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::mcp::tools::scoped_shim! {
        fn handle_output(EditParams) -> crate::mcp::mutation_result::MutationResult<EditOutput>;
    }
    use crate::edit::ops::EditOp;
    use rmcp::handler::server::tool::IntoCallToolResult;
    use tempfile::TempDir;

    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-edit-refusal-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(root.join("doc.md"), "---\ntype: note\n---\nHello world\n").unwrap();
        (tmp, root)
    }

    /// Extract the `report` object out of a `MutationResult<EditOutput>` the way an
    /// MCP client sees it (through `structuredContent`).
    fn report_of(mr: MutationResult<EditOutput>) -> serde_json::Value {
        let ctr = mr.into_call_tool_result().expect("serialize");
        ctr.structured_content
            .expect("structured content present")
            .get("report")
            .cloned()
            .expect("report present")
    }

    fn str_replace(old: &str, new: &str) -> EditOp {
        EditOp::StrReplace {
            old: old.to_string(),
            new: new.to_string(),
            replace_all: false,
        }
    }

    /// NRN-220: a CONFIRM edit whose anchor is not found is a STRUCTURED refusal —
    /// `isError:true`, `outcome:"refused"`, and a machine-branchable
    /// `error.code = anchor-not-found` — not a bare MCP `Err` with the code
    /// laundered to prose.
    #[test]
    fn confirm_anchor_not_found_is_structured_refusal() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let mr = handle_output(
            &ctx,
            EditParams {
                target: "doc".into(),
                edits: vec![str_replace("NONEXISTENT ANCHOR", "x")],
                expected_hash: None,
                confirm: true,
            },
        )
        .expect("a coded refusal returns Ok(MutationResult), not Err");

        assert!(mr.is_error(), "a confirmed refusal maps to isError:true");
        let report = report_of(mr);
        assert_eq!(report["outcome"], "refused");
        assert_eq!(report["error"]["code"], "anchor-not-found");
        assert_eq!(report["applied"], serde_json::json!(false));
        // The document on disk is untouched — the refusal wrote nothing.
        assert_eq!(
            std::fs::read_to_string(root.join("doc.md")).unwrap(),
            "---\ntype: note\n---\nHello world\n"
        );
    }

    /// NRN-220 (review fix): an `expected_hash` CAS drift refusal reports the
    /// RESOLVED vault-relative path in BOTH `report.target` and `error.path` — so
    /// the field means the same thing as on the success path, and a consumer can
    /// re-read the drifted document for a retry. The code is `stale-document-hash`.
    #[test]
    fn expected_hash_drift_reports_resolved_path() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let mr = handle_output(
            &ctx,
            EditParams {
                // stem input; resolves to `doc.md` on disk.
                target: "doc".into(),
                edits: vec![str_replace("Hello world", "Goodbye")],
                expected_hash: Some("deadbeefdeadbeefdeadbeefdeadbeef".into()),
                confirm: true,
            },
        )
        .expect("a CAS refusal returns Ok(MutationResult), not Err");

        assert!(mr.is_error());
        let report = report_of(mr);
        assert_eq!(report["outcome"], "refused");
        assert_eq!(report["error"]["code"], "stale-document-hash");
        // Resolved path in both places (was the raw stem before the review fix).
        assert_eq!(report["target"], "doc.md");
        assert_eq!(report["error"]["path"], "doc.md");
        // The Display message (what the CLI prints) still names the document.
        assert!(
            report["error"]["message"]
                .as_str()
                .unwrap()
                .contains("doc.md"),
            "drift message must name the document: {report}"
        );
    }

    /// NRN-220: a DRY-RUN edit that forecasts a refusal carries `outcome:"refused"`
    /// and `error.code` in the report, but is `isError:false` — a `confirm:false`
    /// preview must not throw in an SDK that raises on `isError`.
    #[test]
    fn dry_run_anchor_refusal_is_not_error() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let mr = handle_output(
            &ctx,
            EditParams {
                target: "doc".into(),
                edits: vec![str_replace("NONEXISTENT ANCHOR", "x")],
                expected_hash: None,
                confirm: false,
            },
        )
        .expect("a dry-run refusal returns Ok(MutationResult)");

        assert!(
            !mr.is_error(),
            "a dry-run forecasted refusal must stay isError:false"
        );
        let report = report_of(mr);
        assert_eq!(report["outcome"], "refused");
        assert_eq!(report["error"]["code"], "anchor-not-found");
    }
}
