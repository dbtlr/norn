//! `norn find` command implementation.

pub mod query;
pub mod render;
pub mod route;

use std::io::{IsTerminal, Write};

use anyhow::Result;
use camino::Utf8Path;

use crate::cli::FindArgs;

/// True when the user supplied at least one predicate that constrains the
/// result set. Sort, limit, format, and --col are output modifiers, not
/// predicates; running with only those would dump the whole vault.
///
/// Compares against the default (empty) filter set rather than enumerating
/// fields, so a filter flag added to `FilterArgs` can never be silently
/// missing here. The one normalization: `--text ""` is documented as a
/// no-op, not a predicate.
fn has_predicate(args: &FindArgs) -> bool {
    let mut filters = args.filters.clone();
    if filters.text.as_deref().is_some_and(str::is_empty) {
        filters.text = None;
    }
    filters != crate::cli::FilterArgs::default()
}

/// Print `norn find --help` to stderr. Used as the "missing predicate" gate.
fn print_find_help() -> Result<()> {
    use clap::CommandFactory;
    let mut cmd = crate::cli::Cli::command();
    let find = cmd
        .find_subcommand_mut("find")
        .ok_or_else(|| anyhow::anyhow!("find subcommand missing from CLI tree"))?;
    let mut stderr = std::io::stderr().lock();
    find.write_help(&mut stderr)?;
    Ok(())
}

fn resolve_format(explicit: Option<crate::cli::FindFormat>) -> crate::cli::FindFormat {
    match explicit {
        Some(fmt) => fmt,
        None => {
            if std::io::stdout().is_terminal() {
                crate::cli::FindFormat::Records
            } else {
                crate::cli::FindFormat::Paths
            }
        }
    }
}

pub fn run(
    args: FindArgs,
    cwd: &Utf8Path,
    loaded_config: &crate::config_loader::LoadedConfig,
    no_cache_refresh: bool,
    color: crate::cli::ColorWhen,
    dynamic_keys: &[String],
) -> Result<i32> {
    if !args.all && !has_predicate(&args) {
        print_find_help()?;
        return Ok(2);
    }

    let cache =
        crate::cache_cmd::open_for_query(cwd, &loaded_config.index_options, no_cache_refresh)?;
    crate::gate_dynamic_query(
        &cache,
        loaded_config,
        dynamic_keys,
        crate::grammar::QueryCmd::Find,
    )?;

    // Shared selection seam: matched docs + deep/raw fetches. The MCP
    // `vault.find` tool consumes the same `select`/`query` path, so the two
    // surfaces can't drift on which documents match or what gets fetched.
    let self::query::Selection { result, deep, raw } = self::query::select(&cache, &args)?;

    // Shared print seam with the daemon-routed path (`route_find`).
    let palette = crate::output::palette::resolve(color);
    emit(&result, &deep, &raw, &args, &palette)?;

    let exit = if cache.has_diagnostic_errors()? { 2 } else { 0 };
    Ok(exit)
}

/// Render a completed find selection to stdout/stderr — the shared print seam
/// used by both the direct dispatch ([`run`]) and the NRN-222 daemon-routed path
/// (`route_find` in `src/lib.rs`), so the two cannot drift on format resolution,
/// paging, or the `--col` warnings. Deliberately does NOT decide the exit code:
/// the direct path derives it from the vault's diagnostics (`has_diagnostic_errors`),
/// which the routed path — serving from an already-verified warm cache — cannot see.
pub fn emit(
    result: &crate::cache::FindResult,
    deep: &[Option<crate::cache::DocumentDeep>],
    raw: &[Option<String>],
    args: &FindArgs,
    palette: &crate::output::palette::Palette,
) -> Result<()> {
    // `build_find_query` is pure and cheap; recompute the sort echo + paging
    // offset the renderers want rather than thread them through.
    let query = self::query::build_find_query(args)?;
    let format = resolve_format(args.format);

    let (sort_field, sort_direction) = match &query.sort {
        Some(s) => (
            Some(s.field.as_str()),
            Some(match s.direction {
                crate::cache::SortDirection::Asc => "asc",
                crate::cache::SortDirection::Desc => "desc",
            }),
        ),
        None => (None, None),
    };

    let stdout_is_tty = std::io::stdout().is_terminal();
    let stderr = std::io::stderr();
    let mut stderr_lock = stderr.lock();

    let mut buffer: Vec<u8> = Vec::new();
    self::render::render(
        result,
        deep,
        raw,
        args,
        format,
        sort_field,
        sort_direction,
        query.starts_at,
        palette,
        &mut buffer,
        &mut stderr_lock,
    )?;

    let buffer_lines = buffer.iter().filter(|&&b| b == b'\n').count();
    let should_page = matches!(format, crate::cli::FindFormat::Records)
        && crate::output::pager::should_page(buffer_lines, args.no_pager, stdout_is_tty);

    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    if should_page {
        crate::output::pager::spawn_pager_or_passthrough(
            &buffer,
            &mut stdout_lock,
            &mut stderr_lock,
            "norn find",
        )?;
    } else {
        stdout_lock.write_all(&buffer)?;
    }

    self::render::warn_col_ignored_on_paths(&args.col, format, &mut stderr_lock)?;
    self::render::warn_unknown_cols(result, &args.col, &mut stderr_lock)?;
    Ok(())
}
