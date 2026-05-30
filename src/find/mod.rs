//! `norn find` command implementation.

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
    args.filters.text.as_deref().is_some_and(|t| !t.is_empty())
        || !args.filters.eq.is_empty()
        || !args.filters.not_eq.is_empty()
        || !args.filters.r#in.is_empty()
        || !args.filters.not_in.is_empty()
        || !args.filters.has.is_empty()
        || !args.filters.missing.is_empty()
        || !args.filters.before.is_empty()
        || !args.filters.after.is_empty()
        || !args.filters.on.is_empty()
        || !args.filters.path.is_empty()
        || !args.filters.links_to.is_empty()
        || args.filters.unresolved_links
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
    alias_field: Option<&str>,
    no_cache_refresh: bool,
    color: crate::cli::ColorWhen,
) -> Result<i32> {
    if !args.all && !has_predicate(&args) {
        print_find_help()?;
        return Ok(2);
    }

    let cache = crate::cache_cmd::open_for_query(cwd, alias_field, no_cache_refresh)?;
    let mut query = self::query::build_find_query(&args)?;
    // `--links-to` targets resolve against the cache (stem/alias lookup), so
    // resolution happens here rather than in the pure query builder.
    query.predicates.links_to =
        crate::filter_args::resolve_links_to(&cache, &args.filters.links_to)?;
    let result = cache.find_documents(&query)?;

    // Join-backed facets (`.headings` and the link sets) require a per-doc deep
    // fetch; the cheap facets (`.frontmatter`, `.body`, `.path`) are already on
    // each `DocumentSummary`. Only pay the join cost when a deep facet is asked
    // for — the default frontmatter-only path stays at zero extra queries.
    let (facets, _fields) = crate::output::projection::split_cols(&args.col);
    let needs_deep = facets.iter().any(|f| {
        matches!(
            f.as_str(),
            "headings" | "outgoing_links" | "unresolved_links" | "incoming_links"
        )
    });
    let deep: Vec<Option<crate::cache::DocumentDeep>> = if needs_deep {
        let mut out = Vec::with_capacity(result.matches.len());
        for doc in &result.matches {
            // body not needed — find already carries `body_text`.
            out.push(cache.document_with_connections(doc.path.as_path(), false)?);
        }
        out
    } else {
        Vec::new()
    };

    let format = resolve_format(args.format);
    let palette = crate::output::palette::resolve(color);

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
        &result,
        &deep,
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
            "norn find",
        )?;
    } else {
        stdout_lock.write_all(&buffer)?;
    }

    self::render::warn_col_ignored_on_paths(&args.col, format, &mut stderr_lock)?;
    self::render::warn_unknown_cols(&result, &args.col, &mut stderr_lock)?;

    let exit = if cache.has_diagnostic_errors()? { 2 } else { 0 };
    Ok(exit)
}
