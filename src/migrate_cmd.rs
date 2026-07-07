//! `norn migrate <plan>` — apply a MigrationPlan from a JSON or YAML file.
//!
//! # Format detection
//! - `.yaml` / `.yml` extensions → YAML
//! - Any other extension, or no extension → JSON
//! - stdin (`-`) → JSON unless `--input-format yaml` is given
//!
//! # Exit codes
//! - 0: success (or dry-run with no failures)
//! - 1: runtime failure (at least one op failed during apply)
//! - 2: pre-flight refusal (parse error, schema-version mismatch, expansion error)

use crate::applier::{apply_migration_plan, ApplyContext};
use crate::apply_report::ApplyReport;
use crate::cache::state_dir_for;
use crate::cli::{InputFormat, MigrateFormat};
use crate::migration_plan::{MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use crate::mutation_lock::pending::{delete_pending_plan, save_pending_plan, sweep_pending};
use crate::mutation_lock::MutationLock;
use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use std::io::{self, Read, Write};

pub struct MigrateRunArgs {
    pub plan_path: String,
    pub dry_run: bool,
    pub yes: bool,
    pub format: MigrateFormat,
    pub input_format: Option<InputFormat>,
    /// Auto-create missing parent directories for create_document ops that
    /// proceed (mkdir -p style). Threaded into `ApplyContext.parents`.
    pub parents: bool,
    pub out: Option<String>,
}

/// Pre-flight / byte-identical-refusal exit code. Runtime and success exits now
/// come from [`crate::apply_report::ApplyReport::exit_code`] (1 = partial-apply
/// failure, 0 = success), the single outcome→exit mapping shared across surfaces.
pub const EXIT_PREFLIGHT: i32 = 2;
/// Success exit code.
pub const EXIT_OK: i32 = 0;

pub fn run(
    args: MigrateRunArgs,
    cwd: &Utf8PathBuf,
    no_cache_refresh: bool,
    config_path: Option<&Utf8PathBuf>,
    verbose: bool,
) -> Result<i32> {
    // ------------------------------------------------------------------
    // 1. Read plan source
    // ------------------------------------------------------------------
    let raw = read_plan_source(&args.plan_path)
        .with_context(|| format!("failed to read migration plan from '{}'", args.plan_path))?;

    // ------------------------------------------------------------------
    // 2. Determine input format (extension → YAML, else JSON, stdin default JSON)
    // ------------------------------------------------------------------
    let fmt = determine_input_format(&args.plan_path, args.input_format);

    // ------------------------------------------------------------------
    // 3. Parse plan
    // ------------------------------------------------------------------
    let plan = parse_plan(&raw, fmt, &args.plan_path)?;

    // ------------------------------------------------------------------
    // 4. Validate schema version — exit 2 if mismatch
    // ------------------------------------------------------------------
    if plan.schema_version != MIGRATION_PLAN_SCHEMA_VERSION {
        eprintln!(
            "error: unsupported plan schema_version {}; this norn build supports v{}",
            plan.schema_version, MIGRATION_PLAN_SCHEMA_VERSION
        );
        return Ok(EXIT_PREFLIGHT);
    }

    // ------------------------------------------------------------------
    // 4a. Acquire mutation lock (apply-only; dry-run is lock-free).
    //
    // is_apply: --dry-run → never; --yes or JSON format → yes; stdin
    // terminal (interactive) → yes (conservative — will confirm via TTY);
    // non-TTY without --yes → no (implicit dry-run, treat as reader).
    // ------------------------------------------------------------------
    let (_, state_dir) =
        state_dir_for(cwd).map_err(|e| anyhow::anyhow!("could not resolve state dir: {e}"))?;
    // Best-effort sweep of stale pending plans before we do any work.
    sweep_pending(&state_dir);
    let _mutation_lock = {
        use std::io::IsTerminal;
        let is_apply = !args.dry_run
            && (args.yes
                || matches!(args.format, MigrateFormat::Json)
                || std::io::stdin().is_terminal());
        match MutationLock::acquire_if_mutating(&state_dir, is_apply) {
            Ok(guard) => guard,
            Err(crate::cache::CacheError::MutationLockTimeout) => {
                if args.plan_path == "-" {
                    match save_pending_plan(&state_dir, &raw) {
                        Ok(pending_path) => {
                            eprintln!("error: another norn mutation is in progress against this vault (timed out after 5 s)");
                            eprintln!("retry with: norn migrate {pending_path}");
                        }
                        Err(save_err) => {
                            eprintln!("error: another norn mutation is in progress against this vault (timed out after 5 s)");
                            eprintln!("warning: could not save stdin plan for retry: {save_err}");
                        }
                    }
                } else {
                    eprintln!(
                        "error: another norn mutation is in progress against this vault (timed out after 5 s)"
                    );
                }
                return Ok(EXIT_PREFLIGHT);
            }
            Err(e) => return Err(anyhow::anyhow!("mutation lock error: {e}")),
        }
    };
    // `_mutation_lock` is Option<MutationLock> held at function scope until run() returns.

    // ------------------------------------------------------------------
    // 5. Build GraphIndex
    // ------------------------------------------------------------------
    let loaded_config = crate::config_loader::load_config(cwd, config_path)?;
    let index =
        crate::cache_cmd::load_graph_index(cwd, &loaded_config.index_options, no_cache_refresh)?;

    // ------------------------------------------------------------------
    // 6. Determine whether to apply
    //    - --dry-run: never apply
    //    - --yes: skip TTY confirmation
    //    - --format json: implicitly non-interactive
    //    - TTY without --yes: prompt
    //    - Non-TTY without --yes: implicit dry-run
    // ------------------------------------------------------------------
    use std::io::IsTerminal;

    let dry_run = if args.dry_run {
        true
    } else if args.yes || matches!(args.format, MigrateFormat::Json) {
        false
    } else if std::io::stdin().is_terminal() {
        // TTY interactive: prompt
        use std::io::Write;
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut prompt_out = std::io::stderr();
        writeln!(prompt_out)?;
        let ok =
            crate::prompt::confirm(&mut reader, &mut prompt_out, "Apply migration plan? [y/N] ")?;
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
        parents: args.parents,
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
    crate::emit_invocation_started(&mut sink, "migrate", cwd, &plan.vault_root, dry_run, &argv);

    let report = match apply_migration_plan(&plan, &index, ctx, &mut sink) {
        Ok(r) => r,
        Err(e) => {
            // NRN-150: a `--format json` consumer gets the structured
            // `{ code, message, path? }` envelope on stdout; a records/TTY caller
            // gets the prose on stderr. Either way this is a preflight refusal.
            match args.format {
                MigrateFormat::Json => crate::render_json_error_envelope(&e)?,
                MigrateFormat::Records => eprintln!("error: {e:#}"),
            }
            return Ok(EXIT_PREFLIGHT);
        }
    };

    // ------------------------------------------------------------------
    // 7. Determine exit code
    // ------------------------------------------------------------------
    // NRN-150/183: exit on the report's own outcome mapping. A partial-apply
    // failure (a write landed, then an op failed) is now returned as
    // `Ok(report)` with `outcome = failed` → exit 1, not the EXIT_PREFLIGHT (2)
    // of a byte-identical refusal (which still arrives on the `Err` arm above).
    // Success → 0.
    let exit = report.exit_code();

    crate::emit_invocation_finished(&mut sink, "migrate", exit, &report);

    crate::emit_cascade_failure_warnings(&report);

    // ------------------------------------------------------------------
    // 8. Render
    // ------------------------------------------------------------------
    render_report(&report, args.format, args.out.as_deref())?;

    // Delete the pending plan on successful retry so it self-cleans.
    if exit == EXIT_OK {
        let path = camino::Utf8Path::new(&args.plan_path);
        if path.as_str().contains("/pending/") && path.as_str().ends_with(".plan.json") {
            delete_pending_plan(path);
        }
    }

    Ok(exit)
}

/// Read plan content from a file path or stdin (`-`).
fn read_plan_source(plan_path: &str) -> Result<String> {
    if plan_path == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .context("could not read migration plan from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(plan_path)
            .with_context(|| format!("could not read file '{plan_path}'"))
    }
}

/// Determine the input format from path extension or explicit override.
fn determine_input_format(plan_path: &str, override_fmt: Option<InputFormat>) -> InputFormat {
    if let Some(fmt) = override_fmt {
        return fmt;
    }
    // stdin: default JSON
    if plan_path == "-" {
        return InputFormat::Json;
    }
    // detect from extension
    let lower = plan_path.to_ascii_lowercase();
    if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        InputFormat::Yaml
    } else {
        InputFormat::Json
    }
}

/// Parse a `MigrationPlan` from raw text in the given format.
fn parse_plan(raw: &str, fmt: InputFormat, source: &str) -> Result<MigrationPlan> {
    match fmt {
        InputFormat::Yaml => serde_yaml::from_str(raw)
            .with_context(|| format!("failed to parse YAML migration plan from '{source}'")),
        InputFormat::Json => serde_json::from_str(raw)
            .with_context(|| format!("failed to parse JSON migration plan from '{source}'")),
    }
}

/// Render the apply report to stdout, OR to a file when `--out` is set.
///
/// `--out` is mutually exclusive with stdout output: when set, the report
/// (always JSON) is written to the file and stdout is silent. This matches
/// repair-apply's convention — if you wanted both file + stdout, run twice.
fn render_report(report: &ApplyReport, format: MigrateFormat, out: Option<&str>) -> Result<()> {
    if let Some(out_path) = out {
        let json = serde_json::to_string_pretty(report)?;
        std::fs::write(out_path, format!("{json}\n"))
            .with_context(|| format!("failed to write apply report to '{out_path}'"))?;
        return Ok(());
    }

    let stdout = io::stdout();
    let mut out_lock = stdout.lock();
    match format {
        MigrateFormat::Json => {
            let json = serde_json::to_string_pretty(report)?;
            out_lock.write_all(json.as_bytes())?;
            out_lock.write_all(b"\n")?;
        }
        MigrateFormat::Records => {
            render_records(report, &mut out_lock)?;
            // TTY `trace:` footer on real apply (Records only; JSON carries
            // trace_id as a field).
            if !report.dry_run {
                writeln!(out_lock, "trace: {}", report.trace_id)?;
            }
        }
    }
    Ok(())
}

/// Human-readable records rendering for the apply report.
fn render_records(report: &ApplyReport, out: &mut dyn Write) -> Result<()> {
    let status_label = if report.dry_run { "dry-run" } else { "applied" };
    writeln!(out, "migrate {status_label}")?;
    writeln!(
        out,
        "  applied: {}  skipped: {}  failed: {}  remaining: {}",
        report.applied, report.skipped, report.failed, report.remaining
    )?;
    for op in &report.operations {
        let status = format!("{:?}", op.status).to_lowercase();
        writeln!(out, "  [{status}] {}", op.summary)?;
    }
    if !report.warnings.is_empty() {
        writeln!(out, "warnings:")?;
        for w in &report.warnings {
            writeln!(out, "  {}: {}", w.code, w.message)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_detection_yaml_extension() {
        assert!(matches!(
            determine_input_format("plan.yaml", None),
            InputFormat::Yaml
        ));
        assert!(matches!(
            determine_input_format("plan.yml", None),
            InputFormat::Yaml
        ));
    }

    #[test]
    fn format_detection_json_extension_and_default() {
        assert!(matches!(
            determine_input_format("plan.json", None),
            InputFormat::Json
        ));
        assert!(matches!(
            determine_input_format("plan", None),
            InputFormat::Json
        ));
    }

    #[test]
    fn format_detection_stdin_defaults_json() {
        assert!(matches!(
            determine_input_format("-", None),
            InputFormat::Json
        ));
    }

    #[test]
    fn format_detection_override_wins() {
        assert!(matches!(
            determine_input_format("plan.json", Some(InputFormat::Yaml)),
            InputFormat::Yaml
        ));
    }
}
