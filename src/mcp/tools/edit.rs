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
use crate::mcp::context::VaultContext;
use anyhow::Result;
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
pub fn handle_output(ctx: &VaultContext, p: EditParams) -> Result<EditOutput> {
    let report = handle(ctx, p)?;
    EditOutput::from_report(&report)
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
pub fn handle(ctx: &VaultContext, p: EditParams) -> Result<EditReport> {
    let cwd = ctx.vault_root.clone();

    if p.edits.is_empty() {
        anyhow::bail!("edits array is empty");
    }

    // CONFIRM acquires the per-vault mutation lock BEFORE the preflight read —
    // matching `norn edit` (main.rs), which locks before `preflight_and_plan`.
    // The lock must span the body read + transform + apply so a concurrent norn
    // writer can't drift the file in the read→apply window and slip past both
    // the `expected_hash` CAS and the applier's index-snapshot hash check. The
    // DRY-RUN path is read-only and takes NO lock.
    let _mutation_lock = if p.confirm {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    } else {
        None
    };

    let index = crate::cache_cmd::load_graph_index(&cwd, &ctx.config.index_options, false)?;
    let cache = ctx.query_cache()?;
    let vault_cfg = &ctx.config.vault_config;

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
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
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
    );
    let trace_id = sink.trace_id().to_string();
    let exit = if apply_outcome.is_ok() { 0 } else { 2 };
    crate::emit_single_op_finished(&mut sink, "edit", exit, apply_outcome.is_ok());
    apply_outcome?;

    Ok(crate::edit::report::build_report(
        &pre.outcome,
        &pre.descriptors,
        true,
        &trace_id,
    ))
}
