//! `vault find` command implementation.

pub mod pager;
pub mod query;
pub mod render;

use std::io::{IsTerminal, Write};

use anyhow::Result;
use camino::Utf8Path;

use crate::cli::FindArgs;

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
        && self::pager::should_page(buffer_lines, args.no_pager, stdout_is_tty);

    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    if should_page {
        self::pager::spawn_pager_or_passthrough(&buffer, &mut stdout_lock, &mut stderr_lock)?;
    } else {
        stdout_lock.write_all(&buffer)?;
    }

    self::render::warn_col_ignored_on_paths(&args.col, format, &mut stderr_lock)?;
    self::render::warn_absent_cols(&result, &args.col, &mut stderr_lock)?;

    let exit = if cache.has_diagnostic_errors()? { 2 } else { 0 };
    Ok(exit)
}
