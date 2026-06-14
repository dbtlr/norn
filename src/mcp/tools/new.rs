//! `vault.new` — create a new document with schema-scaffolded frontmatter.
//!
//! Copies the mutation-safety contract established by `vault.set` (Task 9):
//!
//! - **Default DRY-RUN.** A call WITHOUT `confirm: true` runs preflight + plan
//!   synthesis, returns the create plan with `applied = false`, acquires NO
//!   mutation lock, and writes NOTHING to disk.
//! - **`confirm: true` WRITES.** Acquires the per-vault mutation lock, applies the
//!   single `create_document` op via `repair_apply::apply_repair_plan_with_context`,
//!   and returns the report with `applied = true`.
//!
//! ## How it mirrors the CLI `norn new` (non-TTY path)
//!
//! The CLI's `Command::New` dispatch in `main.rs`:
//! 1. Acquires the mutation lock up front (non-reentrant; `is_apply` drives
//!    whether it blocks).
//! 2. Delegates to `crate::new::preflight_and_plan(args, cwd)`, which loads
//!    config + graph index, runs preflight, synthesizes the plan, and (if
//!    `args.yes`) calls `apply_and_render`.
//!
//! The MCP handler **cannot** call `preflight_and_plan` wholesale because that
//! function reads stdin (for `--body-from-stdin`) and writes to stdout. Instead
//! it replicates the non-TTY logic inline, using the same individual pieces:
//! - `crate::new::validate::preflight` for the path checks.
//! - `crate::new::synth::build_plan` to synthesize the `CreateDocumentPlan`.
//! - `crate::repair_apply::apply_repair_plan_with_context` with a single-change
//!   `RepairPlan` (the same `apply_and_render` path in the CLI new module).
//! - `crate::mcp::mutate::open_mutation_event_sink` on the confirm path, then
//!   `emit_invocation_started` + op span + `emit_single_op_finished` — mirroring
//!   the event stream the CLI's `apply_and_render` writes via `open_event_sink`.
//!
//! ## Audit (confirmed: CLI new DOES write events)
//!
//! The CLI's `apply_and_render` (see `src/new/mod.rs`) opens a real event sink via
//! `crate::open_event_sink`, emits `invocation_started`, one `op_planned` span per
//! change, lets the applier emit the `action` event, then emits `invocation_finished`
//! via `emit_single_op_finished`. We replicate the same sequence here using
//! `open_mutation_event_sink` (the MCP analogue of `open_event_sink`'s real-apply
//! branch). The dry-run path stays silent (no sink opened, no events written),
//! matching the CLI's non-`--yes` branch.

use anyhow::Result;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{NewArgs, NewFormat};
use crate::mcp::context::VaultContext;

/// Parameters for `vault.new`.
///
/// `path` is the vault-relative path of the new document (must end in `.md`).
/// Frontmatter overrides travel via `field` (KEY=VALUE strings) and `field_json`
/// (KEY=JSON strings), feeding through the same `--field` / `--field-json` seam
/// `norn new` uses. `body` seeds the document body. `parents` auto-creates missing
/// parent directories. `force` allows overwriting an existing file.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct NewParams {
    /// Vault-relative path of the new document (must end in `.md`).
    pub path: String,

    /// Frontmatter field overrides in `KEY=VALUE` format, repeatable.
    /// Feeds through `norn new --field KEY=VALUE` (string coercion).
    #[serde(default)]
    pub field: Vec<String>,

    /// Frontmatter field overrides in `KEY=JSON` format, repeatable.
    /// Feeds through `norn new --field-json KEY=JSON` (typed JSON, no coercion).
    #[serde(default)]
    pub field_json: Vec<String>,

    /// Seed the document body with this string. Absent = empty body.
    #[serde(default)]
    pub body: Option<String>,

    /// Auto-create missing parent directories (mkdir -p style), mirroring
    /// `norn new -p` / `--parents`.
    #[serde(default)]
    pub parents: bool,

    /// Overwrite an existing document and skip schema-aware coercion, mirroring
    /// `norn new --force`.
    #[serde(default)]
    pub force: bool,

    /// Apply the creation. **Defaults to `false` (dry-run): the call returns the
    /// planned creation with `applied = false` and writes nothing.** Pass `true` to
    /// acquire the vault mutation lock and create the file on disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.new`.
///
/// Wraps the `render_json` envelope as a generic `serde_json::Value` inside this
/// typed root struct, mirroring the `SetOutput` / `GetOutput` pattern: rmcp
/// requires a `type: object` root `outputSchema`, and the inner envelope carries
/// a `Utf8PathBuf`-adjacent `path` field that cannot derive `JsonSchema` directly
/// (it is serialized as a plain string, but the type lives in the `new` module's
/// domain types). The full envelope is byte-identical to what `norn new --format
/// json --yes` emits — the `applied` flag, `frontmatter_created`, `warnings`,
/// `body_bytes`, and (on confirm) `trace_id`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct NewOutput {
    /// The `norn new` JSON envelope: operation, path, applied flag, scaffolded
    /// frontmatter fields (with source provenance), body_bytes, warnings, and
    /// (on confirm) the trace_id. Byte-for-byte the same shape `norn new
    /// --format json --yes` emits.
    pub report: Value,
}

impl NewOutput {
    fn from_json(json: String) -> Result<Self> {
        Ok(Self {
            report: serde_json::from_str(&json)?,
        })
    }
}

/// Build the MCP output envelope for `vault.new`.
pub fn handle_output(ctx: &VaultContext, p: NewParams) -> Result<NewOutput> {
    let json = handle(ctx, p)?;
    NewOutput::from_json(json)
}

/// Pure handler for `vault.new`.
///
/// Returns the `render_json` envelope string (same as `norn new --format json`).
///
/// ## Non-TTY path mirrored
///
/// MCP is always non-TTY and non-stdin. We replicate the non-TTY logic from
/// `preflight_and_plan` / `apply_and_render` inline:
///
/// DRY-RUN (`!confirm`):
///   1. Load config + index.
///   2. Preflight (path checks).
///   3. `build_plan` → `CreateDocumentPlan`.
///   4. `render_json(plan, applied=false, trace_id="")` — no lock, no apply.
///
/// CONFIRM (`confirm`):
///   1–3. Same.
///   4. Acquire mutation lock.
///   5. Open real event sink via `open_mutation_event_sink`, emit lifecycle events.
///   6. Build single-change `RepairPlan`, apply via `apply_repair_plan_with_context`.
///   7. `render_json(plan, applied=true, trace_id)`.
pub fn handle(ctx: &VaultContext, p: NewParams) -> Result<String> {
    let cwd = ctx.vault_root.clone();

    // ── Step 1: Load config + graph index ─────────────────────────────────────
    // `preflight_and_plan` does this internally; we replicate it so we can
    // short-circuit at preflight without a real sink and without touching stdin.
    let loaded_config = crate::config_loader::load_config(&cwd, None)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?;

    let index = crate::cache_cmd::load_graph_index(&cwd, &loaded_config.index_options, false)?;

    // ── Step 2: Preflight ──────────────────────────────────────────────────────
    crate::new::validate::preflight(cwd.as_str(), &p.path, p.force, p.parents)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // ── Step 3: Build the plan ─────────────────────────────────────────────────
    // Construct NewArgs inline from NewParams — the same pattern set.rs uses for
    // SetArgs. `yes` / `dry_run` / `body_from_stdin` are CLI-TTY knobs inert here.
    let args = NewArgs {
        path: Utf8PathBuf::from(&p.path),
        field: p.field.clone(),
        field_json: p.field_json.clone(),
        body_from_stdin: false,
        force: p.force,
        parents: p.parents,
        yes: false,
        dry_run: false,
        format: NewFormat::Json,
    };

    let body = p.body.clone().unwrap_or_default();
    let body_bytes = body.len();

    let plan = crate::new::synth::build_plan(
        &args,
        &loaded_config.vault_config,
        &loaded_config.compiled,
        Some(&index),
        body.clone(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // ── DRY-RUN (default): no lock, no apply, no write ─────────────────────────
    if !p.confirm {
        return crate::new::report::render_json(&plan, &p.path, false, body_bytes, "")
            .map_err(|e| anyhow::anyhow!("render_json: {e}"));
    }

    // ── CONFIRM: acquire mutation lock, open sink, apply ───────────────────────

    // Acquire mutation lock — same pattern as `Command::New` in main.rs.
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

    // Open a real, file-backed event sink — the same audit trail `norn new --yes`
    // writes via `apply_and_render` → `open_event_sink`. The MCP analogue is
    // `open_mutation_event_sink`, which mirrors the real-apply branch of
    // `open_event_sink`. Best-effort: falls back to discard on error, so telemetry
    // never blocks the creation.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx);
    crate::emit_invocation_started(
        &mut sink,
        "new",
        &cwd,
        cwd.as_str(),
        /*dry_run=*/ false,
        &["new".to_string(), p.path.clone()],
    );

    // Build the single-change RepairPlan the applier expects — mirrors
    // `apply_and_render` in `src/new/mod.rs`.
    let repair_plan = crate::standards::RepairPlan {
        schema_version: crate::standards::REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: cwd.clone(),
        source_filters: crate::standards::RepairPlanFilters::default(),
        summary: crate::standards::RepairPlanSummary {
            findings: 1,
            planned_changes: 1,
            skipped: crate::standards::SkippedSummary::default(),
        },
        changes: vec![plan.change.clone()],
        skipped_findings: vec![],
        footnotes: vec![],
    };

    // Thread `-p` / `--parents` through to the `create_document` applier arm.
    let apply_ctx = crate::repair_apply::CreateApplyContext { parents: p.parents };

    // Emit one op_planned span per change so action events hang off it — mirrors
    // `apply_and_render`'s span construction loop.
    let mut spans = std::collections::HashMap::new();
    {
        let change = &plan.change;
        let span = sink.start_op(&change.operation, change.path.as_str(), None);
        spans.insert(change.change_id.clone(), span);
    }

    let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
        &cwd,
        &index,
        &repair_plan,
        /*dry_run=*/ false,
        &apply_ctx,
        &mut sink,
        &spans,
    );

    let trace_id = sink.trace_id().to_string();
    let exit_code = if apply_outcome.is_ok() { 0 } else { 2 };
    crate::emit_single_op_finished(&mut sink, "new", exit_code, apply_outcome.is_ok());
    apply_outcome?;

    // Post-create validate: surface any missing-required-field findings as
    // warnings in the output envelope — mirrors `apply_and_render`'s
    // `post_create_validate` call.
    let post_warnings =
        post_create_validate(&cwd, &args, &plan.warnings, body_bytes).unwrap_or_default();
    let mut augmented = crate::new::synth::CreateDocumentPlan {
        change: plan.change.clone(),
        warnings: plan.warnings.clone(),
        field_sources: plan.field_sources.clone(),
    };
    augmented.warnings.extend(post_warnings);

    crate::new::report::render_json(&augmented, &p.path, true, body_bytes, &trace_id)
        .map_err(|e| anyhow::anyhow!("render_json: {e}"))
}

/// Re-validate the newly created document and return any missing-required-field
/// findings as additional warnings. Mirrors `src/new/mod.rs::post_create_validate`.
fn post_create_validate(
    vault_root: &camino::Utf8Path,
    args: &NewArgs,
    existing_warnings: &[crate::new::synth::Warning],
    _body_bytes: usize,
) -> Result<Vec<crate::new::synth::Warning>> {
    use crate::new::synth::Warning;

    let vault_root_buf = vault_root.to_owned();
    let loaded = crate::config_loader::load_config(&vault_root_buf, None)
        .map_err(|e| anyhow::anyhow!("post-create validate: config error: {e}"))?;
    let index = crate::cache_cmd::load_graph_index(
        &vault_root_buf,
        &loaded.index_options,
        /*no_cache_refresh=*/ false,
    )?;

    let findings = crate::standards::validate_with_compiled(
        &index,
        &loaded.vault_config.validate,
        &loaded.compiled,
        None,
    );

    let new_path = args.path.as_str();
    let relevant: Vec<_> = findings
        .iter()
        .filter(|f| f.path.as_str() == new_path)
        .collect();

    let already_warned: std::collections::BTreeSet<String> = existing_warnings
        .iter()
        .filter_map(|w| match w {
            Warning::MissingRequiredField { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect();

    let mut extra = Vec::new();
    for f in relevant {
        if let crate::standards::FindingBody::RequiredFrontmatterMissing { field, rule } = &f.body {
            if !already_warned.contains(field) {
                extra.push(Warning::MissingRequiredField {
                    field: field.clone(),
                    rules: rule.as_ref().map(|r| vec![r.clone()]).unwrap_or_default(),
                });
            }
        }
    }

    Ok(extra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const CONFIG_WITH_SCHEMA: &str = r#"
validate:
  rules:
    - name: note-rule
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#;

    /// Seed a temp vault with a schema config that scaffolds `type: note`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-new-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(&norn_dir).unwrap();
        std::fs::write(norn_dir.join("config.yaml"), CONFIG_WITH_SCHEMA).unwrap();
        (tmp, root)
    }

    /// Core mutation-safety contract: dry-run (default `confirm: false`) returns
    /// the plan with `applied = false` AND writes NO file to disk.
    #[test]
    fn dry_run_default_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let new_path = "notes/new-doc.md";

        // The parent "notes/" does not exist; use parents:true so preflight passes.
        let json = handle(
            &ctx,
            NewParams {
                path: new_path.to_string(),
                parents: true,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run) should succeed");

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["applied"].as_bool(),
            Some(false),
            "dry-run report must have applied == false"
        );
        assert_eq!(v["operation"], serde_json::json!("new"));

        // CRITICAL: file must NOT exist on disk after dry-run.
        assert!(
            !root.join(new_path).exists(),
            "dry-run must NOT create the file on disk"
        );
    }

    /// `confirm: true` acquires the lock, creates the file, reports `applied = true`,
    /// and the file exists on disk with schema-scaffolded frontmatter.
    #[test]
    fn confirm_creates_the_file() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let new_path = "notes/new-doc.md";

        let json = handle(
            &ctx,
            NewParams {
                path: new_path.to_string(),
                parents: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm) should succeed");

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            v["applied"].as_bool(),
            Some(true),
            "confirm report must have applied == true"
        );

        // File must exist on disk.
        let disk_path = root.join(new_path);
        assert!(
            disk_path.exists(),
            "confirm must create the file on disk at {disk_path}"
        );

        // Schema default `type: note` must be in frontmatter.
        let content = std::fs::read_to_string(&disk_path).unwrap();
        assert!(
            content.contains("type: note"),
            "schema-scaffolded frontmatter must include `type: note`:\n{content}"
        );
    }

    /// Dry-run on a flat path (no parents needed) also writes nothing.
    #[test]
    fn dry_run_flat_path_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let new_path = "flat-doc.md";

        let json = handle(
            &ctx,
            NewParams {
                path: new_path.to_string(),
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run flat) should succeed");

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["applied"].as_bool(), Some(false));
        assert!(
            !root.join(new_path).exists(),
            "dry-run must NOT create the file on disk"
        );
    }

    /// `render_json` envelope carries `frontmatter_created` with source provenance
    /// on the confirm path (schema-default for `type`).
    #[test]
    fn confirm_report_has_frontmatter_created() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let json = handle(
            &ctx,
            NewParams {
                path: "another.md".to_string(),
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm) should succeed");

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let fc = v["frontmatter_created"]
            .as_array()
            .expect("frontmatter_created must be array");
        let type_entry = fc
            .iter()
            .find(|e| e["field"] == "type")
            .expect("type field must be present");
        assert_eq!(type_entry["value"], serde_json::json!("note"));
        assert_eq!(type_entry["source"], serde_json::json!("schema-default"));
    }

    /// Field override via `field` param (string KEY=VALUE) wires through to the plan.
    #[test]
    fn field_override_is_applied() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        handle(
            &ctx,
            NewParams {
                path: "overridden.md".to_string(),
                field: vec!["title=My Override".to_string()],
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm+field) should succeed");

        let content = std::fs::read_to_string(root.join("overridden.md")).unwrap();
        assert!(
            content.contains("title: My Override"),
            "field override must appear in frontmatter:\n{content}"
        );
    }
}
