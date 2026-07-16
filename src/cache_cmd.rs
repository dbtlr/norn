//! `norn cache` subcommand handlers and the cache-backed read path for
//! query commands.

use crate::cache::prune::{EvictReason, EvictedEntry, PruneOptions, PruneReport};
use crate::cache::{Cache, CacheError, ChangeDetectOptions};
use crate::core::GraphIndex;
use crate::graph::IndexOptions;
use anyhow::Result;
use camino::Utf8Path;

use crate::cli::{CacheIndexArgs, CacheOutputFormat, CachePruneArgs, CacheStatusArgs};

/// Load the graph index for a query command. Opens the per-vault cache,
/// optionally runs an implicit incremental refresh, then reconstructs the
/// in-memory `GraphIndex` from the cached rows. `files.ignore` is enforced at
/// cache-build time (via `Cache::files_ignore`), so the loaded index is already
/// filtered — no read-time pass here (NRN-117).
///
/// Lock contention during the implicit refresh is non-fatal: the command
/// proceeds against the current cache state and writes a single stderr
/// note. Set `no_cache_refresh = true` to skip the refresh entirely.
pub fn load_graph_index(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    no_cache_refresh: bool,
) -> Result<GraphIndex> {
    let mut cache = Cache::open_with_index(
        vault_root,
        options.alias_field.as_deref(),
        &options.ignore,
        &options.resolved_index_set,
        &options.resolved_index_set_hash,
    )?;
    if !no_cache_refresh {
        match cache.index_incremental(vault_root, &ChangeDetectOptions::default()) {
            Ok(_) => {}
            Err(CacheError::LockTimeout) => {
                eprintln!("{}", crate::cache::LOCK_CONTENTION_NOTE);
            }
            Err(error) => return Err(error.into()),
        }
    }
    // files.ignore is applied at cache-build time (the scan gate in
    // graph::build_index_with_options, threaded via Cache::files_ignore), so the
    // loaded index is already filtered — ignored docs are absent and links into
    // them are unresolved. No read-time retain is needed or wanted; the earlier
    // one only dropped in-memory docs (not the SQLite rows count/find/get read)
    // and never retracted already-resolved links (NRN-117, ADR 0007).
    let index = cache.load_graph_index()?;
    Ok(index)
}

/// Open the per-vault cache for query commands. Runs the implicit
/// incremental refresh (unless `no_cache_refresh = true`), returning a
/// usable `Cache` handle. Lock contention during refresh is non-fatal —
/// emits the same stderr note as `load_graph_index` and continues against
/// the current cache state.
#[allow(dead_code)]
pub fn open_for_query(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    no_cache_refresh: bool,
) -> Result<Cache> {
    let mut cache = Cache::open_with_index(
        vault_root,
        options.alias_field.as_deref(),
        &options.ignore,
        &options.resolved_index_set,
        &options.resolved_index_set_hash,
    )?;
    if !no_cache_refresh {
        match cache.index_incremental(vault_root, &ChangeDetectOptions::default()) {
            Ok(_) => {}
            Err(CacheError::LockTimeout) => {
                eprintln!("{}", crate::cache::LOCK_CONTENTION_NOTE);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(cache)
}

pub fn run_index(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    args: &CacheIndexArgs,
) -> Result<()> {
    let mut cache = Cache::open_with_index(
        vault_root,
        options.alias_field.as_deref(),
        &options.ignore,
        &options.resolved_index_set,
        &options.resolved_index_set_hash,
    )?;
    if args.rebuild {
        let report = cache.rebuild(vault_root)?;
        eprintln!(
            "vault: cache rebuilt {} docs, {} links in {}ms",
            report.doc_count, report.link_count, report.duration_ms,
        );
    } else {
        let report = cache.index_incremental(
            vault_root,
            &ChangeDetectOptions {
                force_hash: args.force_hash,
            },
        )?;
        eprintln!(
            "vault: cache indexed {} docs, {} links in {}ms",
            report.doc_count, report.link_count, report.duration_ms,
        );
    }
    Ok(())
}

pub fn run_rebuild(vault_root: &Utf8Path, options: &IndexOptions) -> Result<()> {
    let mut cache = Cache::open_with_index(
        vault_root,
        options.alias_field.as_deref(),
        &options.ignore,
        &options.resolved_index_set,
        &options.resolved_index_set_hash,
    )?;
    let report = cache.rebuild(vault_root)?;
    eprintln!(
        "vault: cache rebuilt {} docs, {} links in {}ms",
        report.doc_count, report.link_count, report.duration_ms,
    );
    Ok(())
}

pub fn run_clear(vault_root: &Utf8Path) -> Result<()> {
    // `clear` discards the database entirely; the next open recreates with
    // whatever alias_field is then in scope, so we don't need to pass one here.
    let mut cache = Cache::open(vault_root)?;
    cache.clear()?;
    eprintln!("vault: cache cleared");
    Ok(())
}

pub fn run_prune(
    vault_root: &Utf8Path,
    cache_cfg: Option<&crate::standards::CacheConfig>,
    args: &CachePruneArgs,
) -> Result<()> {
    let cache_tree = crate::cache::cache_tree_root()?;
    let state_tree = crate::cache::state_tree_root()?;
    let retention = args
        .retention
        .or_else(|| cache_cfg.and_then(|c| c.retention))
        .unwrap_or(crate::standards::DEFAULT_CACHE_RETENTION);
    // Best-effort: an un-canonicalizable cwd just means no exemption.
    let exempt_hash = crate::cache::vault_identity_hash(vault_root);
    let opts = PruneOptions {
        retention,
        cap_bytes: crate::cache::prune::CACHE_TREE_SIZE_CAP_BYTES,
        dry_run: args.dry_run,
        exempt_hash,
    };
    let report = crate::cache::prune::sweep(&cache_tree, &state_tree, &opts);
    if !args.dry_run {
        crate::cache::prune::touch_marker(&cache_tree);
    }
    match args.format {
        CacheOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        CacheOutputFormat::Text => render_prune_text(&report),
    }
    Ok(())
}

fn render_prune_text(report: &PruneReport) {
    let verb = if report.dry_run {
        "planned"
    } else {
        "performed"
    };
    let freed = if report.dry_run {
        "would be freed"
    } else {
        "freed"
    };
    let header = if report.dry_run {
        "would be evicted:"
    } else {
        "evicted:"
    };
    for (label, tree) in [("cache", &report.cache), ("state", &report.state)] {
        let mut notes = Vec::new();
        if tree.skipped_locked > 0 {
            notes.push(format!("{} skipped: locked", tree.skipped_locked));
        }
        if tree.skipped_errors > 0 {
            notes.push(format!("{} skipped: error", tree.skipped_errors));
        }
        if tree.kept_unknown > 0 {
            notes.push(format!("{} kept: root unknown", tree.kept_unknown));
        }
        let notes = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        };
        println!(
            "{label}  {} {} {verb}, {} {freed}{notes}",
            tree.evicted.len(),
            // "evictions", not "entries": one entry can emit a dev-stale row
            // plus a terminal row in the same sweep.
            if tree.evicted.len() == 1 {
                "eviction"
            } else {
                "evictions"
            },
            format_bytes(tree.bytes_freed),
        );
    }
    let all: Vec<(&str, &EvictedEntry)> = report
        .cache
        .evicted
        .iter()
        .map(|e| ("cache", e))
        .chain(report.state.evicted.iter().map(|e| ("state", e)))
        .collect();
    if !all.is_empty() {
        println!();
        println!("{header}");
        for (tree, e) in all {
            let root = e.root.as_deref().unwrap_or("(root unknown)");
            let reason = match e.reason {
                EvictReason::DeadRoot => "dead root".to_string(),
                EvictReason::Unreadable => "unreadable".to_string(),
                EvictReason::Empty => "empty".to_string(),
                EvictReason::Aged => match e.age_days {
                    Some(d) => format!("aged {d}d"),
                    None => "aged".to_string(),
                },
                EvictReason::OverCap => "over cap".to_string(),
                EvictReason::DevStale => match e.age_days {
                    Some(d) => format!("dev stale {d}d"),
                    None => "dev stale".to_string(),
                },
            };
            println!("  [{tree}] {root}  {reason}  {}", format_bytes(e.bytes));
        }
    }
}

fn format_bytes(b: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, size) in UNITS {
        if b >= size {
            return format!("{:.1} {unit}", b as f64 / size as f64);
        }
    }
    format!("{b} B")
}

pub fn run_status(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    args: &CacheStatusArgs,
) -> Result<()> {
    let cache = Cache::open_with_index(
        vault_root,
        options.alias_field.as_deref(),
        &options.ignore,
        &options.resolved_index_set,
        &options.resolved_index_set_hash,
    )?;
    let status = cache.status()?;
    match args.format {
        CacheOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        CacheOutputFormat::Text => {
            println!("channel:           {}", status.channel);
            println!("cache path:        {}", status.cache_path);
            println!("size:              {} bytes", status.size_bytes);
            println!("documents:         {}", status.doc_count);
            println!("files:             {}", status.file_count);
            println!("links:             {}", status.link_count);
            println!("schema version:    {}", status.schema_version);
            if let Some(ts) = status.last_full_rebuild {
                println!("last full rebuild: {ts}");
            }
        }
    }
    Ok(())
}
