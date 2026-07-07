//! `norn rewrite-wikilink OLD NEW` — graph-aware wikilink rewrite (body + frontmatter).
//!
//! Builds a one-op MigrationPlan with kind `rewrite_wikilink`, runs through
//! the unified applier. Pre-flight refusal (exit 2) when OLD is unresolvable.
//!
//! # Exit codes
//! - 0: success (or dry-run with no failures)
//! - 1: runtime failure (at least one op failed during apply)
//! - 2: pre-flight refusal (OLD does not resolve to any document)

use crate::applier::{apply_migration_plan, ApplyContext};
use crate::apply_report::ApplyReport;
use crate::cache::state_dir_for;
use crate::cache::CacheError;
use crate::cli::RewriteWikilinkFormat;
use crate::migration_plan::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use crate::mutation_lock::pending::sweep_pending;
use crate::mutation_lock::MutationLock;
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use std::io::IsTerminal;
use std::io::Write;

pub struct RewriteWikilinkRunArgs {
    pub old: String,
    pub new: String,
    pub dry_run: bool,
    pub yes: bool,
    pub format: RewriteWikilinkFormat,
    pub out: Option<String>,
}

/// Pre-flight / byte-identical-refusal exit code. Runtime and success exits now
/// come from [`crate::apply_report::ApplyReport::exit_code`] (1 = partial-apply
/// failure, 0 = success), the single outcome→exit mapping shared across surfaces.
pub const EXIT_PREFLIGHT: i32 = 2;

pub fn run(
    args: RewriteWikilinkRunArgs,
    cwd: &Utf8PathBuf,
    no_cache_refresh: bool,
    config_path: Option<&Utf8PathBuf>,
    verbose: bool,
) -> Result<i32> {
    // ------------------------------------------------------------------
    // 0. Acquire mutation lock before cache load.
    // ------------------------------------------------------------------
    let (_, state_dir) =
        state_dir_for(cwd).map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
    sweep_pending(&state_dir);
    let _mutation_lock = {
        let is_apply = !args.dry_run
            && (args.yes
                || matches!(args.format, RewriteWikilinkFormat::Json)
                || std::io::stdin().is_terminal());
        match MutationLock::acquire_if_mutating(&state_dir, is_apply) {
            Ok(guard) => guard,
            Err(CacheError::MutationLockTimeout) => {
                eprintln!(
                    "error: another norn mutation is in progress against this vault (timed out after 5 s)"
                );
                return Ok(EXIT_PREFLIGHT);
            }
            Err(e) => return Err(anyhow::anyhow!("mutation lock error: {e}")),
        }
    };

    // ------------------------------------------------------------------
    // 1. Build GraphIndex
    // ------------------------------------------------------------------
    let loaded_config = crate::config_loader::load_config(cwd, config_path)?;
    let index =
        crate::cache_cmd::load_graph_index(cwd, &loaded_config.index_options, no_cache_refresh)?;

    // ------------------------------------------------------------------
    // 2. Build one-op MigrationPlan
    // ------------------------------------------------------------------
    let vault_root = cwd.to_string();
    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.clone(),
        generator: None,
        generated_at: None,
        operations: vec![MigrationOp {
            kind: "rewrite_wikilink".into(),
            id: None,
            requires: vec![],
            fields: serde_json::json!({"old": args.old, "new": args.new}),
            footnote: None,
        }],
        skipped: vec![],
        plan_footnote: None,
    };

    // ------------------------------------------------------------------
    // 3. Determine dry_run mode
    //    - --dry-run: never apply
    //    - --yes or --format json: skip TTY confirmation, apply
    //    - TTY without --yes: prompt
    //    - Non-TTY without --yes: implicit dry-run
    // ------------------------------------------------------------------
    let dry_run = if args.dry_run {
        true
    } else if args.yes || matches!(args.format, RewriteWikilinkFormat::Json) {
        false
    } else if std::io::stdin().is_terminal() {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut prompt_out = std::io::stderr();
        writeln!(prompt_out)?;
        let ok = crate::prompt::confirm(
            &mut reader,
            &mut prompt_out,
            "Apply wikilink rewrite? [y/N] ",
        )?;
        if !ok {
            std::process::exit(1);
        }
        false
    } else {
        // Non-TTY, no --yes: implicit dry-run
        true
    };

    let ctx = ApplyContext {
        dry_run,
        parents: false,
        verbose,
        refuse_as_report: false,
    };

    let argv: Vec<String> = std::env::args().collect();
    let mut sink = crate::open_event_sink(
        cwd,
        dry_run,
        loaded_config.vault_config.telemetry.as_ref(),
        &argv,
    );
    crate::emit_invocation_started(
        &mut sink,
        "rewrite-wikilink",
        cwd,
        &plan.vault_root,
        dry_run,
        &argv,
    );

    // ------------------------------------------------------------------
    // 4. Apply — pre-flight refusal on Err
    // ------------------------------------------------------------------
    let report = match apply_migration_plan(&plan, &index, ctx, &mut sink) {
        Ok(r) => r,
        Err(e) => {
            // NRN-150: structured `{ code, message, path? }` envelope on stdout
            // for `--format json`; prose on stderr otherwise. Preflight refusal.
            match args.format {
                RewriteWikilinkFormat::Json => crate::render_json_error_envelope(&e)?,
                RewriteWikilinkFormat::Records => eprintln!("error: {e:#}"),
            }
            return Ok(EXIT_PREFLIGHT);
        }
    };

    // ------------------------------------------------------------------
    // 5. Exit code
    // ------------------------------------------------------------------
    // NRN-150/183: exit on the report's own outcome mapping. A partial-apply
    // failure (a write landed, then an op failed) is now returned as
    // `Ok(report)` with `outcome = failed` → exit 1, not the EXIT_PREFLIGHT (2)
    // of a byte-identical refusal (which still arrives on the `Err` arm above).
    // Success → 0.
    let exit = report.exit_code();

    crate::emit_invocation_finished(&mut sink, "rewrite-wikilink", exit, &report);

    // ------------------------------------------------------------------
    // 6. Render
    // ------------------------------------------------------------------
    render_report(&report, &args)?;

    Ok(exit)
}

/// Render the apply report to stdout, OR to a file when `--out` is set.
///
/// `--out` is mutually exclusive with stdout output: when set, the report
/// (always JSON) is written to the file and stdout is silent.
fn render_report(report: &ApplyReport, args: &RewriteWikilinkRunArgs) -> Result<()> {
    if let Some(out_path) = &args.out {
        let json = serde_json::to_string_pretty(report)?;
        std::fs::write(out_path, format!("{json}\n"))
            .with_context(|| format!("failed to write apply report to '{out_path}'"))?;
        return Ok(());
    }

    let stdout = std::io::stdout();
    let mut out_lock = stdout.lock();
    match args.format {
        RewriteWikilinkFormat::Json => {
            let json = serde_json::to_string_pretty(report)?;
            out_lock.write_all(json.as_bytes())?;
            out_lock.write_all(b"\n")?;
        }
        RewriteWikilinkFormat::Records => {
            render_records(report, &args.old, &args.new, &mut out_lock)?;
            // TTY `trace:` footer on real apply (Records only; JSON carries
            // trace_id as a field).
            if !report.dry_run {
                writeln!(out_lock, "trace: {}", report.trace_id)?;
            }
        }
    }
    Ok(())
}

/// Human-readable TTY rendering showing body/frontmatter breakdown.
fn render_records(report: &ApplyReport, old: &str, new: &str, out: &mut dyn Write) -> Result<()> {
    let body_count = report
        .operations
        .iter()
        .filter(|o| o.kind == "rewrite_link")
        .count();
    let fm_count = report
        .operations
        .iter()
        .filter(|o| o.kind == "set_frontmatter")
        .count();
    let total = report.operations.len();
    let status = if report.dry_run {
        "would rewrite"
    } else {
        "rewrote"
    };
    writeln!(
        out,
        "{} [[{}]] → [[{}]] in {} ops ({} body + {} frontmatter)",
        status, old, new, total, body_count, fm_count
    )?;
    if !report.warnings.is_empty() {
        writeln!(out, "warnings:")?;
        for w in &report.warnings {
            writeln!(out, "  {}: {}", w.code, w.message)?;
        }
    }
    Ok(())
}
