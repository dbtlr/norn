//! The `norn repair` command.
//!
//! Runs the validate engine, filters the findings, and turns them into a
//! `MigrationPlan` via `planner::findings`. `run_plan` emits the plan for
//! review (`--plan` mode); `run_summary` rolls it up. The `Repair` arm in
//! `lib.rs` dispatches here, and `route.rs` forwards a routable repair to the
//! daemon; `repair_apply.rs` executes the plan the apply step accepts.
//! `render` formats the output, `skip_reasons` explains unrepaired findings.

pub mod render;
pub mod route;
pub mod skip_reasons;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};

use crate::cli::{RepairArgs, RepairPlanFormat};
use crate::config_loader::load_config;
use crate::core::GraphIndex;
use crate::migration_plan::MigrationPlan;
use crate::planner::findings::plan_from_findings;
use crate::repair::skip_reasons::code_matches_any;
use crate::standards::{validate_with_compiled, ConfidenceFilter, RepairPlanFilters};
use crate::validate_filter::{filter_findings, ValidateFilterOptions};

/// Shared context the dispatcher hands to `run_plan` / `run_summary`.
pub struct RepairRunContext<'a> {
    pub cwd: &'a Utf8PathBuf,
    pub config_path: Option<&'a Utf8PathBuf>,
    pub no_cache_refresh: bool,
    pub verbose: bool,
}

/// Gather validation findings, build the GraphIndex, and apply the triage
/// filters shared by both `run_plan` and `run_summary`.
///
/// Returns `(index, findings)`.
fn gather_findings(
    args: &RepairArgs,
    ctx: &RepairRunContext<'_>,
) -> Result<(
    GraphIndex,
    Vec<crate::standards::Finding>,
    crate::config_loader::LoadedConfig,
)> {
    let loaded_config = load_config(ctx.cwd, ctx.config_path)?;
    let mut index = crate::cache::command::load_graph_index(
        ctx.cwd,
        &loaded_config.index_options,
        ctx.no_cache_refresh,
    )?;
    crate::trim_diagnostics(&mut index, ctx.verbose);
    let findings = validate_with_compiled(
        &index,
        &loaded_config.validate,
        &loaded_config.compiled,
        loaded_config.index_options.alias_field.as_deref(),
    );
    let filters = ValidateFilterOptions::from(args);
    let findings = filter_findings(findings, &filters)?;
    Ok((index, findings, loaded_config))
}

/// Translate the CLI triage + skip-reason + confidence flags into the
/// planner's `RepairPlanFilters`.
fn plan_filters(args: &RepairArgs) -> RepairPlanFilters {
    RepairPlanFilters {
        code: normalized_filter_values(&args.triage.code),
        severity: normalized_filter_values(&args.triage.severity),
        field: normalized_filter_values(&args.triage.field),
        rule: normalized_filter_values(&args.triage.rule),
        path: normalized_filter_values(&args.triage.path),
        target: normalized_filter_values(&args.triage.target),
        reason: normalized_filter_values(&args.triage.reason),
        skip_reason: normalized_filter_values(&args.skip_reason),
        confidence: args.confidence.map(|c| match c {
            crate::cli::ConfidenceArg::High => ConfidenceFilter::High,
        }),
    }
}

fn normalized_filter_values(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

/// `norn repair --plan` — generate a MigrationPlan from current findings and
/// render it as report / json / paths. Read-only.
pub fn run_plan(args: &RepairArgs, ctx: &RepairRunContext<'_>) -> Result<i32> {
    let (index, findings, loaded_config) = gather_findings(args, ctx)?;

    let mut plan = plan_from_findings(
        ctx.cwd.clone(),
        plan_filters(args),
        findings,
        &loaded_config.repair,
        &index,
    );

    // --skip-reason narrows the skipped set only (planner does not apply it).
    // The MigrationPlan SkippedFinding `reason` carries the kebab-case reason code.
    let skip_patterns = normalized_filter_values(&args.skip_reason);
    if !skip_patterns.is_empty() {
        plan.skipped
            .retain(|sf| code_matches_any(&sf.reason, &skip_patterns));
    }

    // The exit-code signal: any error-severity diagnostic anywhere in the FULL
    // index, independent of the triage filters just applied to `plan` — the
    // SAME predicate `crate::exit_code_for` derives from, so the shared
    // `emit_plan` (used by both this direct path and the routed path, NRN-231)
    // reproduces it without needing the `GraphIndex` itself.
    let has_diagnostic_errors = crate::graph::has_errors(&index);

    emit_plan(&plan, args, ctx.cwd, has_diagnostic_errors)
}

/// Render `plan` to stdout / `--out`, and derive the exit code — the SHARED
/// post-plan-construction step for both `norn repair --plan` (direct) and the
/// routed path (`vault.repair` reconstructed via `repair::route`, NRN-231).
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
    let (index, findings, loaded_config) = gather_findings(args, ctx)?;

    // Count by code (sorted) for a stable summary.
    let mut by_code: BTreeMap<&str, usize> = BTreeMap::new();
    for finding in &findings {
        *by_code.entry(finding.code.as_str()).or_insert(0) += 1;
    }

    // Of those, how many would the planner turn into operations?
    let plan = plan_from_findings(
        ctx.cwd.clone(),
        plan_filters(args),
        findings.clone(),
        &loaded_config.repair,
        &index,
    );

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{} findings across {} documents",
        findings.len(),
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
