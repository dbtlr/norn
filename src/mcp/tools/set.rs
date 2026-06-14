//! `vault.set` — schema-aware single-document frontmatter (and body) mutation.
//!
//! This is the FIRST MCP mutation tool, and it establishes the **mutation-safety
//! contract** every later mutation tool (`vault.new`, `vault.move`,
//! `vault.delete`, `vault.apply_plan`) copies:
//!
//! - **Default DRY-RUN.** A call WITHOUT `confirm: true` runs the full
//!   preflight/plan and returns the report with `applied = false`, acquiring NO
//!   mutation lock and writing NOTHING to disk.
//! - **`confirm: true` WRITES.** It acquires the per-vault mutation lock and
//!   applies the plan, returning the report with `applied = true`.
//!
//! Every MCP call is effectively non-TTY, so this mirrors `norn set`'s non-TTY
//! semantics exactly: the CLI's "non-TTY without --yes = implicit dry-run" path
//! maps onto `confirm = false`, and the CLI's `--yes` apply path maps onto
//! `confirm = true`. Same preflight (`set::synth::preflight_and_plan`), same
//! mutation lock (`mutation_lock::MutationLock`), same applier
//! (`repair_apply::apply_repair_plan_with_context`), same trace-id source (a
//! telemetry `EventSink` minted exactly as the CLI mints it) — so `vault.set`
//! and `norn set` cannot drift on resolution, schema enforcement, or apply
//! semantics.
//!
//! The one deliberate difference: the CLI reads a wholesale body replacement
//! from stdin (`--body-from-stdin`); an MCP client has no stdin, so the body
//! travels as a `body` param and the body op is synthesized via the same
//! `set::synth::synth_body_op` seam the CLI's stdin path uses.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{SetArgs, SetFormat};
use crate::mcp::context::VaultContext;
use crate::set::report::{build_report, SetReport};

/// Parameters for `vault.set`.
///
/// `set` is the frontmatter mutation map: each `field -> value` pair sets that
/// field to the given JSON value. Values travel as JSON (scalars, arrays,
/// objects, explicit null) and are fed through the CLI's `--field-json` seam, so
/// they are coerced and schema-validated exactly as `norn set --field-json`
/// does. `remove` drops keys entirely. `body`, when present, wholly replaces the
/// document body (the MCP analogue of `norn set --body-from-stdin`).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SetParams {
    /// Target document (stem or path), as `norn set` accepts.
    pub target: String,

    /// Frontmatter fields to set: `field -> JSON value`. Each value is applied
    /// verbatim (scalar, array, object, or null) and schema-validated like
    /// `norn set --field-json field=<json>`. Empty map = no frontmatter change.
    #[serde(default)]
    pub set: BTreeMap<String, serde_json::Value>,

    /// Frontmatter keys to remove entirely. Silent no-op for missing keys, like
    /// `norn set --remove key`.
    #[serde(default)]
    pub remove: Vec<String>,

    /// Wholesale body replacement. When present, the document body (everything
    /// after the frontmatter) is replaced with this string — the MCP analogue of
    /// `norn set --body-from-stdin`. Absent = body unchanged.
    #[serde(default)]
    pub body: Option<String>,

    /// Bypass schema enforcement (type validation + required-field protection),
    /// mirroring `norn set --force`.
    #[serde(default)]
    pub force: bool,

    /// Apply the mutation. **Defaults to `false` (dry-run): the call returns the
    /// planned change with `applied = false` and writes nothing.** Pass `true` to
    /// acquire the vault mutation lock and write the change to disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.set`.
///
/// rmcp requires a tool's advertised `outputSchema` to have a root `type:
/// object`. [`SetReport`] carries a `camino::Utf8PathBuf` target field, which has
/// no `schemars::JsonSchema` impl, so the report cannot derive `JsonSchema`
/// directly. We wrap it as a generic `serde_json::Value` inside this typed
/// envelope (the same pattern `vault.get` / `vault.validate` use): the full
/// report structure travels faithfully in the JSON; only the inner schema is
/// left generic.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SetOutput {
    /// The `SetReport` JSON: the planned (or applied) frontmatter changes, the
    /// `applied` flag, body-change sizing, warnings, and (on apply) the trace id.
    /// Byte-for-byte the same shape `norn set --format json` emits.
    pub report: serde_json::Value,
}

impl SetOutput {
    fn from_report(report: &SetReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.set`: run the pure handler, then
/// project the report into the typed [`SetOutput`]. The single function the
/// `#[tool]` wrapper calls.
pub fn handle_output(ctx: &VaultContext, p: SetParams) -> Result<SetOutput> {
    let report = handle(ctx, p)?;
    SetOutput::from_report(&report)
}

/// Pure handler for `vault.set`.
///
/// Mirrors `norn set`'s dispatch (see `main.rs` `Command::Set`):
/// load config → load the graph index (honoring `files.ignore`) → open a query
/// cache → `preflight_and_plan` → DRY-RUN unless `confirm`. On `confirm`, acquire
/// the per-vault mutation lock and apply via the shared repair applier.
///
/// **Safety invariant:** when `!confirm`, this acquires NO lock and never calls
/// the applier — it returns `build_report(.., applied = false, ..)` and leaves
/// the file untouched.
pub fn handle(ctx: &VaultContext, p: SetParams) -> Result<SetReport> {
    let cwd = ctx.vault_root.clone();

    // Load the graph index honoring files.ignore, exactly like the CLI set path.
    let index = crate::cache_cmd::load_graph_index(&cwd, &ctx.config.index_options, false)?;

    // Cache for target resolution (needs document query, not just the index).
    let cache = ctx.query_cache()?;

    let vault_cfg = &ctx.config.vault_config;

    // Build SetArgs inline. The MCP `set` map routes through --field-json so JSON
    // values (scalars, arrays, objects, null) are applied verbatim and
    // schema-validated like the CLI. `body` is handled below via the same
    // synth_body_op seam the CLI's --body-from-stdin path uses, so we leave
    // body_from_stdin false (an MCP server has no stdin).
    let field_json: Vec<String> = p
        .set
        .iter()
        .map(|(k, v)| Ok(format!("{k}={}", serde_json::to_string(v)?)))
        .collect::<Result<Vec<_>>>()?;

    let args = SetArgs {
        target: p.target.clone(),
        fields: Vec::new(),
        field_json,
        push: Vec::new(),
        pop: Vec::new(),
        remove: p.remove.clone(),
        body_from_stdin: false,
        force: p.force,
        // `yes` / `dry_run` are CLI-TTY knobs; the MCP contract is driven by
        // `confirm` below. preflight_and_plan does not read either field, so
        // their values here are inert.
        yes: false,
        dry_run: false,
        format: SetFormat::Json,
    };

    let mut outcome =
        crate::set::synth::preflight_and_plan(&cwd, &cache, &index, vault_cfg, &args)?;

    // Body replacement: the CLI reads this from stdin in step 8 of
    // preflight_and_plan; an MCP client has none, so we synthesize the same
    // `replace_body` op here via the identical `synth_body_op` seam and stamp it
    // exactly as preflight stamps every other change (path + doc hash +
    // change_id). This keeps `vault.set` body semantics byte-identical to
    // `norn set --body-from-stdin`.
    if let Some(new_body) = p.body.as_deref() {
        inject_body_change(&cwd, &mut outcome, new_body)?;
    }

    // DRY-RUN (default): no lock, no apply, no write.
    if !p.confirm {
        return Ok(build_report(&outcome, false, ""));
    }

    // CONFIRM: acquire the per-vault mutation lock, then apply.
    let (_, state_dir) = crate::cache::state_dir_for(&cwd)
        .map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
    crate::mutation_lock::pending::sweep_pending(&state_dir);
    let _mutation_lock = match crate::mutation_lock::MutationLock::acquire_if_mutating(
        &state_dir, /*is_apply=*/ true,
    ) {
        Ok(guard) => guard,
        Err(crate::cache::CacheError::MutationLockTimeout) => {
            anyhow::bail!(
                "another norn mutation is in progress against this vault (timed out after 5 s)"
            );
        }
        Err(e) => anyhow::bail!("mutation lock error: {e}"),
    };

    // Open a REAL, file-backed event sink on the apply path — the same audit
    // trail `norn set --yes` writes (lifecycle → op_planned → action → finished).
    // This is what makes an MCP-applied mutation "audited for free": an
    // off-filesystem client still leaves the append-only event stream a CLI
    // mutation would. Best-effort by contract (falls back to discard if the file
    // can't be opened), so telemetry never blocks the mutation. The sink also
    // owns the trace id stamped into the report.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "set",
        &cwd,
        outcome.plan.vault_root.as_str(),
        /*dry_run=*/ false,
        &["set".to_string(), p.target.clone()],
    );

    // Pre-stamp an op span per planned change, exactly as the CLI does, so the
    // applier's per-op action events thread under their `op_planned` span. The
    // applier emits the actions; passing a real sink + spans is what makes them
    // flow to disk.
    let mut spans = std::collections::HashMap::new();
    for change in &outcome.plan.changes {
        let span = sink.start_op(&change.operation, change.path.as_str(), None);
        spans.insert(change.change_id.clone(), span);
    }

    let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
        &cwd,
        &index,
        &outcome.plan,
        /*dry_run=*/ false,
        &crate::repair_apply::CreateApplyContext::default(),
        &mut sink,
        &spans,
    );
    let trace_id = sink.trace_id().to_string();
    let exit = if apply_outcome.is_ok() { 0 } else { 2 };
    crate::emit_single_op_finished(&mut sink, "set", exit, apply_outcome.is_ok());
    apply_outcome?;

    Ok(build_report(&outcome, true, &trace_id))
}

/// Synthesize and stamp a `replace_body` change for an in-memory body string,
/// mirroring steps 8–9 of `set::synth::preflight_and_plan` (which only fires for
/// the CLI's `--body-from-stdin` path). Re-reads the file to compute the current
/// body and the blake3 document hash, exactly as preflight does, so the op groups
/// with frontmatter changes on the same path and passes the applier's hash check.
fn inject_body_change(
    cwd: &camino::Utf8Path,
    outcome: &mut crate::set::synth::PreflightOutcome,
    new_body: &str,
) -> Result<()> {
    let full_path = cwd.join(&outcome.target);
    let content = std::fs::read_to_string(full_path.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to read {full_path}: {e}"))?;
    let current_body = body_after_frontmatter(&content);

    let Some(mut op) = crate::set::synth::synth_body_op(&current_body, new_body) else {
        // Identical body: no-op, leaving body_changed = false (preflight default).
        return Ok(());
    };

    // Stamp path + document_hash + change_id exactly as preflight step 9 does.
    let doc_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    op.path = outcome.target.clone();
    op.document_hash = doc_hash;
    if op.change_id.is_empty() {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(op.path.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(op.operation.as_bytes());
        hasher.update(b"\0");
        hasher.update(op.field.as_deref().unwrap_or("").as_bytes());
        let digest = hasher.finalize();
        op.change_id = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    }

    outcome.body_changed = true;
    outcome.body_bytes_old = current_body.len();
    outcome.body_bytes_new = Some(new_body.len());
    outcome.plan.changes.push(op);
    outcome.plan.summary.findings = outcome.plan.changes.len();
    outcome.plan.summary.planned_changes = outcome.plan.changes.len();
    Ok(())
}

/// Return the body portion of `content` (everything after the closing frontmatter
/// `---\n`), matching `set::synth::parse_doc`'s body slice.
fn body_after_frontmatter(content: &str) -> String {
    let mut diagnostics = Vec::new();
    let (_fm, _range, _body, body_start) =
        crate::frontmatter::extract_frontmatter(content, &mut diagnostics);
    content[body_start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Seed a temp vault with a single `task.md` carrying `status: backlog`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-set-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("task.md"),
            "---\ntype: task\nstatus: backlog\n---\nTask body\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn disk_status(root: &Utf8PathBuf) -> String {
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("status:") {
                return rest.trim().to_string();
            }
        }
        panic!("no status field in task.md:\n{content}");
    }

    /// The heart of the mutation-safety contract: a call WITHOUT confirm runs the
    /// plan, reports `applied = false`, and writes NOTHING to disk.
    #[test]
    fn dry_run_default_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut set = BTreeMap::new();
        set.insert("status".to_string(), serde_json::json!("active"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set,
                remove: Vec::new(),
                body: None,
                force: false,
                confirm: false,
            },
        )
        .expect("handle (dry-run) should succeed");

        assert!(!report.applied, "dry-run report must have applied == false");
        assert_eq!(
            disk_status(&root),
            "backlog",
            "dry-run must leave the file on disk UNCHANGED (status still backlog)"
        );
    }

    /// `confirm: true` acquires the lock, applies, reports `applied = true`, and
    /// the file on disk reflects the change.
    #[test]
    fn confirm_writes_the_change() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut set = BTreeMap::new();
        set.insert("status".to_string(), serde_json::json!("active"));

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set,
                remove: Vec::new(),
                body: None,
                force: false,
                confirm: true,
            },
        )
        .expect("handle (confirm) should succeed");

        assert!(report.applied, "confirm report must have applied == true");
        assert_eq!(
            disk_status(&root),
            "active",
            "confirm must write the change to disk (status now active)"
        );
    }

    /// Body replacement (the `--body-from-stdin` analogue) under confirm rewrites
    /// the body and preserves frontmatter.
    #[test]
    fn confirm_replaces_body() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set: BTreeMap::new(),
                remove: Vec::new(),
                body: Some("Replaced body\n".into()),
                force: false,
                confirm: true,
            },
        )
        .expect("handle (confirm body) should succeed");

        assert!(report.applied);
        assert!(report.body_changed, "body_changed should be true");
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains("Replaced body"),
            "body should be replaced on disk:\n{content}"
        );
        assert!(
            content.contains("status: backlog"),
            "frontmatter must be preserved:\n{content}"
        );
    }

    /// Dry-run body replacement reports the change but writes nothing.
    #[test]
    fn dry_run_body_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let report = handle(
            &ctx,
            SetParams {
                target: "task".into(),
                set: BTreeMap::new(),
                remove: Vec::new(),
                body: Some("Replaced body\n".into()),
                force: false,
                confirm: false,
            },
        )
        .expect("handle (dry-run body) should succeed");

        assert!(!report.applied);
        assert!(
            report.body_changed,
            "dry-run still reports the planned body change"
        );
        let content = std::fs::read_to_string(root.join("task.md")).unwrap();
        assert!(
            content.contains("Task body"),
            "dry-run must leave the original body on disk:\n{content}"
        );
        assert!(
            !content.contains("Replaced body"),
            "dry-run must NOT write the new body:\n{content}"
        );
    }
}
