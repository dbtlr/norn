//! The `norn repair` command.
//!
//! Runs the validate engine, filters the findings, and turns them into a
//! `MigrationPlan` via `planner::findings`. [`filtered_findings`] + [`build_plan`]
//! are the surface-neutral finding→plan orchestration (NRN-291), shared by the
//! cold `<RepairParams as Request>::execute` path (MCP handler + routed daemon)
//! and the CLI-local paths here. `norn repair --plan` routes through the generic
//! `crate::dispatch` (the `Repair` arm in `lib.rs`) and renders via [`emit_plan`];
//! bare `norn repair` stays CLI-local via [`run_summary`]. `repair_apply.rs`
//! executes the plan the apply step accepts. `render` formats the output,
//! `skip_reasons` explains unrepaired findings.

pub mod render;
pub mod skip_reasons;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::{RepairArgs, RepairPlanFormat};
use crate::config_loader::{load_config, LoadedConfig};
use crate::core::GraphIndex;
use crate::mcp::tools::repair::RepairParams;
use crate::migration_plan::MigrationPlan;
use crate::planner::findings::plan_from_findings;
use crate::repair::skip_reasons::code_matches_any;
use crate::standards::{validate_with_compiled, Finding};
use crate::validate_filter::filter_findings;

/// Shared context the dispatcher hands to `run_summary` (bare `norn repair`).
pub struct RepairRunContext<'a> {
    pub cwd: &'a Utf8PathBuf,
    pub config_path: Option<&'a Utf8PathBuf>,
    pub no_cache_refresh: bool,
    pub verbose: bool,
}

/// Filter the vault's validation findings for `params` against an already-loaded
/// `index` + `config`. The surface-neutral finding-filter orchestration
/// (NRN-291): the cold `execute()` path (MCP handler + routed daemon) and the
/// CLI-local `run_summary` path both call this, keyed on `RepairParams` — the
/// canonical request vocabulary. Index construction stays surface-specific (the
/// caller loads it: `VaultEnv::load_graph_index` on the cold path,
/// `cache::command::load_graph_index` on the CLI path), so this function does no
/// I/O and never forks on surface.
pub(crate) fn filtered_findings(
    params: &RepairParams,
    index: &GraphIndex,
    config: &LoadedConfig,
) -> Result<Vec<Finding>> {
    let findings = validate_with_compiled(
        index,
        &config.validate,
        &config.compiled,
        config.index_options.alias_field.as_deref(),
    );
    filter_findings(findings, &params.validate_filter_options())
}

/// Build the in-memory `MigrationPlan` for `params` from already-filtered
/// findings. The surface-neutral plan-construction twin of [`filtered_findings`]
/// (NRN-291): `plan_from_findings` is pure (no filesystem side effects), then
/// `--skip-reason` narrows the skipped set — the SAME sequence both the cold
/// `execute()` path and the CLI-local summary path run.
pub(crate) fn build_plan(
    params: &RepairParams,
    vault_root: Utf8PathBuf,
    findings: Vec<Finding>,
    config: &LoadedConfig,
    index: &GraphIndex,
) -> MigrationPlan {
    let mut plan = plan_from_findings(
        vault_root,
        params.plan_filters(),
        findings,
        &config.repair,
        index,
    );

    // --skip-reason narrows the skipped set only (planner does not apply it).
    // The MigrationPlan SkippedFinding `reason` carries the kebab-case reason code.
    let skip_patterns = params.skip_patterns();
    if !skip_patterns.is_empty() {
        plan.skipped
            .retain(|sf| code_matches_any(&sf.reason, &skip_patterns));
    }
    plan
}

/// Load config + the CLI-direct graph index for a `norn repair` invocation, with
/// the standard non-verbose diagnostic trim. The CLI-local index-loading seam
/// for [`run_summary`]; the cold `execute()` path loads via
/// `VaultEnv::load_graph_index` instead (NRN-291), so this stays the CLI-only
/// loader.
fn load_cli_index(ctx: &RepairRunContext<'_>) -> Result<(GraphIndex, LoadedConfig)> {
    let loaded_config = load_config(ctx.cwd, ctx.config_path)?;
    let mut index = crate::cache::command::load_graph_index(
        ctx.cwd,
        &loaded_config.index_options,
        ctx.no_cache_refresh,
    )?;
    crate::trim_diagnostics(&mut index, ctx.verbose);
    Ok((index, loaded_config))
}

/// Render `plan` to stdout / `--out`, and derive the exit code — the SHARED
/// post-plan-construction step for both `norn repair --plan` (via the generic
/// `crate::dispatch` render closure) and the CLI-local paths here (NRN-291).
/// Byte-identical by construction: both callers funnel through this one
/// function rather than duplicating the render/write logic.
///
/// `has_diagnostic_errors` is the exit-code signal (any error-severity
/// diagnostic anywhere in the vault) — the direct caller derives it from its
/// own `GraphIndex` (`crate::graph::has_errors`); the routed caller reads it
/// off the wire (`vault.repair`'s `RepairOutput::has_diagnostic_errors`).
pub(crate) fn emit_plan(
    plan: &MigrationPlan,
    args: &RepairArgs,
    cwd: &Utf8Path,
    has_diagnostic_errors: bool,
) -> Result<i32> {
    // --out: always writes JSON to the file (independent of --format). The
    // absolute-vs-join resolution is the shared `config_loader::resolve_path`
    // (widened to `&Utf8Path` so both callers — direct's owned `cwd:
    // &Utf8PathBuf` and the routed seam's `cwd: &Utf8Path` — pass through).
    if let Some(out) = &args.out {
        let out_path = crate::config_loader::resolve_path(cwd, out);
        let plan_text = serde_json::to_string_pretty(plan)?;
        fs::write(&out_path, format!("{plan_text}\n")).map_err(|error| {
            anyhow::anyhow!("failed to write migration plan {out_path}: {error}")
        })?;
    }

    // --format: governs stdout. When --out is set without --format, stdout stays silent.
    let stdout_format = if args.format.is_none() && args.out.is_some() {
        None
    } else {
        Some(args.format.unwrap_or_else(|| {
            use std::io::IsTerminal;
            if std::io::stdout().is_terminal() {
                RepairPlanFormat::Report
            } else {
                RepairPlanFormat::Json
            }
        }))
    };

    if let Some(format) = stdout_format {
        match format {
            RepairPlanFormat::Report => render::write_report(plan, args)?,
            RepairPlanFormat::Json => {
                let json = serde_json::to_string_pretty(plan)?;
                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                stdout.write_all(json.as_bytes())?;
                stdout.write_all(b"\n")?;
            }
            RepairPlanFormat::Paths => render::write_paths(plan)?,
        }
    }

    Ok(if has_diagnostic_errors { 1 } else { 0 })
}

/// `norn repair` (bare) — print a read-only findings summary. Placeholder for a
/// future interactive repair workflow.
pub fn run_summary(args: &RepairArgs, ctx: &RepairRunContext<'_>) -> Result<i32> {
    let params = RepairParams::from_args(args);
    let (index, loaded_config) = load_cli_index(ctx)?;
    let findings = filtered_findings(&params, &index, &loaded_config)?;
    let findings_len = findings.len();

    // Count by code (sorted) for a stable summary. Owned keys so `findings`
    // can move into the planner below.
    let mut by_code: BTreeMap<String, usize> = BTreeMap::new();
    for finding in &findings {
        *by_code.entry(finding.code.clone()).or_insert(0) += 1;
    }

    // Of those, how many would the planner turn into operations? Shares the ONE
    // finding→plan orchestration with the `--plan` and cold `execute()` paths.
    let plan = build_plan(&params, ctx.cwd.clone(), findings, &loaded_config, &index);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{} findings across {} documents",
        findings_len,
        index.documents.len()
    )?;
    if !by_code.is_empty() {
        for (code, count) in &by_code {
            writeln!(out, "  {count}  {code}")?;
        }
    }
    writeln!(out)?;
    writeln!(
        out,
        "{} repairable as operations, {} skipped",
        plan.operations.len(),
        plan.skipped.len()
    )?;
    if !plan.operations.is_empty() || !plan.skipped.is_empty() {
        writeln!(out)?;
        writeln!(out, "Run `norn repair --plan` to generate a MigrationPlan.")?;
        writeln!(out, "Pipe it into `norn apply -` to apply.")?;
    }

    Ok(crate::exit_code_for(&index))
}
