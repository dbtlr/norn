//! `vault find` command implementation.

pub mod query;
pub mod render;

use std::io::{IsTerminal, Write};

use anyhow::Result;
use camino::Utf8Path;

use crate::cli::FindArgs;

/// True when the user supplied at least one predicate that constrains the
/// result set. Sort, limit, format, and --col are output modifiers, not
/// predicates; running with only those would dump the whole vault.
fn has_predicate(args: &FindArgs) -> bool {
    args.text.as_deref().is_some_and(|t| !t.is_empty())
        || !args.eq.is_empty()
        || !args.not_eq.is_empty()
        || !args.r#in.is_empty()
        || !args.not_in.is_empty()
        || !args.has.is_empty()
        || !args.missing.is_empty()
        || !args.before.is_empty()
        || !args.after.is_empty()
        || !args.on.is_empty()
        || !args.path.is_empty()
}

/// Print `vault find --help` to stderr. Used as the "missing predicate" gate.
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
    no_cache_refresh: bool,
    color: crate::cli::ColorWhen,
) -> Result<i32> {
    if !args.all && !has_predicate(&args) {
        print_find_help()?;
        return Ok(2);
    }

    let cache = crate::cache::open_for_query(cwd, no_cache_refresh)?;
    let query = self::query::build_find_query(&args)?;
    let result = cache.find_documents(&query)?;

    let format = resolve_format(args.format);
    let palette = crate::output::palette::resolve(color);

    let (sort_field, sort_direction) = match &query.sort {
        Some(s) => (
            Some(s.field.as_str()),
            Some(match s.direction {
                vault_cache::SortDirection::Asc => "asc",
                vault_cache::SortDirection::Desc => "desc",
            }),
        ),
        None => (None, None),
    };

    let stdout_is_tty = std::io::stdout().is_terminal();
    let stderr = std::io::stderr();
    let mut stderr_lock = stderr.lock();

    let mut buffer: Vec<u8> = Vec::new();
    self::render::render(
        &result,
        &args,
        format,
        sort_field,
        sort_direction,
        query.starts_at,
        &palette,
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
            "vault find",
        )?;
    } else {
        stdout_lock.write_all(&buffer)?;
    }

    self::render::warn_col_ignored_on_paths(&args.col, format, &mut stderr_lock)?;
    self::render::warn_absent_cols(&result, &args.col, &mut stderr_lock)?;

    let exit = if cache.has_diagnostic_errors()? { 2 } else { 0 };
    Ok(exit)
}
