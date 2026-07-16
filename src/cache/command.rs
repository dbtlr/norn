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
    let index = retry_once_on_corruption(
        || load_graph_index_attempt(vault_root, options, no_cache_refresh),
        || notice_and_evict(vault_root),
    )?;
    Ok(index)
}

fn load_graph_index_attempt(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    no_cache_refresh: bool,
) -> Result<GraphIndex> {
    let mut cache = Cache::open_with_index_trusting(
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
    cache.load_graph_index()
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
    let cache = retry_once_on_corruption(
        || open_for_query_attempt(vault_root, options, no_cache_refresh),
        || notice_and_evict(vault_root),
    )?;
    Ok(cache)
}

fn open_for_query_attempt(
    vault_root: &Utf8Path,
    options: &IndexOptions,
    no_cache_refresh: bool,
) -> Result<Cache> {
    let mut cache = Cache::open_with_index_trusting(
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

/// Run a cache-backed read, rebuilding the cache once if it surfaces SQLite
/// corruption. NRN-275 removed the per-open `PRAGMA integrity_check` from the
/// direct read path (`Cache::open_with_index_trusting`), so byte corruption now
/// surfaces as `SQLITE_CORRUPT` / `SQLITE_NOTADB` while a query reads a bad page
/// rather than at open. The cache is a rebuildable derived artifact: on the first
/// corruption we run `on_corruption` (evict the database) and retry the read
/// ONCE — the retry opens a Fresh db and the mandatory incremental refresh
/// reconstructs it from the vault, re-establishing trust. A second corruption (a
/// fault the rebuild could not clear) propagates instead of looping.
fn retry_once_on_corruption<T>(
    mut attempt: impl FnMut() -> Result<T>,
    mut on_corruption: impl FnMut() -> Result<()>,
) -> Result<T> {
    match attempt() {
        Err(err) if is_cache_corruption(&err) => {
            on_corruption()?;
            attempt()
        }
        other => other,
    }
}

/// Does this error chain carry a SQLite corruption-class failure
/// (`SQLITE_CORRUPT` / `SQLITE_NOTADB`)? These are the codes SQLite raises when a
/// query reads structurally malformed bytes — the signal to evict + rebuild. The
/// concrete `rusqlite::Error` is usually wrapped (in `CacheError::Sqlite`, then
/// `anyhow`), so walk the whole chain and downcast each cause — the same
/// classification the warm daemon's eviction seam uses.
///
/// `pub(crate)` so the top-level CLI error seam (`crate::cli_main`) can classify a
/// command's error and self-heal a cache whose corruption first surfaced at query
/// time — after `open_for_query` already returned — which the in-place retry below
/// cannot reach (see [`evict_corrupt_cache_after_error`]).
pub(crate) fn is_cache_corruption(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|e| e.sqlite_error_code())
            .is_some_and(|code| {
                matches!(
                    code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
            })
    })
}

/// The `vault:` stderr notice that a corrupt cache was discarded and will be
/// rebuilt from the vault. Mirrors the `emit_rebuild_message` notice the
/// schema/identity/alias rebuild paths print (NRN-275): the trusting open's retry
/// re-opens straight into the Fresh arm, so `emit_rebuild_message` never fires for
/// this path and this is the only operator-visible signal a rebuild happened.
const CORRUPTION_REBUILD_NOTICE: &str =
    "vault: cache is corrupted; discarding it — rebuilding from the vault";

/// Emit the corruption notice, then evict — the eviction seam the in-place retry
/// uses (the read is retried immediately after, rebuilding in this same process).
fn notice_and_evict(vault_root: &Utf8Path) -> Result<()> {
    eprintln!("{CORRUPTION_REBUILD_NOTICE}");
    evict_cache_db(vault_root)
}

/// Top-level self-heal for a corruption that first surfaced at QUERY time —
/// after `open_for_query` / `load_graph_index` already returned a handle, so the
/// command ran its SQL outside the in-place retry above and failed. Called from
/// the CLI error seam: emit the notice and evict so the NEXT invocation opens
/// Fresh and rebuilds from the vault (this invocation still fails closed with the
/// error). Best-effort — a lock timeout or a racing eviction is swallowed, since
/// the caller is already on the error path and the next open re-detects the state.
pub(crate) fn evict_corrupt_cache_after_error(vault_root: &Utf8Path) {
    eprintln!("{CORRUPTION_REBUILD_NOTICE}");
    let _ = evict_cache_db(vault_root);
}

/// Evict a vault's cache database — delete `cache.db` and its WAL/SHM sidecars —
/// so the next open takes the Fresh path and the incremental refresh rebuilds it
/// from the vault. Used by the rebuild-on-corruption retry and the error seam.
///
/// Holds the shared cache write lock across the unlink: a concurrent process
/// mid-refresh holds this same lock, so taking it here closes the race where we
/// would otherwise unlink `cache.db` out from under an in-flight writer, leaving
/// it writing to an orphaned inode. `NotFound` on the primary db is tolerated (a
/// concurrent eviction won the race), matching the already-tolerant WAL/SHM
/// removals. Compare [`run_clear`], which deletes the whole entry dir up front
/// (a zero-timeout lock, no open) rather than rebuilding in-process afterward.
fn evict_cache_db(vault_root: &Utf8Path) -> Result<()> {
    let layout = crate::cache::identity::cache_layout_for(vault_root)?;
    let _lock = crate::cache::lock::WriteLock::acquire(
        &layout.entry_dir,
        std::time::Duration::from_secs(5),
    )?;
    let db_path = layout.db_dir.join("cache.db");
    match std::fs::remove_file(db_path.as_std_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("evicting corrupt cache at {db_path}"))
            )
        }
    }
    let _ = std::fs::remove_file(db_path.with_extension("db-wal").as_std_path());
    let _ = std::fs::remove_file(db_path.with_extension("db-shm").as_std_path());
    Ok(())
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

/// Delete the vault's ENTIRE cache entry dir — every channel, every
/// schema-version database, and any legacy layout leftovers — WITHOUT opening
/// the cache first (NRN-270). `clear` is the operator escape hatch: it must
/// work against ANY broken cache state (a corrupt `cache.db`, an undecodable
/// `meta` row, a permission-damaged file), so it can never route through
/// `Cache::open`/SQLite — opening first is exactly what leaves the escape
/// hatch unable to fire when it's needed most. The entry dir is resolved
/// purely from the vault's canonical-path identity (the same mapping
/// `Cache::open` uses internally, with no SQLite involved), and the shared
/// entry `.lock` is taken with a zero-timeout flock — the same primitive
/// `prune`'s `evict_cache_entry` uses — so a concurrent writer blocks the
/// clear instead of racing it. The vault-level STATE tree (the mutation event
/// stream) is never touched; only the cache entry dir is removed.
///
/// Returns the process exit code: `0` on success, including when the entry
/// dir was already absent ("already clear" is success, not an error). `2` if
/// another process holds the write lock — the clear is refused and nothing is
/// deleted, mirroring the exit-2 convention other lock-contention refusals use
/// (see `CacheError::MutationLockTimeout` call sites). Any OTHER filesystem
/// failure (a permission error checking or removing the entry, a lock-file
/// open failure that isn't the concurrent-deletion race below) is a real
/// error and propagates as one — never silently downgraded to "already
/// clear" or misreported as contention.
pub fn run_clear(vault_root: &Utf8Path) -> Result<i32> {
    let layout = crate::cache::identity::cache_layout_for(vault_root)?;
    // `Path::exists()` swallows every stat failure (including a permission
    // error) into `false`, which would misreport a real problem as "already
    // clear". `try_exists()` keeps `Ok(false)` for a genuine absence but
    // surfaces anything else as an `Err` we propagate.
    match layout.entry_dir.as_std_path().try_exists() {
        Ok(false) => {
            eprintln!("vault: cache cleared");
            return Ok(0);
        }
        Ok(true) => {}
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("checking cache entry at {}", layout.entry_dir)))
        }
    }
    let lock_path = layout.entry_dir.join(".lock");
    match crate::cache::acquire_flock(&lock_path, std::time::Duration::ZERO) {
        Ok(_held) => {
            match std::fs::remove_dir_all(layout.entry_dir.as_std_path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow::Error::new(e)
                        .context(format!("clearing cache at {}", layout.entry_dir)))
                }
            }
            eprintln!("vault: cache cleared");
            Ok(0)
        }
        // The entry dir vanished between the `try_exists` check above and this
        // acquire (a concurrent clear/prune won the race): opening the lock
        // file inside `acquire_flock` fails `NotFound` because its parent
        // directory is gone, before the lock loop ever runs. Same outcome as
        // the up-front absence check: already clear.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("vault: cache cleared");
            Ok(0)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            eprintln!("error: cache is locked by another norn process; refusing to clear");
            Ok(2)
        }
        Err(e) => {
            Err(anyhow::Error::new(e).context(format!("acquiring cache lock at {lock_path}")))
        }
    }
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
        exempt_own_db_subpath: crate::cache::prune::own_db_subpath()?,
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
            // "evictions", not "entries": one entry can emit a stale-db row
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
                EvictReason::StaleDb => match e.age_days {
                    Some(d) => format!("stale db {d}d"),
                    None => "stale db".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A synthesized `SQLITE_CORRUPT` failure wrapped exactly as production wraps
    /// it: `rusqlite::Error` → `CacheError::Sqlite` → `anyhow`. Proves the chain
    /// walk reaches the concrete code through both layers.
    fn corrupt_err() -> anyhow::Error {
        let sqlite = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        );
        anyhow::Error::new(CacheError::Sqlite(sqlite))
    }

    fn notadb_err() -> anyhow::Error {
        let sqlite = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_NOTADB),
            None,
        );
        anyhow::Error::new(CacheError::Sqlite(sqlite))
    }

    #[test]
    fn is_cache_corruption_classifies_sqlite_codes() {
        assert!(is_cache_corruption(&corrupt_err()));
        assert!(is_cache_corruption(&notadb_err()));

        // A non-corruption sqlite failure (BUSY) must NOT classify as corruption.
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(!is_cache_corruption(&anyhow::Error::new(
            CacheError::Sqlite(busy)
        )));

        // A plain non-sqlite error must NOT classify as corruption.
        assert!(!is_cache_corruption(&anyhow::anyhow!("unrelated failure")));

        // A CacheError variant that carries no sqlite cause must NOT classify.
        assert!(!is_cache_corruption(&anyhow::Error::new(
            CacheError::LockTimeout
        )));
    }

    #[test]
    fn retry_once_on_corruption_passes_success_through_without_eviction() {
        let attempts = Cell::new(0usize);
        let evicts = Cell::new(0usize);
        let out = retry_once_on_corruption(
            || {
                attempts.set(attempts.get() + 1);
                Ok(7)
            },
            || {
                evicts.set(evicts.get() + 1);
                Ok(())
            },
        );
        assert_eq!(out.unwrap(), 7);
        assert_eq!(attempts.get(), 1, "success must not retry");
        assert_eq!(evicts.get(), 0, "success must not evict");
    }

    #[test]
    fn retry_once_on_corruption_evicts_rebuilds_and_succeeds() {
        let attempts = Cell::new(0usize);
        let evicts = Cell::new(0usize);
        let out = retry_once_on_corruption(
            || {
                let n = attempts.get();
                attempts.set(n + 1);
                if n == 0 {
                    Err(corrupt_err())
                } else {
                    Ok(42)
                }
            },
            || {
                evicts.set(evicts.get() + 1);
                Ok(())
            },
        );
        assert_eq!(out.unwrap(), 42, "the retry after eviction must succeed");
        assert_eq!(attempts.get(), 2, "corruption must drive exactly one retry");
        assert_eq!(evicts.get(), 1, "the retry must evict exactly once");
    }

    #[test]
    fn retry_once_on_corruption_fails_closed_on_re_corruption() {
        // A second corruption (the rebuild could not clear the fault) must
        // propagate the error rather than loop — fail closed.
        let attempts = Cell::new(0usize);
        let evicts = Cell::new(0usize);
        let out: Result<i32> = retry_once_on_corruption(
            || {
                attempts.set(attempts.get() + 1);
                Err(corrupt_err())
            },
            || {
                evicts.set(evicts.get() + 1);
                Ok(())
            },
        );
        assert!(out.is_err(), "a second corruption must propagate");
        assert!(
            is_cache_corruption(&out.unwrap_err()),
            "the propagated error must still be the corruption-class error"
        );
        assert_eq!(attempts.get(), 2, "exactly one retry, then propagate");
        assert_eq!(evicts.get(), 1, "eviction runs once, not per attempt");
    }

    #[test]
    fn retry_once_on_corruption_does_not_evict_on_non_corruption_error() {
        let attempts = Cell::new(0usize);
        let evicts = Cell::new(0usize);
        let out: Result<i32> = retry_once_on_corruption(
            || {
                attempts.set(attempts.get() + 1);
                Err(anyhow::anyhow!("lock contention or similar"))
            },
            || {
                evicts.set(evicts.get() + 1);
                Ok(())
            },
        );
        assert!(out.is_err());
        assert_eq!(attempts.get(), 1, "a non-corruption error must not retry");
        assert_eq!(evicts.get(), 0, "a non-corruption error must not evict");
    }

    /// Build + populate a cache for a fresh temp vault, then drop it (checkpoints
    /// and removes the WAL/SHM sidecars). Returns the vault root, its tempdir
    /// guard, and the resolved `cache.db` path.
    fn build_populated_cache() -> (tempfile::TempDir, camino::Utf8PathBuf, camino::Utf8PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let base = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        // A named `vault/` subdir, not the tempdir root: `TempDir` names itself
        // `.tmpXXXX` and the scanner treats a hidden root as ignored (walks zero
        // files), so building at the root would populate an empty cache.
        let root = base.join("vault");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.md"), "---\ntitle: A\n---\n# A\nbody\n").unwrap();
        std::fs::write(root.join("b.md"), "# B\n[[a]]\n").unwrap();

        let options = IndexOptions::default();
        let mut cache = Cache::open_with_index(
            &root,
            None,
            &[],
            &options.resolved_index_set,
            &options.resolved_index_set_hash,
        )
        .unwrap();
        cache.rebuild(&root).unwrap();
        let db_path = cache.cache_dir.join("cache.db");
        drop(cache);
        // Last connection closed → main db is authoritative; drop any sidecars.
        let _ = std::fs::remove_file(db_path.with_extension("db-wal").as_std_path());
        let _ = std::fs::remove_file(db_path.with_extension("db-shm").as_std_path());
        (tmp, root, db_path)
    }

    #[test]
    fn load_graph_index_rebuilds_on_data_page_corruption() {
        use std::io::{Seek, SeekFrom, Write};

        let (_tmp, root, db_path) = build_populated_cache();

        // Zero the `documents` table's b-tree root page. The `meta` rows stay
        // readable, so the trusting open's inspect REUSES the cache (skipping the
        // integrity_check that used to catch this), and the corruption instead
        // surfaces when the refresh/load reads the malformed `documents` page.
        let (page_size, rootpage): (i64, i64) = {
            let conn = rusqlite::Connection::open(db_path.as_std_path()).unwrap();
            let page_size = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .unwrap();
            let rootpage = conn
                .query_row(
                    "SELECT rootpage FROM sqlite_master WHERE name = 'documents'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            (page_size, rootpage)
        };
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(db_path.as_std_path())
                .unwrap();
            f.seek(SeekFrom::Start(((rootpage - 1) * page_size) as u64))
                .unwrap();
            f.write_all(&vec![0u8; page_size as usize]).unwrap();
            f.sync_all().unwrap();
        }

        // The read must evict → rebuild → retry and succeed with the real docs.
        let options = IndexOptions::default();
        let index = load_graph_index(&root, &options, false).unwrap();
        assert!(
            index.documents.len() >= 2,
            "rebuilt index must be repopulated from the vault, got {} docs",
            index.documents.len()
        );
    }

    #[test]
    fn load_graph_index_rebuilds_on_gross_scribble() {
        let (_tmp, root, db_path) = build_populated_cache();

        // Whole-file garbage: the header/meta are unreadable, so the open's
        // inspect catches it (schema/identity SELECTs fail) and rebuilds — the
        // trusting path still self-heals gross corruption without integrity_check.
        std::fs::write(
            db_path.as_std_path(),
            b"this is not a sqlite database at all",
        )
        .unwrap();

        let options = IndexOptions::default();
        let index = load_graph_index(&root, &options, false).unwrap();
        assert!(
            index.documents.len() >= 2,
            "gross corruption must still self-heal into a repopulated index, got {} docs",
            index.documents.len()
        );
    }
}
