//! `vault new` orchestration glue. Mirror of `crates/vault-cli/src/set/mod.rs`.

pub mod report;
pub mod synth;
pub mod validate;

use std::io::{IsTerminal, Write};

use anyhow::Result;
use camino::Utf8Path;

use crate::cli::{NewArgs, NewFormat};

// ── Public surface ─────────────────────────────────────────────────────────────

/// Holds the rendered output string and the process exit code the caller
/// should use. Mirrors the return shape that `Command::Set` uses inline.
#[derive(Debug)]
pub struct OutputBundle {
    pub rendered: String,
    pub exit_code: i32,
}

/// Orchestration entry for `vault new`.
///
/// Flow:
/// 1. Load config (`.vault/config.yaml`).
/// 2. Open cache + build `GraphIndex`.
/// 3. Run preflight checks.
/// 4. Read body from stdin if `--body-from-stdin`.
/// 5. Synthesize the plan via `synth::build_plan`.
/// 6. Decide dry-run vs. apply (respecting `--dry-run`, `--yes`, `--format json`, TTY).
/// 7. On apply, call `repair_apply::apply_repair_plan` (Phase 8 wires the
///    `create_document` arm — until then, this path will surface an "unknown
///    operation" error from the orchestrator).
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
    let index = crate::cache::load_graph_index(
        &vault_root_buf,
        &loaded_config.index_options,
        /*no_cache_refresh=*/ false,
    )?;

    // ── Step 3: Preflight ─────────────────────────────────────────────────────
    crate::new::validate::preflight(
        vault_root.as_str(),
        args.path.as_str(),
        args.force,
        args.parents,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    // ── Step 4: Body from stdin ───────────────────────────────────────────────
    let body = if args.body_from_stdin {
        let raw = std::io::read_to_string(std::io::stdin())?;
        // Trim a single trailing newline to match shell convention (echo adds one).
        raw.strip_suffix('\n').unwrap_or(&raw).to_string()
    } else {
        String::new()
    };

    // ── Step 5: Synthesize the plan ───────────────────────────────────────────
    let plan = crate::new::synth::build_plan(
        args,
        &loaded_config.vault_config,
        &loaded_config.compiled,
        Some(&index),
        body.clone(),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    let body_bytes = body.len();

    // ── Step 6: Decide dry-run vs. apply ──────────────────────────────────────
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
                crate::new::report::render_records(&plan, args.path.as_str(), applied, body_bytes)
            }
            NewFormat::Json => {
                crate::new::report::render_json(&plan, args.path.as_str(), applied, body_bytes)?
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
        return apply_and_render(args, vault_root, &index, &plan, body_bytes, render_preview);
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
        return apply_and_render(args, vault_root, &index, &plan, body_bytes, render_preview);
    }

    // Non-TTY without --yes: implicit dry-run.
    let rendered = render_preview(false)?;
    Ok(OutputBundle {
        rendered,
        exit_code: 0,
    })
}

// ── Apply path ────────────────────────────────────────────────────────────────

/// Call the apply orchestrator and render the post-apply output.
///
/// Phase 8 wires the `create_document` arm in `apply_repair_plan`. Until then,
/// this path will surface an "unknown operation" error from the orchestrator.
fn apply_and_render(
    _args: &NewArgs,
    vault_root: &Utf8Path,
    index: &vault_core::GraphIndex,
    plan: &crate::new::synth::CreateDocumentPlan,
    _body_bytes: usize,
    render_preview: impl Fn(bool) -> Result<String>,
) -> Result<OutputBundle> {
    use camino::Utf8PathBuf;

    // Build the single-change RepairPlan expected by apply_repair_plan.
    let repair_plan = vault_standards::RepairPlan {
        schema_version: vault_standards::REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: Utf8PathBuf::from(vault_root.as_str()),
        source_filters: vault_standards::RepairPlanFilters::default(),
        summary: vault_standards::RepairPlanSummary {
            findings: 1,
            planned_changes: 1,
            skipped: vault_standards::SkippedSummary::default(),
        },
        changes: vec![plan.change.clone()],
        skipped_findings: vec![],
        footnotes: vec![],
    };

    // Phase 8 wires the create_document arm here.
    let vault_root_buf = vault_root.to_owned();
    crate::repair_apply::apply_repair_plan(
        &vault_root_buf,
        index,
        &repair_plan,
        /*dry_run=*/ false,
    )?;

    let rendered = render_preview(true)?;
    Ok(OutputBundle {
        rendered,
        exit_code: 0,
    })
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
        let dir = root.join(".vault");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), yaml).unwrap();
    }

    fn args_for(path: &str) -> crate::cli::NewArgs {
        crate::cli::NewArgs {
            path: path.into(),
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
}
