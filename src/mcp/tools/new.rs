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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::{NewArgs, NewFormat};
use crate::mcp::context::{RequestScope, VaultContext};
use crate::mcp::mutation_result::MutationResult;

/// Parameters for `vault.new`.
///
/// Three creation modes (mirrors `norn new`):
///
/// - **Mode A — explicit path:** supply `path`. The document is created at that
///   vault-relative path.
/// - **Mode B — rule-targeted:** supply `rule` (+ optional `title` + `vars`).
///   The path is derived from the named rule's `target` template. Use
///   `vault.describe` to see available rules and their required variables.
/// - **Mode C — inbox fallback:** supply neither `path` nor `rule`, but DO
///   supply `title`. The document is placed in the configured inbox folder as
///   `<inbox>/<title|slugify>.md`.
///
/// Frontmatter overrides travel via `field` (KEY=VALUE strings) and `field_json`
/// (KEY=JSON strings), feeding through the same `--field` / `--field-json` seam
/// `norn new` uses. `body` seeds the document body (takes precedence over any
/// rule body scaffold). `parents` auto-creates missing parent directories.
/// `force` allows overwriting an existing file.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct NewParams {
    /// Vault-relative path of the new document (must end in `.md` when supplied).
    /// Optional: omit when using `rule` (Mode B) or the inbox fallback (Mode C).
    /// Mutually exclusive with `rule`. Supply one or the other, not both.
    #[serde(default)]
    pub path: Option<String>,

    /// Name of a creatable rule to use for path derivation (from `vault.describe`
    /// `creatable_rules`). The concrete path is rendered from the rule's `target`
    /// template using `title` and `vars`. Mutually exclusive with `path`.
    #[serde(default)]
    pub rule: Option<String>,

    /// Document title, used to render `{{title|slugify}}` in a rule target or
    /// inbox path. Required for Mode B (rule with `{{title}}`) and Mode C (inbox).
    #[serde(default)]
    pub title: Option<String>,

    /// Template variable bag for rule-targeted creation. Pass values for every
    /// name listed in `vault.describe`'s `creatable_rules[*].required_vars`.
    /// E.g. `{"workspace": "norn"}` for a rule referencing `{{var.workspace}}`.
    #[serde(default)]
    pub vars: std::collections::BTreeMap<String, String>,

    /// Frontmatter field overrides in `KEY=VALUE` format, repeatable.
    /// Feeds through `norn new --field KEY=VALUE` (string coercion).
    #[serde(default)]
    pub field: Vec<String>,

    /// Frontmatter field overrides in `KEY=JSON` format, repeatable.
    /// Feeds through `norn new --field-json KEY=JSON` (typed JSON, no coercion).
    #[serde(default)]
    pub field_json: Vec<String>,

    /// Seed the document body with this string. When supplied alongside a rule
    /// that declares a `body` scaffold, this explicit value takes precedence
    /// (equivalent to piping stdin in the CLI).
    /// Absent = use rule body scaffold if available, else empty.
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
/// Wraps the [`NewReport`](crate::new::report::NewReport) as a generic
/// `serde_json::Value` inside this typed root struct, mirroring the
/// `SetOutput` / `GetOutput` pattern: rmcp requires a `type: object` root
/// `outputSchema`, and the inner report carries a `Utf8PathBuf`-adjacent
/// `path` field that cannot derive `JsonSchema` directly (it is serialized as
/// a plain string, but the type lives in the `new` module's domain types). The
/// full report is byte-for-byte the same shape `norn new --format json --yes`
/// emits — the `applied` flag, `frontmatter_created`, `warnings`,
/// `body_bytes`, and (on confirm) `trace_id` (NRN-230).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct NewOutput {
    /// The `NewReport`: schema_version, operation, path, applied flag,
    /// scaffolded frontmatter fields (with source provenance), body_bytes,
    /// warnings, and (on confirm) the trace_id. Byte-for-byte the same shape
    /// `norn new --format json --yes` emits.
    pub report: Value,
}

impl NewOutput {
    fn from_report(report: &crate::new::report::NewReport) -> Result<Self> {
        Ok(Self {
            report: serde_json::to_value(report)?,
        })
    }
}

/// Build the MCP output envelope for `vault.new`.
pub fn handle_output(
    ctx: &VaultContext,
    scope: &RequestScope,
    p: NewParams,
) -> Result<MutationResult<NewOutput>> {
    let dry_run = !p.confirm;
    // Capture a coded refusal (NRN-220): a recognized preflight refusal
    // (`destination-exists`, containment, …) becomes a structured `refused`
    // report + `isError:true` instead of a bare MCP `Err`. Others propagate.
    let report = match handle(ctx, scope, p) {
        Ok(report) => report,
        Err(e) => match crate::mcp::mutate::refusal_from_error(&e) {
            Some(err) => crate::new::report::NewReport::refused(err),
            None => return Err(e),
        },
    };
    let outcome = report.outcome;
    Ok(MutationResult::from_outcome(
        NewOutput::from_report(&report)?,
        dry_run,
        outcome,
    ))
}

/// Pure handler for `vault.new`.
///
/// Returns the typed `NewReport` (NRN-230) — the same data `norn new --format
/// json` renders, before rendering. The caller (`handle_output`) projects it
/// into the `{ "report": <NewReport> } ` MCP envelope via `serde_json::to_value`.
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
///   4. `build_report(plan, applied=false, trace_id="")` — no lock, no apply.
///
/// CONFIRM (`confirm`):
///   0. Acquire the mutation lock FIRST — before config/index load, preflight,
///      or `build_plan` — so a concurrent norn writer can't drift the vault in
///      the read→apply window.
///   1–3. Same steps as DRY-RUN, now lock-held.
///   4. Open real event sink via `open_mutation_event_sink`, emit lifecycle events.
///   5. Build single-change `RepairPlan`, apply via `apply_repair_plan_with_context`.
///   6. `build_report(plan, applied=true, trace_id)`.
pub fn handle(
    ctx: &VaultContext,
    scope: &RequestScope,
    p: NewParams,
) -> Result<crate::new::report::NewReport> {
    let cwd = ctx.vault_root.clone();

    // CONFIRM locks BEFORE any read that feeds the write; dry-run never locks.
    // See `crate::mcp::mutate::acquire_mutation_lock` for the invariant.
    let _mutation_lock = if p.confirm {
        Some(crate::mcp::mutate::acquire_mutation_lock(&cwd)?)
    } else {
        None
    };

    // ── Step 1: Load config + graph index ─────────────────────────────────────
    // `preflight_and_plan` does this internally; we replicate it so we can
    // short-circuit at preflight without a real sink and without touching stdin.
    // Config comes from the request scope (`scope.config()`; bound at the request
    // boundary by `begin_request` in warm mode — NRN-253) — NOT re-read from disk,
    // so the whole request sees one config generation. The graph index goes
    // through the context too: warm-connection reuse under the daemon; fresh open
    // in cold mode (NRN-130).
    let loaded_config = scope.config();
    let index = ctx.load_graph_index(scope)?;

    // ── Step 2: Three-mode path resolution via the shared `resolve_target` fn ─
    // Mirrors the CLI's `preflight_and_plan` mode resolution (NRN-56).
    // The doc_path drives the param; explicit `body` overrides the rule scaffold
    // (stdin-equivalent precedence: explicit body > rule scaffold > empty).
    let doc_path_opt = p.path.as_deref().map(camino::Utf8Path::new);
    let resolved = crate::new::resolve_target(
        &loaded_config.vault_config,
        doc_path_opt,
        p.rule.as_deref(),
        p.title.as_deref(),
        &p.vars,
    )?;
    let doc_path = resolved.path;

    // ── Step 3: Preflight ──────────────────────────────────────────────────────
    // Preserve the typed `PreflightError` (do NOT stringify it) so the MCP refusal
    // seam can downcast it to a structured `error.code` (NRN-220). thiserror gives
    // the `Into<anyhow::Error>` conversion, keeping the concrete type recoverable.
    crate::new::validate::preflight(cwd.as_str(), doc_path.as_str(), p.force, p.parents)?;

    // ── Step 4: Build the plan ─────────────────────────────────────────────────
    // Construct NewArgs inline from NewParams — the same pattern set.rs uses for
    // SetArgs. `yes` / `dry_run` / `body_from_stdin` are CLI-TTY knobs inert here.
    //
    // `as_rule: None` and `path: Some(doc_path)` are intentional: mode resolution
    // (path / rule-targeted / inbox) already happened above in `resolve_target`,
    // which produced the concrete `doc_path`. `build_plan` only consumes `field`,
    // `field_json`, `force`, and `parents` from NewArgs — it never re-reads
    // `as_rule` or re-resolves the path — so passing the already-resolved path
    // with `as_rule: None` is correct and avoids a second resolution round-trip.
    let args = NewArgs {
        path: Some(doc_path.clone()),
        as_rule: None,
        title: p.title.clone(),
        var: vec![],
        field: p.field.clone(),
        field_json: p.field_json.clone(),
        body_from_stdin: false,
        force: p.force,
        parents: p.parents,
        yes: false,
        dry_run: false,
        format: NewFormat::Json,
    };

    // Body resolution: explicit `body` param > rule body scaffold > empty.
    // This mirrors the CLI's stdin > scaffold > empty precedence.
    let body = if let Some(explicit_body) = p.body.clone() {
        explicit_body
    } else if let Some(scaffold) = resolved.body_scaffold {
        // Render the scaffold with the same Context used in path generation so
        // that `{{title}}` and `{{var.X}}` substitutions are byte-identical.
        let cfg = &loaded_config.vault_config;
        let ctx_sub = crate::standards::substitution::Context {
            now: chrono::Local::now().naive_local(),
            title: p.title.clone().unwrap_or_default(),
            path_vars: resolved.path_vars.clone(),
            date_format: cfg.templates.date_format.clone(),
            time_format: cfg.templates.time_format.clone(),
        };
        // NRN-230: preserve the typed `BodyScaffoldRenderError` (its Display
        // carries the exact "body scaffold render error: …" prose the CLI
        // prints) so `refusal_from_error` can downcast it to a structured
        // `template-render-failed` refusal.
        crate::standards::substitution::render(&scaffold, &ctx_sub)
            .map_err(crate::new::BodyScaffoldRenderError)?
    } else {
        String::new()
    };

    let body_bytes = body.len();

    // NRN-230/F4: preserve the typed `SynthError` (do NOT stringify it) so the
    // MCP refusal seam (`crate::mcp::mutate::refusal_from_error`) can downcast
    // it to a structured `error.code` — the same pattern `PreflightError`
    // already uses just above (Step 3).
    let mut plan = crate::new::synth::build_plan(
        &args,
        &doc_path,
        &resolved.path_vars,
        &loaded_config.vault_config,
        &loaded_config.compiled,
        Some(&index),
        body.clone(),
    )?;

    // NRN-230/F1: `--title` has no effect in Mode A (explicit path, no
    // `rule`) — mirrors the CLI's warning push in
    // `new::preflight_and_plan` (byte-identical `Warning::TitleIgnored`),
    // which this MCP replica previously omitted entirely: a confirmed parity
    // gap where `vault.new` returned `warnings: []` for the same Mode-A +
    // `title` input the CLI warns on.
    if p.path.is_some() && p.rule.is_none() {
        if let Some(title) = &p.title {
            plan.warnings
                .push(crate::new::synth::Warning::TitleIgnored {
                    title: title.clone(),
                });
        }
    }

    let doc_path_str = doc_path.as_str().to_owned();

    // ── DRY-RUN (default): no lock, no apply, no write ─────────────────────────
    if !p.confirm {
        // NRN-101: non-binding predicted id for an unresolved `{{seq}}` target,
        // mirroring the CLI dry-run preview.
        let predicted: Option<String> = if crate::seq_alloc::has_seq(&doc_path) {
            Some(crate::seq_alloc::predict(&cwd, &doc_path)?.to_string())
        } else {
            None
        };
        return Ok(crate::new::report::build_report(
            &plan,
            &doc_path_str,
            false,
            body_bytes,
            "",
            predicted.as_deref(),
        ));
    }

    // ── CONFIRM: the mutation lock was already acquired above, before the
    // config/index load — open the real sink and apply.

    // Open a real, file-backed event sink — the same audit trail `norn new --yes`
    // writes via `apply_and_render` → `open_event_sink`. The MCP analogue is
    // `open_mutation_event_sink`, which mirrors the real-apply branch of
    // `open_event_sink`. Best-effort: falls back to discard on error, so telemetry
    // never blocks the creation.
    let mut sink = crate::mcp::mutate::open_mutation_event_sink(ctx, scope);
    crate::emit_invocation_started(
        &mut sink,
        "new",
        &cwd,
        cwd.as_str(),
        /*dry_run=*/ false,
        &["new".to_string(), doc_path_str.clone()],
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

    // Thread `-p` / `--parents` and `files.ignore` through to the
    // `create_document` applier arm — the latter re-checks the resolved
    // `{{seq}}` path against `files.ignore` before any write (NRN-138).
    let apply_ctx = crate::repair_apply::CreateApplyContext {
        parents: p.parents,
        ignore: loaded_config.vault_config.files.ignore.clone(),
        // NRN-265: `new` passes the `{{seq}}` template UNRESOLVED — the delegate's
        // Pass-1e seq resolver is load-bearing here, so it must NOT be declared
        // pre-resolved.
        creates_preresolved: false,
    };

    // Emit one op_planned span for the single create_document change so action
    // events hang off it — mirrors `apply_and_render`'s span construction.
    let spans = crate::repair_apply::build_op_spans(&mut sink, std::slice::from_ref(&plan.change));

    let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
        &cwd,
        &index,
        &repair_plan,
        /*dry_run=*/ false,
        &apply_ctx,
        &mut sink,
        &spans,
        None,
    );

    let trace_id = sink.trace_id().to_string();
    let exit_code = if apply_outcome.is_ok() { 0 } else { 2 };
    crate::emit_single_op_finished(&mut sink, "new", exit_code, apply_outcome.is_ok());
    let apply_report = apply_outcome?;

    // Warm mode: commit the created document's cache increment (awaited) so the
    // next read stays cheap; a no-op in cold mode (NRN-252 / NRN-158).
    ctx.commit_apply_increments(scope, &apply_report.touched_paths(), index);

    // NRN-101: an incremental `{{seq}}` target is resolved at apply time inside
    // the applier (under the lock). Render + post-create validate against the
    // path actually written, not the unresolved `{{seq}}` template — the CLI
    // path does the same in `apply_and_render`.
    let effective_path = apply_report
        .created_documents
        .first()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| doc_path.clone());

    // Post-create validate: surface any missing-required-field findings as
    // warnings in the output envelope — mirrors `apply_and_render`'s
    // `post_create_validate` call.
    let post_warnings =
        crate::new::post_create_validate(&cwd, effective_path.as_str(), &plan.warnings, body_bytes)
            .unwrap_or_default();
    let mut augmented = crate::new::synth::CreateDocumentPlan {
        change: plan.change.clone(),
        warnings: plan.warnings.clone(),
        field_sources: plan.field_sources.clone(),
    };
    augmented.warnings.extend(post_warnings);

    Ok(crate::new::report::build_report(
        &augmented,
        effective_path.as_str(),
        true,
        body_bytes,
        &trace_id,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::mcp::tools::scoped_shim! {
        fn handle(NewParams) -> crate::new::report::NewReport;
        fn handle_output(NewParams) -> crate::mcp::mutation_result::MutationResult<NewOutput>;
    }
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

    const CONFIG_WITH_SEQ_RULE: &str = r#"
validate:
  rules:
    - name: task
      target: "tasks/MMR-{{seq}}.md"
      frontmatter_defaults:
        type: task
"#;

    /// NRN-101 MCP parity: `vault.new` with a `{{seq}}` rule resolves the id at
    /// apply (confirm) and predicts non-bindingly on dry-run — mirroring the CLI.
    #[test]
    fn seq_rule_resolves_on_confirm_and_predicts_on_dry_run() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-new-seq-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(&norn_dir).unwrap();
        std::fs::write(norn_dir.join("config.yaml"), CONFIG_WITH_SEQ_RULE).unwrap();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let call = |confirm: bool| -> serde_json::Value {
            let report = handle(
                &ctx,
                NewParams {
                    rule: Some("task".to_string()),
                    parents: true,
                    confirm,
                    ..Default::default()
                },
            )
            .expect("handle should succeed");
            serde_json::to_value(report).unwrap()
        };

        // Confirm → allocates MMR-1 and writes it.
        let v1 = call(true);
        assert_eq!(v1["applied"], serde_json::json!(true));
        assert_eq!(v1["path"], serde_json::json!("tasks/MMR-1.md"));
        assert!(root.join("tasks/MMR-1.md").exists());

        // Dry-run → predicts MMR-2 (non-binding), writes nothing.
        let v2 = call(false);
        assert_eq!(v2["applied"], serde_json::json!(false));
        assert_eq!(v2["path"], serde_json::json!("tasks/MMR-{{seq}}.md"));
        assert_eq!(v2["predicted_path"], serde_json::json!("tasks/MMR-2.md"));
        assert!(!root.join("tasks/MMR-2.md").exists());
    }

    /// Core mutation-safety contract: dry-run (default `confirm: false`) returns
    /// the plan with `applied = false` AND writes NO file to disk.
    #[test]
    fn dry_run_default_writes_nothing() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let new_path = "notes/new-doc.md";

        // The parent "notes/" does not exist; use parents:true so preflight passes.
        let report = handle(
            &ctx,
            NewParams {
                path: Some(new_path.to_string()),
                parents: true,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run) should succeed");

        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
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

        let report = handle(
            &ctx,
            NewParams {
                path: Some(new_path.to_string()),
                parents: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm) should succeed");

        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
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

        let report = handle(
            &ctx,
            NewParams {
                path: Some(new_path.to_string()),
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run flat) should succeed");

        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
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

        let report = handle(
            &ctx,
            NewParams {
                path: Some("another.md".to_string()),
                confirm: true,
                ..Default::default()
            },
        )
        .expect("handle (confirm) should succeed");

        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
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
                path: Some("overridden.md".to_string()),
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

    /// Seed a vault with a creatable rule for rule-targeted creation tests.
    fn vault_with_rule() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-new-rule-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(&norn_dir).unwrap();
        std::fs::write(
            norn_dir.join("config.yaml"),
            "validate:\n  rules:\n    - name: task\n      target: \"Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md\"\n      body: \"## Context\\n\"\n      frontmatter_defaults:\n        type: task\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Rule-targeted dry-run: vault.new { rule: "task", title: "Fix It",
    /// vars: {workspace: "norn"} } → applied:false, path derives to
    /// Workspaces/norn/tasks/fix-it.md, file NOT written.
    #[test]
    fn mcp_new_by_rule_dry_run_returns_derived_path() {
        let (_tmp, root) = vault_with_rule();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mut vars = std::collections::BTreeMap::new();
        vars.insert("workspace".to_string(), "norn".to_string());

        // dry-run (confirm:false default).
        let report = handle(
            &ctx,
            NewParams {
                rule: Some("task".to_string()),
                title: Some("Fix It".to_string()),
                vars,
                parents: true, // Workspaces/norn/tasks/ doesn't exist yet.
                confirm: false,
                ..Default::default()
            },
        )
        .expect("handle (dry-run by rule) should succeed");

        let v: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            v["applied"].as_bool(),
            Some(false),
            "dry-run must have applied == false"
        );
        // Path must be derived from the rule's target template.
        let path = v["path"].as_str().unwrap_or("");
        assert!(
            path.contains("Workspaces/norn/tasks/fix-it.md"),
            "path must resolve to Workspaces/norn/tasks/fix-it.md, got: {path}"
        );
        // File must NOT be on disk.
        assert!(
            !root.join("Workspaces/norn/tasks/fix-it.md").exists(),
            "dry-run must NOT create the file on disk"
        );
    }

    /// Rule-targeted confirm: after dry-run returns derived path, confirm:true
    /// must create the file at the derived path.
    ///
    /// Sequencing: send dry-run → drain response → send confirm → drain.
    /// (The in-process call lock is NOT FIFO; do not interleave concurrently.)
    #[test]
    fn mcp_new_by_rule_dry_run_then_confirm_writes_file() {
        let (_tmp, root) = vault_with_rule();

        // ── dry-run ───────────────────────────────────────────────────────────
        let ctx_dry = VaultContext::open(&root, None).expect("open ctx for dry-run");
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("workspace".to_string(), "norn".to_string());

        let report_dry = handle(
            &ctx_dry,
            NewParams {
                rule: Some("task".to_string()),
                title: Some("Fix It".to_string()),
                vars: vars.clone(),
                parents: true,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("dry-run should succeed");

        let v_dry: serde_json::Value = serde_json::to_value(&report_dry).unwrap();
        assert_eq!(v_dry["applied"].as_bool(), Some(false));
        let derived_path = v_dry["path"].as_str().expect("path in dry-run response");
        assert!(
            derived_path.contains("fix-it.md"),
            "derived path should contain fix-it.md, got: {derived_path}"
        );
        // File still doesn't exist after dry-run.
        assert!(
            !root.join(derived_path).exists(),
            "dry-run must not create the file"
        );

        // ── confirm: drain dry-run first, then send confirm ───────────────────
        let ctx_confirm = VaultContext::open(&root, None).expect("open ctx for confirm");
        let report_confirm = handle(
            &ctx_confirm,
            NewParams {
                rule: Some("task".to_string()),
                title: Some("Fix It".to_string()),
                vars,
                parents: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("confirm should succeed");

        let v_confirm: serde_json::Value = serde_json::to_value(&report_confirm).unwrap();
        assert_eq!(
            v_confirm["applied"].as_bool(),
            Some(true),
            "confirm must have applied == true"
        );

        // File must now exist at the derived path.
        let expected_path = root.join("Workspaces/norn/tasks/fix-it.md");
        assert!(
            expected_path.exists(),
            "confirm must create the file at {expected_path}"
        );

        // Frontmatter must include the rule's type: task default.
        let content = std::fs::read_to_string(&expected_path).unwrap();
        assert!(
            content.contains("type: task"),
            "frontmatter must include type: task from rule default:\n{content}"
        );
    }

    /// NRN-220: creating a document whose path already exists is a STRUCTURED
    /// refusal — `isError:true`, `outcome:"refused"`, and a machine-branchable
    /// `error.code = destination-exists` — not a bare MCP `Err` with the code
    /// laundered to prose.
    #[test]
    fn confirm_destination_exists_is_structured_refusal() {
        use rmcp::handler::server::tool::IntoCallToolResult;
        let (_tmp, root) = seeded_vault();
        // Pre-create the target so preflight refuses.
        std::fs::write(root.join("exists.md"), "---\ntype: note\n---\nbody\n").unwrap();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let mr = handle_output(
            &ctx,
            NewParams {
                path: Some("exists.md".to_string()),
                confirm: true,
                ..Default::default()
            },
        )
        .expect("a coded refusal returns Ok(MutationResult), not Err");

        assert!(
            mr.is_error(),
            "a confirmed destination-exists refusal maps to isError:true"
        );
        let ctr = mr.into_call_tool_result().expect("serialize");
        let report = ctr.structured_content.expect("structured content present")["report"].clone();
        assert_eq!(report["outcome"], "refused");
        assert_eq!(report["error"]["code"], "destination-exists");
        assert_eq!(report["applied"], serde_json::json!(false));
        assert_eq!(report["path"], "exists.md");
        assert_eq!(report["error"]["path"], "exists.md");
        // Shape parity with the success envelope + the set/edit refusal reports:
        // a generic consumer reads these always-present fields on every outcome.
        for field in ["trace_id", "frontmatter_created", "body_bytes", "warnings"] {
            assert!(
                report.get(field).is_some(),
                "refusal envelope must carry always-present field `{field}`: {report}"
            );
        }
        // The pre-existing file is untouched.
        assert_eq!(
            std::fs::read_to_string(root.join("exists.md")).unwrap(),
            "---\ntype: note\n---\nbody\n"
        );
    }

    /// F1 (NRN-230): Mode A (explicit `path`, no `rule`) + `title` must push
    /// the SAME `title-ignored` warning the CLI's `new::preflight_and_plan`
    /// pushes — mirror-asserted against the CLI's own `--format json`
    /// warnings output on the SAME vault + input (not a hardcoded literal),
    /// so the parity is explicit. This was a confirmed gap: the MCP replica
    /// previously never pushed this warning at all, so `vault.new` returned
    /// `warnings: []` where the CLI warned.
    #[test]
    fn f1_title_ignored_warning_matches_cli_parity() {
        let (_tmp, root) = seeded_vault();

        // ── CLI side: the real `new::preflight_and_plan`, JSON dry-run ───────
        let cli_args = NewArgs {
            path: Some(camino::Utf8PathBuf::from("notes/mode-a.md")),
            as_rule: None,
            title: Some("Ignored Title".to_string()),
            var: vec![],
            field: vec![],
            field_json: vec![],
            body_from_stdin: false,
            force: false,
            parents: true,
            yes: false,
            dry_run: true,
            format: NewFormat::Json,
        };
        let cli_bundle =
            crate::new::preflight_and_plan(&cli_args, &root).expect("CLI dry-run must succeed");
        let cli_json: serde_json::Value = serde_json::from_str(&cli_bundle.rendered).unwrap();
        let cli_warnings = cli_json["warnings"]
            .as_array()
            .expect("CLI warnings must be an array");

        // ── MCP side: the SAME vault, the SAME Mode-A + title input ──────────
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let mcp_report = handle(
            &ctx,
            NewParams {
                path: Some("notes/mode-a.md".to_string()),
                title: Some("Ignored Title".to_string()),
                parents: true,
                confirm: false,
                ..Default::default()
            },
        )
        .expect("MCP dry-run must succeed");
        let mcp_value: serde_json::Value = serde_json::to_value(&mcp_report).unwrap();
        let mcp_warnings = mcp_value["warnings"]
            .as_array()
            .expect("MCP warnings must be an array");

        // Parity is explicit, not hardcoded: the MCP replica's warnings array
        // must equal the CLI's, byte-for-byte.
        assert_eq!(
            mcp_warnings, cli_warnings,
            "vault.new must emit the SAME warnings the CLI emits for the same \
             Mode-A + title input"
        );
        // Guard against a vacuous pass (both sides silently emitting nothing):
        // the CLI is known to warn here (NRN-37c), so assert the warning
        // actually fired on both sides.
        assert!(
            cli_warnings.iter().any(|w| w["kind"] == "title-ignored"),
            "expected the CLI to emit a title-ignored warning, got: {cli_warnings:?}"
        );
        assert!(
            mcp_warnings.iter().any(|w| w["kind"] == "title-ignored"),
            "expected vault.new to emit a title-ignored warning, got: {mcp_warnings:?}"
        );
    }

    // ── NRN-230 (PR A): resolve/synth refusal coding ─────────────────────────
    //
    // For EVERY newly-coded semantic (F3's `NewResolveError` family, F4's
    // `SynthError` family), assert the structured refusal `vault.new` returns:
    // `isError:true`, `error.code`, and `error.message` byte-identical to the
    // CLI's `error: {Display}` prose (verified against the exact literal, since
    // the CLI and this MCP replica print/carry the SAME typed error's Display).

    /// Seed a vault with an arbitrary `.norn/config.yaml`.
    fn vault_with_config(config_yaml: &str) -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-new-resolve-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let norn_dir = root.join(".norn");
        std::fs::create_dir_all(&norn_dir).unwrap();
        std::fs::write(norn_dir.join("config.yaml"), config_yaml).unwrap();
        (tmp, root)
    }

    /// Run a `vault.new` confirm call and return `(code, message)` from the
    /// structured refusal envelope — failing the test outright if the call
    /// was NOT a coded refusal (bare `Err`, or a successful create).
    fn refusal_code_and_message(ctx: &VaultContext, p: NewParams) -> (String, String) {
        let mr =
            handle_output(ctx, p).expect("a coded refusal returns Ok(MutationResult), not Err");
        assert!(
            mr.is_error(),
            "expected a structured refusal (isError:true)"
        );
        let report = &mr.value().report;
        (
            report["error"]["code"]
                .as_str()
                .expect("error.code present")
                .to_string(),
            report["error"]["message"]
                .as_str()
                .expect("error.message present")
                .to_string(),
        )
    }

    /// F3: supplying both `path` and `rule` is `path-and-rule-conflict`.
    #[test]
    fn resolve_both_path_and_rule_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("validate: {}\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                path: Some("x.md".to_string()),
                rule: Some("whatever".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "path-and-rule-conflict");
        assert_eq!(message, "pass either a path or --as, not both");
    }

    /// F3: a `rule` name absent from `validate.rules` is `unknown-rule`.
    #[test]
    fn resolve_unknown_rule_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("validate: {}\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("bogus-rule".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "unknown-rule");
        assert_eq!(message, "unknown rule `bogus-rule`");
    }

    /// F3: a rule with no `target` (non-creatable) is `rule-not-creatable`.
    #[test]
    fn resolve_rule_not_creatable_is_coded_refusal() {
        let (_tmp, root) = vault_with_config(
            "validate:\n  rules:\n    - name: no-target-rule\n      match:\n        path: \"**/*.md\"\n      frontmatter_defaults:\n        type: note\n",
        );
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("no-target-rule".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "rule-not-creatable");
        assert_eq!(
            message,
            "rule `no-target-rule` is not creatable (no `target`)"
        );
    }

    /// F3: `generate_path`'s missing-var refusal is `missing-var`, delegated
    /// transparently through `NewResolveError::GeneratePath`.
    #[test]
    fn resolve_generate_path_missing_var_is_coded_refusal() {
        let (_tmp, root) = vault_with_rule();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("task".to_string()),
                title: Some("Fix It".to_string()),
                // No `workspace` var supplied — the rule target references
                // `{{var.workspace}}`.
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "missing-var");
        assert_eq!(
            message,
            "missing required template variable `workspace` (supply with --var workspace=...)"
        );
    }

    /// F3: `generate_path`'s missing-title refusal is `missing-title`.
    #[test]
    fn resolve_generate_path_missing_title_is_coded_refusal() {
        let (_tmp, root) = vault_with_rule();
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("workspace".to_string(), "norn".to_string());
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("task".to_string()),
                vars,
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "missing-title");
        assert_eq!(message, "this target needs a title (supply with --title)");
    }

    /// F3: `generate_path`'s template-render refusal (an unknown bare
    /// `{{placeholder}}`, distinct from `var.`/`path.`/`title`) is
    /// `template-render-failed`.
    #[test]
    fn resolve_generate_path_render_failure_is_coded_refusal() {
        let (_tmp, root) = vault_with_config(
            "validate:\n  rules:\n    - name: bad-template\n      target: \"notes/{{bogus}}.md\"\n",
        );
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("bad-template".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "template-render-failed");
        assert_eq!(message, "template error: unknown variable `bogus`");
    }

    /// NRN-230: a rule `body` scaffold that fails to render is a coded
    /// refusal with the REUSED `template-render-failed` code — a body-scaffold
    /// render failure and a path-template render failure are one semantic (a
    /// configured template failed to render); the message carries the
    /// site-distinguishing "body scaffold render error: …" prose, byte-identical
    /// to the CLI's stderr.
    #[test]
    fn body_scaffold_render_failure_is_coded_refusal() {
        let (_tmp, root) = vault_with_config(
            "validate:\n  rules:\n    - name: scaffolded\n      target: \"fixed.md\"\n      body: \"hello {{bogus}}\"\n",
        );
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("scaffolded".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "template-render-failed");
        assert_eq!(
            message,
            "body scaffold render error: unknown variable `bogus`"
        );
    }

    /// F3: a misplaced `{{seq}}` (in a directory component, not the file name)
    /// is `seq-misplaced`.
    #[test]
    fn resolve_generate_path_seq_misplaced_is_coded_refusal() {
        let (_tmp, root) = vault_with_config(
            "validate:\n  rules:\n    - name: seq-bad\n      target: \"{{seq}}/note.md\"\n",
        );
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                rule: Some("seq-bad".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "seq-misplaced");
        assert_eq!(
            message,
            "`{{seq}}` is only supported once, in the file name of a rule target"
        );
    }

    /// F3: neither `path` nor `rule`, and no `inbox.path` configured, is
    /// `no-inbox-configured`.
    #[test]
    fn resolve_no_inbox_configured_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("validate: {}\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                title: Some("Some Title".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "no-inbox-configured");
        assert_eq!(message, "no path, no --as, and no inbox configured");
    }

    /// F3: the inbox fallback with no `title` is `inbox-requires-title`.
    #[test]
    fn resolve_inbox_requires_title_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("inbox:\n  path: Inbox\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "inbox-requires-title");
        assert_eq!(message, "inbox creation requires --title");
    }

    /// F4: a malformed `--field` `KEY=VALUE` pair is `assignment-malformed`
    /// (reused from `set`'s identical semantic, NRN-235).
    #[test]
    fn synth_invalid_field_format_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("validate: {}\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                path: Some("foo.md".to_string()),
                field: vec!["no_equals_sign".to_string()],
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "assignment-malformed");
        assert_eq!(
            message,
            "invalid --field format (expected key=value): no_equals_sign"
        );
    }

    /// F4: a `--field-json` value that fails to parse as JSON is
    /// `field-json-invalid` (reused from `set`'s identical semantic). The
    /// serde_json parse-error suffix is not pinned (it is not part of norn's
    /// own stable contract), but the `new`-specific prefix is.
    #[test]
    fn synth_invalid_field_json_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("validate: {}\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                path: Some("foo.md".to_string()),
                field_json: vec!["tags={not valid json".to_string()],
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "field-json-invalid");
        assert!(
            message.starts_with("invalid --field-json tags: "),
            "got: {message}"
        );
    }

    /// F4: a value that fails schema-aware coercion to its field's declared
    /// type is `field-type-invalid` (reused from `set`'s identical semantic).
    #[test]
    fn synth_coercion_failure_is_coded_refusal() {
        let (_tmp, root) = vault_with_config(
            "validate:\n  rules:\n    - name: r\n      match:\n        path: \"**/*.md\"\n      field_types:\n        bad_date: date\n",
        );
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                path: Some("foo.md".to_string()),
                field: vec!["bad_date=notadate".to_string()],
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "field-type-invalid");
        assert_eq!(
            message,
            "schema-aware coercion failed for field `bad_date`: value 'notadate' is not a valid date (expected YYYY-MM-DD)"
        );
    }

    /// F4: a `frontmatter_defaults` value whose template references an
    /// unknown variable fails `resolve_to_fixpoint`'s substitution pass —
    /// `substitution-failed`, a genuinely new, `new`-specific semantic.
    #[test]
    fn synth_substitution_failure_is_coded_refusal() {
        let (_tmp, root) = vault_with_config(
            "validate:\n  rules:\n    - name: r\n      match:\n        path: \"**/*.md\"\n      frontmatter_defaults:\n        foo: \"{{nope}}\"\n",
        );
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let (code, message) = refusal_code_and_message(
            &ctx,
            NewParams {
                path: Some("foo.md".to_string()),
                confirm: true,
                ..Default::default()
            },
        );
        assert_eq!(code, "substitution-failed");
        assert_eq!(
            message,
            "substitution failed: substitution failed: rule pass 0: unknown variable `nope`"
        );
    }

    /// F4: a path excluded by `files.ignore` is `path-ignored` — a genuinely
    /// new, `new`-specific semantic (no `set` analog). The envelope names the
    /// offending path.
    #[test]
    fn synth_path_ignored_is_coded_refusal() {
        let (_tmp, root) = vault_with_config("files:\n  ignore:\n    - \"scratch/**\"\n");
        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let mr = handle_output(
            &ctx,
            NewParams {
                path: Some("scratch/foo.md".to_string()),
                parents: true,
                confirm: true,
                ..Default::default()
            },
        )
        .expect("a coded refusal returns Ok(MutationResult), not Err");
        assert!(mr.is_error());
        let report = &mr.value().report;
        assert_eq!(report["error"]["code"], "path-ignored");
        assert_eq!(
            report["error"]["message"],
            "cannot create scratch/foo.md: excluded by files.ignore (norn does not manage ignored paths)"
        );
        assert_eq!(report["error"]["path"], "scratch/foo.md");
    }
}
