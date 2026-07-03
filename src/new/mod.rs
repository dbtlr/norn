//! `norn new` orchestration glue. Mirror of `src/set/mod.rs`.

pub mod generate;
pub mod report;
pub mod synth;
pub mod validate;

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::{NewArgs, NewFormat};
use crate::standards::VaultConfig;

// ── Public surface ─────────────────────────────────────────────────────────────

/// Holds the rendered output string and the process exit code the caller
/// should use. Mirrors the return shape that `Command::Set` uses inline.
#[derive(Debug)]
pub struct OutputBundle {
    pub rendered: String,
    pub exit_code: i32,
}

/// The result of three-mode path resolution.
///
/// Carries everything the caller needs to call `synth::build_plan` and
/// resolve the body scaffold — without depending on `NewArgs` or CLI types.
#[derive(Debug)]
pub struct ResolvedTarget {
    /// Vault-relative path of the new document.
    pub path: Utf8PathBuf,
    /// Caller-supplied (and rule-derived) template variable bag.
    pub path_vars: BTreeMap<String, String>,
    /// Raw `body` scaffold template from the targeted rule, if present.
    /// Must be rendered with the substitution engine before use.
    pub body_scaffold: Option<String>,
}

/// Resolve the creation target from primitive inputs — no dependency on `NewArgs`.
///
/// Implements three-mode resolution:
///
/// - **Mode A:** `doc_path` is `Some` and `as_rule` is `None` → use the path as-is.
/// - **Mode B:** `doc_path` is `None` and `as_rule` is `Some` → look up the rule
///   by name, generate path from its `target` template.
/// - **Mode C:** both `None` → inbox fallback, requires `title` and `cfg.inbox.path`.
///
/// Returns `Err` (exit-2 class) on all refusal conditions:
/// - Both path and rule supplied.
/// - Unknown rule name.
/// - Rule has no `target` (non-creatable).
/// - `generate_path` fails: missing var, missing title.
/// - Inbox fallback: no inbox configured, or no title.
/// - Malformed var entries (caller responsibility; `BTreeMap` is pre-parsed).
pub fn resolve_target(
    cfg: &VaultConfig,
    doc_path: Option<&Utf8Path>,
    as_rule: Option<&str>,
    title: Option<&str>,
    vars: &BTreeMap<String, String>,
) -> Result<ResolvedTarget> {
    match (doc_path, as_rule) {
        (Some(_), Some(_)) => Err(anyhow::anyhow!("pass either a path or --as, not both")),
        (Some(p), None) => Ok(ResolvedTarget {
            path: p.to_owned(),
            path_vars: BTreeMap::new(),
            body_scaffold: None,
        }),
        (None, Some(name)) => {
            let rule = cfg
                .validate
                .rules
                .iter()
                .find(|r| r.name.as_deref() == Some(name))
                .ok_or_else(|| anyhow::anyhow!("unknown rule `{name}`"))?;
            let target = rule
                .target
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("rule `{name}` is not creatable (no `target`)"))?;
            let inputs = crate::new::generate::GenerateInputs { title, vars };
            let generated = crate::new::generate::generate_path(target, &inputs, cfg)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let scaffold = rule.body.clone();
            Ok(ResolvedTarget {
                path: Utf8PathBuf::from(generated),
                path_vars: vars.clone(),
                body_scaffold: scaffold,
            })
        }
        (None, None) => {
            let inbox = cfg
                .inbox
                .path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("no path, no --as, and no inbox configured"))?;
            let t = title.ok_or_else(|| anyhow::anyhow!("inbox creation requires --title"))?;
            let target = format!("{inbox}/{{{{title|slugify}}}}.md");
            let inputs = crate::new::generate::GenerateInputs {
                title: Some(t),
                vars,
            };
            let generated = crate::new::generate::generate_path(&target, &inputs, cfg)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(ResolvedTarget {
                path: Utf8PathBuf::from(generated),
                path_vars: vars.clone(),
                body_scaffold: None,
            })
        }
    }
}

/// Orchestration entry for `norn new`.
///
/// Flow:
/// 1. Load config (`.norn/config.yaml`).
/// 2. Open cache + build `GraphIndex`.
/// 3. Run preflight checks.
/// 4. Read body from stdin if `--body-from-stdin`.
/// 5. Synthesize the plan via `synth::build_plan`.
/// 6. Decide dry-run vs. apply (respecting `--dry-run`, `--yes`, `--format json`, TTY).
/// 7. On apply, call `repair_apply::apply_repair_plan_with_context` with the
///    `create_document` arm and the `-p` / `--parents` flag threaded through.
/// 8. Render output and return an `OutputBundle` with the appropriate exit code.
///
/// Exit-code mapping:
/// - 0 — success (dry-run or applied).
/// - 1 — user cancelled (TTY confirm → n/N).
/// - 2 — preflight or synth error, config-load error.
pub fn preflight_and_plan(args: &NewArgs, vault_root: &Utf8Path) -> Result<OutputBundle> {
    // ── Step 1: Load config ───────────────────────────────────────────────────
    let vault_root_buf = vault_root.to_owned();
    let loaded_config = crate::config_loader::load_config(&vault_root_buf, None)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?;

    // ── Step 2: Open cache + build GraphIndex ─────────────────────────────────
    let index = crate::cache_cmd::load_graph_index(
        &vault_root_buf,
        &loaded_config.index_options,
        /*no_cache_refresh=*/ false,
    )?;

    // ── Step 2.5: Parse --var KEY=VALUE into a BTreeMap ───────────────────────
    let vars: BTreeMap<String, String> = parse_var_args(&args.var)?;

    // ── Step 3: Three-mode path resolution ───────────────────────────────────
    //
    // Delegated to the shared `resolve_target` function so CLI and MCP stay
    // capability-isomorphic (NRN-56). The refusal conditions (unknown rule,
    // non-creatable rule, missing var, missing title, both path+rule,
    // inbox unconfigured) live entirely inside `resolve_target`.
    let cfg = &loaded_config.vault_config;
    let resolved = resolve_target(
        cfg,
        args.path.as_deref(),
        args.as_rule.as_deref(),
        args.title.as_deref(),
        &vars,
    )?;
    let doc_path = resolved.path;
    let path_vars = resolved.path_vars;
    let rule_body_scaffold = resolved.body_scaffold;

    // ── Step 4: Preflight ─────────────────────────────────────────────────────
    crate::new::validate::preflight(
        vault_root.as_str(),
        doc_path.as_str(),
        args.force,
        args.parents,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // ── Step 5: Body resolution (highest → lowest precedence) ─────────────────
    //   1. `--body-from-stdin`   — operator supplies explicit content (existing).
    //   2. Rule `body` scaffold  — targeted rule's inline template, rendered with
    //                              the same substitution context used for path
    //                              generation (title + path_vars + date/time).
    //   3. Empty string          — existing default.
    let body = if args.body_from_stdin {
        let raw = std::io::read_to_string(std::io::stdin())?;
        // Trim a single trailing newline to match shell convention (echo adds one).
        raw.strip_suffix('\n').unwrap_or(&raw).to_string()
    } else if let Some(scaffold) = rule_body_scaffold {
        // Render the scaffold with the same Context used in path generation so
        // that `{{title}}` and `{{var.X}}` substitutions are byte-identical.
        let ctx = crate::standards::substitution::Context {
            now: chrono::Local::now().naive_local(),
            title: args.title.clone().unwrap_or_default(),
            path_vars: path_vars.clone(),
            date_format: cfg.templates.date_format.clone(),
            time_format: cfg.templates.time_format.clone(),
        };
        crate::standards::substitution::render(&scaffold, &ctx)
            .map_err(|e| anyhow::anyhow!("body scaffold render error: {e}"))?
    } else {
        String::new()
    };

    // ── Step 6: Synthesize the plan ───────────────────────────────────────────
    let plan = crate::new::synth::build_plan(
        args,
        &doc_path,
        &path_vars,
        &loaded_config.vault_config,
        &loaded_config.compiled,
        Some(&index),
        body.clone(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let body_bytes = body.len();
    let doc_path_str = doc_path.as_str().to_owned();

    // ── Step 7: Decide dry-run vs. apply ──────────────────────────────────────
    //
    // Decision tree (mirrors Command::Set logic in main.rs):
    //   --dry-run           → always dry-run
    //   --yes               → apply
    //   --format json       → implicit non-interactive; dry-run unless --yes
    //   stdout is TTY       → interactive confirm
    //   non-TTY, no --yes   → implicit dry-run

    let render_preview = |applied: bool| -> Result<String> {
        Ok(match args.format {
            NewFormat::Records => {
                crate::new::report::render_records(&plan, &doc_path_str, applied, body_bytes)
            }
            NewFormat::Json => {
                crate::new::report::render_json(&plan, &doc_path_str, applied, body_bytes, "")?
            }
        })
    };

    if args.dry_run {
        // Explicit dry-run: render preview only.
        let rendered = render_preview(false)?;
        return Ok(OutputBundle {
            rendered,
            exit_code: 0,
        });
    }

    if args.yes {
        // --yes: skip confirm, go straight to apply.
        return apply_and_render(
            args,
            &doc_path,
            vault_root,
            &index,
            &plan,
            body_bytes,
            loaded_config.vault_config.telemetry.as_ref(),
        );
    }

    if matches!(args.format, NewFormat::Json) {
        // JSON format is implicitly non-interactive. Without --yes, dry-run.
        let rendered = render_preview(false)?;
        return Ok(OutputBundle {
            rendered,
            exit_code: 0,
        });
    }

    if std::io::stdout().is_terminal() {
        // TTY interactive: render preview, prompt, then apply or cancel.
        let preview = render_preview(false)?;
        // Print preview to stdout before prompting.
        print!("{preview}");
        std::io::stdout().flush()?;

        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut prompt_out = std::io::stderr();
        writeln!(prompt_out)?;
        let ok = crate::prompt::confirm(&mut reader, &mut prompt_out, "Apply? [y/N] ")?;
        if !ok {
            return Ok(OutputBundle {
                rendered: String::new(),
                exit_code: 1,
            });
        }
        return apply_and_render(
            args,
            &doc_path,
            vault_root,
            &index,
            &plan,
            body_bytes,
            loaded_config.vault_config.telemetry.as_ref(),
        );
    }

    // Non-TTY without --yes: implicit dry-run.
    let rendered = render_preview(false)?;
    Ok(OutputBundle {
        rendered,
        exit_code: 0,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse `--var KEY=VALUE` arguments into a `BTreeMap`.
/// Returns exit-2 error on malformed entries (no `=`).
fn parse_var_args(var_args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for kv in var_args {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid --var format (expected KEY=VALUE): {kv}"))?;
        if k.is_empty() {
            return Err(anyhow::anyhow!("invalid --var format (key is empty): {kv}"));
        }
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

// ── Apply path ────────────────────────────────────────────────────────────────

/// Call the apply orchestrator and render the post-apply output.
///
/// `telemetry` is the loaded vault telemetry config, used to open a real
/// (file-backed) event sink for the apply. `new` is a single `create_document`
/// op, so it emits the same lifecycle → op_planned → action → finished stream
/// as the applier-routed mutators, and the `trace_id` is threaded into the
/// rendered report.
fn apply_and_render(
    args: &NewArgs,
    doc_path: &Utf8Path,
    vault_root: &Utf8Path,
    index: &crate::core::GraphIndex,
    plan: &crate::new::synth::CreateDocumentPlan,
    body_bytes: usize,
    telemetry: Option<&crate::standards::TelemetryConfig>,
) -> Result<OutputBundle> {
    use camino::Utf8PathBuf;

    // Build the single-change RepairPlan expected by apply_repair_plan_with_context.
    let repair_plan = crate::standards::RepairPlan {
        schema_version: crate::standards::REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: Utf8PathBuf::from(vault_root.as_str()),
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

    // Thread the -p / --parents flag through to the create_document arm.
    let ctx = crate::repair_apply::CreateApplyContext {
        parents: args.parents,
    };
    let vault_root_buf = vault_root.to_owned();

    // Real apply: open a file-backed sink and emit the full event stream.
    let argv: Vec<String> = std::env::args().collect();
    let mut sink = crate::open_event_sink(vault_root, /*dry_run=*/ false, telemetry, &argv);
    crate::emit_invocation_started(
        &mut sink,
        "new",
        vault_root,
        vault_root.as_str(),
        /*dry_run=*/ false,
        &argv,
    );

    // Emit one op_planned for the single create_document change; collect its
    // span so the action event (create_document) hangs off it.
    let spans = crate::repair_apply::build_op_spans(&mut sink, std::slice::from_ref(&plan.change));

    let apply_outcome = crate::repair_apply::apply_repair_plan_with_context(
        &vault_root_buf,
        index,
        &repair_plan,
        /*dry_run=*/ false,
        &ctx,
        &mut sink,
        &spans,
    );

    let trace_id = sink.trace_id().to_string();

    // Emit the finished lifecycle event (single create op → applied=1 on success).
    let exit_code = match &apply_outcome {
        Ok(_) => 0,
        Err(_) => 2,
    };
    crate::emit_single_op_finished(&mut sink, "new", exit_code, apply_outcome.is_ok());
    let apply_report = apply_outcome?;

    // NRN-101: an incremental `{{seq}}` target is resolved to its concrete id at
    // apply time inside the applier (under the mutation lock). The synth-time
    // `doc_path` still carries the unresolved `{{seq}}`, so render + post-create
    // validate against the path the applier actually wrote.
    let effective_path = apply_report
        .created_documents
        .first()
        .map(|c| c.path.clone())
        .unwrap_or_else(|| doc_path.to_owned());

    // Task 8.3: post-create validate hook.
    // Re-validate the new doc to surface any findings as warnings in the envelope.
    // We reload the index to include the newly created file.
    let post_warnings = post_create_validate(
        vault_root,
        effective_path.as_str(),
        &plan.warnings,
        body_bytes,
    )
    .unwrap_or_default();

    // If post-validate found additional warnings, merge them into the plan's warnings
    // for rendering. We render with a modified plan that includes post_warnings.
    let mut augmented = crate::new::synth::CreateDocumentPlan {
        change: plan.change.clone(),
        warnings: plan.warnings.clone(),
        field_sources: plan.field_sources.clone(),
    };
    augmented.warnings.extend(post_warnings);
    let mut rendered = match args.format {
        crate::cli::NewFormat::Records => crate::new::report::render_records(
            &augmented,
            effective_path.as_str(),
            true,
            body_bytes,
        ),
        crate::cli::NewFormat::Json => crate::new::report::render_json(
            &augmented,
            effective_path.as_str(),
            true,
            body_bytes,
            &trace_id,
        )?,
    };

    // TTY `trace:` footer on real apply (Records format only; JSON carries
    // trace_id as a field).
    if matches!(args.format, crate::cli::NewFormat::Records) {
        rendered.push_str(&format!("trace: {trace_id}\n"));
    }

    Ok(OutputBundle {
        rendered,
        exit_code: 0,
    })
}

/// Re-validate the newly created document and return any findings as
/// `Warning` variants to surface in the output envelope.
///
/// Choice: rebuild the cache + index after apply (clean; adequate for v1 on
/// small–medium vaults). A single-doc validate path doesn't exist yet in
/// vault-standards, so the rebuild is the straightforward option. The 50ms
/// perf budget applies only to the primary query path — post-create validate
/// is a one-shot operation and is acceptable to be slightly slower.
/// Shared by the CLI `apply_and_render` path and the `vault.new` MCP handler so
/// post-create validation stays byte-identical across both surfaces.
pub(crate) fn post_create_validate(
    vault_root: &Utf8Path,
    doc_path: &str,
    existing_warnings: &[crate::new::synth::Warning],
    _body_bytes: usize,
) -> Result<Vec<crate::new::synth::Warning>> {
    use crate::new::synth::Warning;

    // Quick rebuild of the index to include the newly created file.
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

    // Filter to only findings for the newly created document.
    let new_path = doc_path;
    let relevant: Vec<_> = findings
        .iter()
        .filter(|f| f.path.as_str() == new_path)
        .collect();

    // Collect field names already warned by the synth phase (MissingRequiredField).
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
            // Deduplicate with synth-phase warnings.
            if !already_warned.contains(field) {
                extra.push(Warning::MissingRequiredField {
                    field: field.clone(),
                    rules: rule.as_ref().map(|r| vec![r.clone()]).unwrap_or_default(),
                });
            }
        }
        // Other finding codes are not yet mapped to Warning variants (v1).
    }

    Ok(extra)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::Builder;

    fn vault() -> tempfile::TempDir {
        Builder::new().prefix("vault-new-mod-").tempdir().unwrap()
    }

    fn write_config(root: &std::path::Path, yaml: &str) {
        let dir = root.join(".norn");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), yaml).unwrap();
    }

    fn args_for(path: &str) -> crate::cli::NewArgs {
        crate::cli::NewArgs {
            path: Some(path.into()),
            as_rule: None,
            title: None,
            var: vec![],
            field: vec![],
            field_json: vec![],
            body_from_stdin: false,
            force: false,
            parents: false,
            yes: false,
            dry_run: true, // tests default to dry-run to avoid TTY/apply
            format: crate::cli::NewFormat::Records,
        }
    }

    #[test]
    fn preflight_and_plan_dry_run_happy_path() {
        let root = vault();
        write_config(
            root.path(),
            r#"
validate:
  rules:
    - name: any
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#,
        );
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let args = args_for("foo.md");
        let bundle = preflight_and_plan(&args, cwd).unwrap();
        assert_eq!(bundle.exit_code, 0);
        assert!(
            bundle.rendered.contains("foo.md") || bundle.rendered.contains("new"),
            "rendered: {}",
            bundle.rendered
        );
    }

    #[test]
    fn preflight_and_plan_refuses_existing_path() {
        let root = vault();
        write_config(root.path(), "validate: {}\n");
        std::fs::write(root.path().join("foo.md"), "existing").unwrap();
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let mut args = args_for("foo.md");
        args.dry_run = true;
        let err = preflight_and_plan(&args, cwd).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exists") || msg.contains("DestinationExists"),
            "error: {msg}"
        );
    }

    #[test]
    fn preflight_and_plan_refuses_missing_parent_without_parents() {
        let root = vault();
        write_config(root.path(), "validate: {}\n");
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let mut args = args_for("deep/nested/foo.md");
        args.dry_run = true;
        let err = preflight_and_plan(&args, cwd).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parent") || msg.contains("ParentMissing"),
            "error: {msg}"
        );
    }

    #[test]
    fn preflight_and_plan_json_format_emits_envelope() {
        let root = vault();
        write_config(root.path(), "validate: {}\n");
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let mut args = args_for("foo.md");
        args.dry_run = true;
        args.format = crate::cli::NewFormat::Json;
        let bundle = preflight_and_plan(&args, cwd).unwrap();
        let v: serde_json::Value = serde_json::from_str(&bundle.rendered).unwrap();
        assert_eq!(v["operation"], serde_json::json!("new"));
        assert_eq!(v["applied"], serde_json::json!(false));
    }

    // ── Apply path tests ───────────────────────────────────────────────────────

    #[test]
    fn apply_path_creates_file_and_emits_applied_true() {
        let root = vault();
        write_config(
            root.path(),
            r#"
validate:
  rules:
    - name: any
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#,
        );
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let mut args = args_for("foo.md");
        args.dry_run = false;
        args.yes = true;
        args.format = crate::cli::NewFormat::Json;
        let bundle = preflight_and_plan(&args, cwd).unwrap();
        assert_eq!(bundle.exit_code, 0);
        let v: serde_json::Value = serde_json::from_str(&bundle.rendered).unwrap();
        assert_eq!(v["applied"], serde_json::json!(true));
        assert!(
            root.path().join("foo.md").exists(),
            "foo.md should have been created"
        );
    }

    #[test]
    fn apply_path_with_parents_flag_creates_nested_dirs() {
        let root = vault();
        write_config(root.path(), "validate: {}\n");
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let mut args = args_for("deep/nested/dir/bar.md");
        args.dry_run = false;
        args.yes = true;
        args.parents = true;
        args.format = crate::cli::NewFormat::Json;
        let bundle = preflight_and_plan(&args, cwd).unwrap();
        assert_eq!(bundle.exit_code, 0);
        assert!(
            root.path().join("deep/nested/dir/bar.md").exists(),
            "nested file should have been created"
        );
    }

    // ── Task 8.3: Post-create validate hook ────────────────────────────────────

    #[test]
    fn post_create_validate_surfaces_missing_required_field() {
        let root = vault();
        // Rule requires both `type` and `description`, but only provides default for `type`.
        write_config(
            root.path(),
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      required_frontmatter: [type, description]
      frontmatter_defaults:
        type: note
"#,
        );
        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        let mut args = args_for("foo.md");
        args.dry_run = false;
        args.yes = true;
        args.format = crate::cli::NewFormat::Json;
        let bundle = preflight_and_plan(&args, cwd).unwrap();
        assert_eq!(bundle.exit_code, 0);
        let v: serde_json::Value = serde_json::from_str(&bundle.rendered).unwrap();
        assert_eq!(v["applied"], serde_json::json!(true));

        // The warnings array should include missing-required-field for `description`.
        let warnings = v["warnings"].as_array().unwrap();
        let has_missing_desc = warnings
            .iter()
            .any(|w| w["kind"] == "missing-required-field" && w["field"] == "description");
        assert!(
            has_missing_desc,
            "expected missing-required-field for description in warnings: {warnings:?}"
        );
    }

    // ── Task 8.4: Stem-collision warning end-to-end ────────────────────────────

    #[test]
    fn stem_collision_warning_surfaces_in_envelope() {
        let root = vault();
        write_config(root.path(), "validate: {}\n");
        // Pre-create a file with the same stem in a different directory.
        std::fs::create_dir_all(root.path().join("notes")).unwrap();
        std::fs::write(root.path().join("notes/foo.md"), "---\ntype: note\n---\n").unwrap();

        let cwd = camino::Utf8Path::from_path(root.path()).unwrap();
        // Now create other-dir/foo.md — same stem "foo", different path.
        let mut args = args_for("other-dir/foo.md");
        args.dry_run = true; // dry-run is enough; stem-collision warning comes from synth
        args.format = crate::cli::NewFormat::Json;
        // Need other-dir to exist for the preflight to pass without -p.
        std::fs::create_dir_all(root.path().join("other-dir")).unwrap();
        let bundle = preflight_and_plan(&args, cwd).unwrap();
        assert_eq!(bundle.exit_code, 0);
        let v: serde_json::Value = serde_json::from_str(&bundle.rendered).unwrap();

        let warnings = v["warnings"].as_array().unwrap();
        let stem_warn = warnings.iter().find(|w| w["kind"] == "stem-collision");
        assert!(
            stem_warn.is_some(),
            "expected stem-collision warning in envelope, warnings: {warnings:?}"
        );
        let sw = stem_warn.unwrap();
        assert_eq!(sw["stem"], serde_json::json!("foo"));
        let locs = sw["locations"].as_array().unwrap();
        assert!(
            locs.iter()
                .any(|l| l.as_str().unwrap_or("").contains("notes/foo.md")),
            "expected notes/foo.md in collision locations: {locs:?}"
        );
    }
}
