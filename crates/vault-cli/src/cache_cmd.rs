//! `vault cache` subcommand handlers and the cache-backed read path for
//! query commands.

use crate::cache::{Cache, CacheError, ChangeDetectOptions};
use crate::core::GraphIndex;
use crate::graph::IndexOptions;
use crate::standards::path_match::PathPattern;
use anyhow::Result;
use camino::Utf8Path;

use crate::cli::{CacheIndexArgs, CacheOutputFormat, CacheStatusArgs};

/// Load the graph index for a query command. Opens the per-vault cache,
/// optionally runs an implicit incremental refresh, then reconstructs the
/// in-memory `GraphIndex` from the cached rows. Configured `ignore`
/// patterns are applied as a read-time filter so cache contents stay
/// independent of per-invocation config.
///
/// Lock contention during the implicit refresh is non-fatal: the command
/// proceeds against the current cache state and writes a single stderr
/// note. Set `no_cache_refresh = true` to skip the refresh entirely.
pub fn load_graph_index(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    no_cache_refresh: bool,
) -> Result<GraphIndex> {
    let mut cache = Cache::open_with_config(vault_root, options.alias_field.as_deref())?;
    if !no_cache_refresh {
        match cache.index_incremental(vault_root, &ChangeDetectOptions::default()) {
            Ok(_) => {}
            Err(CacheError::LockTimeout) => {
                eprintln!(
                    "vault: another cache operation is in progress; using current cache state"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut index = cache.load_graph_index()?;
    apply_ignore_filter(&mut index, &options.ignore);
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
    alias_field: Option<&str>,
    no_cache_refresh: bool,
) -> Result<Cache> {
    let mut cache = Cache::open_with_config(vault_root, alias_field)?;
    if !no_cache_refresh {
        match cache.index_incremental(vault_root, &ChangeDetectOptions::default()) {
            Ok(_) => {}
            Err(CacheError::LockTimeout) => {
                eprintln!(
                    "vault: another cache operation is in progress; using current cache state"
                );
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(cache)
}

fn apply_ignore_filter(index: &mut GraphIndex, ignore: &[String]) {
    // Pre-compile patterns once; PathPattern::parse (Regex::new) is expensive.
    let compiled: Vec<PathPattern> = ignore
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .filter_map(|p| PathPattern::parse(p).ok())
        .collect();
    if compiled.is_empty() {
        return;
    }
    let is_ignored = |path: &camino::Utf8Path| -> bool {
        compiled
            .iter()
            .any(|p| p.match_path(path.as_str()).is_some())
    };
    let mut ignored_files: Vec<camino::Utf8PathBuf> = Vec::new();
    index.files.retain(|f| {
        if is_ignored(&f.path) {
            ignored_files.push(f.path.clone());
            false
        } else {
            true
        }
    });
    index.documents.retain(|d| !is_ignored(&d.path));
    index.ignored_files.extend(ignored_files);
    index.ignored_files.sort();
    index.ignored_files.dedup();
}

pub fn run_index(
    vault_root: &Utf8Path,
    alias_field: Option<&str>,
    args: &CacheIndexArgs,
) -> Result<()> {
    let mut cache = Cache::open_with_config(vault_root, alias_field)?;
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

pub fn run_rebuild(vault_root: &Utf8Path, alias_field: Option<&str>) -> Result<()> {
    let mut cache = Cache::open_with_config(vault_root, alias_field)?;
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

pub fn run_status(
    vault_root: &Utf8Path,
    alias_field: Option<&str>,
    args: &CacheStatusArgs,
) -> Result<()> {
    let cache = Cache::open_with_config(vault_root, alias_field)?;
    let status = cache.status()?;
    match args.format {
        CacheOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        CacheOutputFormat::Text => {
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
