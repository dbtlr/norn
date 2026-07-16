//! Cross-vault GC for the global cache and state trees.
//!
//! Scans `<XDG_CACHE_HOME>/norn/` and `<XDG_STATE_HOME>/norn/` for per-vault
//! hash entries, recovers each entry's recorded vault root, classifies, and
//! evicts. Cache entries are disposable (evicted on any signal); state entries
//! hold the append-only mutation event stream and are evicted only when their
//! vault root no longer exists (or the entry is empty).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Serialize, Serializer};

/// Internal, non-configurable total-size cap for the cache tree (1 GiB).
pub(crate) const CACHE_TREE_SIZE_CAP_BYTES: u64 = 1024 * 1024 * 1024;
/// Lazy-sweep throttle: at most one sweep per interval.
pub(crate) const PRUNE_THROTTLE: Duration = Duration::from_secs(24 * 3_600);
/// Stale-database TTL (48h). Every database location under an entry EXCEPT the
/// live current-schema one (`<entry>/v{current}`) is aged on its own clock and
/// evicted past this window (NRN-286): dev databases of any schema (NRN-269),
/// live databases of non-current schema versions (`<entry>/vN`, N != current),
/// and legacy bare `cache.db` files at either channel root. The whole entry's
/// freshness would otherwise span every channel and schema, so a stale ~29MB
/// build or an obsolete-schema db inside a live-active entry would never age
/// out. Shorter than the entry retention window because these all rebuild
/// cheaply from source.
pub(crate) const STALE_DB_TTL: Duration = Duration::from_secs(48 * 3_600);
/// Marker file (in the cache tree root) recording the last lazy-sweep decision.
pub(crate) const PRUNE_MARKER: &str = ".last-prune";

/// Record a sweep decision: create/refresh the marker file's mtime. Best-effort.
pub(crate) fn touch_marker(cache_tree: &Utf8Path) {
    let _ = std::fs::create_dir_all(cache_tree.as_std_path());
    let marker = cache_tree.join(PRUNE_MARKER);
    let _ = std::fs::write(marker.as_std_path(), b"");
}

/// Per-invocation lazy GC. Best-effort end to end: never errors, never
/// fails the command. Steady-state cost is one stat (the marker check
/// runs before any config load). If the marker can't be written (e.g.
/// read-only cache home), the config probe repeats each invocation.
pub(crate) fn lazy_sweep(cwd: &Utf8PathBuf, config_path: Option<&Utf8PathBuf>) {
    let Ok(cache_tree) = crate::cache::cache_tree_root() else {
        return;
    };
    let marker = cache_tree.join(PRUNE_MARKER);
    if let Ok(md) = std::fs::metadata(marker.as_std_path()) {
        // Marker exists: sweep only if it is verifiably stale. An unreadable
        // mtime or clock skew biases toward skipping — a missed sweep
        // self-corrects within one throttle interval; a runaway sweep doesn't.
        let stale = md
            .modified()
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map(|d| d >= PRUNE_THROTTLE)
            .unwrap_or(false);
        if !stale {
            return;
        }
    }
    // Marker stale or absent: load this vault's cache config, best-effort.
    let cache_cfg = crate::config_loader::load_config(cwd, config_path)
        .ok()
        .and_then(|c| c.vault_config.cache);
    let enabled = cache_cfg
        .as_ref()
        .map(|c| c.lazy_prune_enabled())
        .unwrap_or(true);
    if enabled {
        let Ok(state_tree) = crate::cache::state_tree_root() else {
            // Enabled but no state tree: degenerate env; retries next invocation.
            return;
        };
        // Channel-resolution failure (invalid NORN_CACHE_CHANNEL): skip the
        // sweep this invocation rather than protect the wrong own-db path.
        let Ok(exempt_own_db_subpath) = own_db_subpath() else {
            return;
        };
        let opts = PruneOptions {
            retention: cache_cfg
                .as_ref()
                .and_then(|c| c.retention)
                .unwrap_or(crate::standards::DEFAULT_CACHE_RETENTION),
            cap_bytes: CACHE_TREE_SIZE_CAP_BYTES,
            dry_run: false,
            exempt_hash: crate::cache::vault_identity_hash(cwd),
            exempt_own_db_subpath,
        };
        let report = sweep(&cache_tree, &state_tree, &opts);
        if report.cache.skipped_locked > 0 {
            eprintln!(
                "warn: cache prune skipped {} locked entr{}",
                report.cache.skipped_locked,
                if report.cache.skipped_locked == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
        }
    }
    // Touched after sweep OR manual-skip: manual vaults also pay at most
    // one config probe per throttle interval.
    touch_marker(&cache_tree);
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum EvictReason {
    DeadRoot,
    Unreadable,
    Empty,
    Aged,
    OverCap,
    StaleDb,
}

#[derive(Debug, Serialize)]
pub(crate) struct EvictedEntry {
    /// Recorded canonical vault root, when recoverable.
    pub root: Option<String>,
    pub hash: String,
    pub reason: EvictReason,
    /// Age of the entry in whole days (newest-mtime metric), when known.
    pub age_days: Option<u64>,
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct TreeReport {
    pub scanned: usize,
    pub evicted: Vec<EvictedEntry>,
    pub skipped_locked: usize,
    /// Entries an eviction attempt failed on for a non-lock reason
    /// (permissions, IO); distinct entries, like `skipped_locked`.
    pub skipped_errors: usize,
    pub kept_unknown: usize,
    pub bytes_freed: u64,
}

#[derive(Debug)]
pub(crate) struct PruneReport {
    pub dry_run: bool,
    pub cache: TreeReport,
    pub state: TreeReport,
}

impl PruneReport {
    pub fn total_bytes_freed(&self) -> u64 {
        self.cache.bytes_freed + self.state.bytes_freed
    }
}

impl Serialize for PruneReport {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("PruneReport", 4)?;
        s.serialize_field("dry_run", &self.dry_run)?;
        s.serialize_field("cache", &self.cache)?;
        s.serialize_field("state", &self.state)?;
        s.serialize_field("total_bytes_freed", &self.total_bytes_freed())?;
        s.end()
    }
}

pub(crate) struct PruneOptions {
    /// Age-eviction window measured against the newest-mtime metric; cache tree only.
    pub retention: Duration,
    /// Cache-tree total-size cap; evict oldest-first until the tree is under this limit.
    pub cap_bytes: u64,
    pub dry_run: bool,
    /// Hash of the running command's own vault entry; the entry-level passes
    /// (dead-root/aged/empty/cap) never evict it.
    pub exempt_hash: Option<String>,
    /// Relative subpath (from a vault entry dir) of the INVOKING binary's own
    /// current cache db dir — `v{schema}` on live, `dev/v{schema}` on dev. In
    /// the exempt entry the stale-db pass protects exactly this db while still
    /// reaping the entry's OTHER stale dbs (old schema, legacy bare, peer
    /// channel); see [`own_db_subpath`].
    pub exempt_own_db_subpath: Utf8PathBuf,
}

/// The relative subpath (from a vault entry dir) of the INVOKING binary's own
/// current cache db dir — `v{schema}` on live, `dev/v{schema}` on dev. The
/// stale-db pass protects this path in the exempt (current-invocation) entry so
/// a single-vault user's own in-use db is never reaped, while its leftover
/// stale dbs (old schema, legacy bare, peer channel) still age out on the 48h
/// clock.
pub(crate) fn own_db_subpath() -> Result<Utf8PathBuf, crate::cache::error::CacheError> {
    let seg = crate::cache::identity::schema_segment();
    // Propagate a channel-resolution failure (an invalid `NORN_CACHE_CHANNEL`)
    // rather than guessing the live layout: guessing would protect the wrong
    // path and leave the invoking binary's real own db unprotected. `run_prune`
    // surfaces the error; `lazy_sweep` skips the sweep (best-effort posture).
    let ch = crate::cache::channel::channel()?;
    Ok(match ch.db_subdir() {
        Some(sub) => Utf8Path::new(sub).join(seg),
        None => Utf8PathBuf::from(seg),
    })
}

/// One scanned tree entry with everything classification needs.
struct Entry {
    hash: String,
    dir: Utf8PathBuf,
    root: Option<String>,
    bytes: u64,
    newest_mtime: Option<SystemTime>,
    file_count: usize,
}

fn is_entry_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Lock files are coordination artifacts, not content; an entry holding only
/// locks is empty for GC purposes.
fn is_lock_file(name: &str) -> bool {
    name == ".lock" || name == ".mutation.lock"
}

/// Recursive size / newest-mtime / file-count walk. Best-effort: unreadable
/// children contribute nothing. norn's own lock files contribute bytes but
/// are excluded from the file count AND from newest-mtime — a lock file's
/// mtime is locking metadata, not cache freshness, and any flock acquisition
/// (including the sweep's own) perturbs it.
fn measure(dir: &Utf8Path) -> (u64, Option<SystemTime>, usize) {
    measure_excluding(dir, &[])
}

/// `measure`, skipping any file at or under one of `excluded` (each an exact
/// file path or a directory prefix). Backs dry-run projection of an entry's
/// post-eviction measure without touching disk — the stale-db TTL pass passes
/// the schema dirs / legacy db files it would remove so a dry run classifies
/// and accounts exactly like a real sweep.
fn measure_excluding(dir: &Utf8Path, excluded: &[Utf8PathBuf]) -> (u64, Option<SystemTime>, usize) {
    let mut bytes = 0u64;
    let mut newest: Option<SystemTime> = None;
    let mut files = 0usize;
    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return (0, None, 0);
    };
    for e in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(e.path()) else {
            continue;
        };
        if excluded
            .iter()
            .any(|ex| path == *ex || path.starts_with(ex))
        {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            let (b, m, f) = measure_excluding(&path, excluded);
            bytes += b;
            newest = max_opt(newest, m);
            files += f;
        } else {
            bytes += md.len();
            if !e.file_name().to_str().is_some_and(is_lock_file) {
                files += 1;
                newest = max_opt(newest, md.modified().ok());
            }
        }
    }
    (bytes, newest, files)
}

fn max_opt(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (x, None) | (None, x) => x,
    }
}

/// Recover `meta.vault_root` for an entry, read-only, by probing candidate db
/// paths in a fixed, documented order and returning the first that reads
/// (NRN-286):
///
///   1. current live schema dir  — `<entry>/v{current}/cache.db`
///   2. other live schema dirs   — `<entry>/vN/cache.db` (N != current), highest N first
///   3. legacy live bare db      — `<entry>/cache.db`
///   4. current dev schema dir   — `<entry>/dev/v{current}/cache.db`
///   5. other dev schema dirs    — `<entry>/dev/vN/cache.db`, highest N first
///   6. legacy dev bare db       — `<entry>/dev/cache.db`
///
/// Policy on corruption (NRN-269 spirit, relaxed): the candidate list is
/// bounded, so a present-but-corrupt db in a preferred slot does NOT strand the
/// whole entry as `Unreadable` — it simply falls through to the next candidate.
/// This is safe because the whole entry dir is removed together on eviction, so
/// every channel and schema ages as one unit, and there is no risk of endless
/// fallback opens.
fn read_cache_root(entry_dir: &Utf8Path) -> Option<String> {
    cache_root_candidates(entry_dir)
        .into_iter()
        .find_map(|db| read_root_from_db(&db))
}

/// The ordered `meta.vault_root` recovery candidates for an entry — see
/// [`read_cache_root`] for the order and rationale.
fn cache_root_candidates(entry_dir: &Utf8Path) -> Vec<Utf8PathBuf> {
    let current = crate::cache::identity::schema_segment();
    let dev = entry_dir.join(dev_subdir());
    let mut out = Vec::new();
    // 1 + 2: live channel, current schema first then other schemas.
    out.push(entry_dir.join(&current).join("cache.db"));
    for dir in other_schema_dirs(entry_dir, Some(&current)) {
        out.push(dir.join("cache.db"));
    }
    // 3: legacy bare live db.
    out.push(entry_dir.join("cache.db"));
    // 4 + 5: dev channel, current schema first then other schemas.
    out.push(dev.join(&current).join("cache.db"));
    for dir in other_schema_dirs(&dev, Some(&current)) {
        out.push(dir.join("cache.db"));
    }
    // 6: legacy bare dev db.
    out.push(dev.join("cache.db"));
    out
}

fn read_root_from_db(db: &Utf8Path) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        db.as_std_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'vault_root'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Recover a state entry's vault root: cache twin first, then the first line
/// of the newest events-*.jsonl (`Attributes."norn.vault_root"`).
fn read_state_root(
    entry_dir: &Utf8Path,
    cache_roots: &HashMap<String, String>,
    hash: &str,
) -> Option<String> {
    if let Some(root) = cache_roots.get(hash) {
        return Some(root.clone());
    }
    let events = entry_dir.join("events");
    // date-stamped names: lexicographic == chronological
    let newest = std::fs::read_dir(events.as_std_path())
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .filter(|n| n.starts_with("events-") && n.ends_with(".jsonl"))
        .max()?;
    let file = std::fs::File::open(events.join(&newest).as_std_path()).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut first = String::new();
    std::io::BufRead::read_line(&mut reader, &mut first).ok()?;
    if first.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(first.trim_end()).ok()?;
    v.get("Attributes")?
        .get("norn.vault_root")?
        .as_str()
        .map(str::to_owned)
}

fn scan_tree(tree: &Utf8Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(tree.as_std_path()) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            if !is_entry_hash(&name) || !e.metadata().ok()?.is_dir() {
                return None;
            }
            let dir = Utf8PathBuf::from_path_buf(e.path()).ok()?;
            let (bytes, newest_mtime, file_count) = measure(&dir);
            Some(Entry {
                hash: name,
                dir,
                root: None,
                bytes,
                newest_mtime,
                file_count,
            })
        })
        .collect();
    out.sort_by(|a, b| a.hash.cmp(&b.hash));
    out
}

fn root_is_dead(root: &str) -> bool {
    !std::path::Path::new(root).exists()
}

fn age_days(newest: Option<SystemTime>, now: SystemTime) -> Option<u64> {
    newest
        .and_then(|m| now.duration_since(m).ok())
        .map(|d| d.as_secs() / 86_400)
}

/// The three files that make up a single SQLite cache database.
const DB_FILES: [&str; 3] = ["cache.db", "cache.db-wal", "cache.db-shm"];

/// The dev-channel subdir name, sourced from the channel definition so prune's
/// path construction can never drift from the authoritative layout.
fn dev_subdir() -> &'static str {
    crate::cache::channel::Channel::Dev
        .db_subdir()
        .expect("the dev channel always nests under a subdir")
}

/// Whether `dir` holds any file of a cache database — `cache.db` OR a
/// crash-orphaned `-wal`/`-shm` sidecar left without its main db. A dir with
/// only an orphaned sidecar is still a reclaimable stale-db location; its bytes
/// must not pin the entry or count against the cap forever.
fn has_any_db_file(dir: &Utf8Path) -> bool {
    DB_FILES.iter().any(|n| dir.join(n).as_std_path().exists())
}

/// A schema-dir name is exactly `v` + one or more decimal digits (`v5`, `v12`).
/// Returns the parsed version, or `None` for anything else. Strict on purpose:
/// only genuine schema dirs are recognized as stale-db locations or root-
/// recovery candidates.
fn schema_version_of(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('v')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Schema dirs directly under `root`, highest version first (deterministic),
/// gated on holding any db file. `exempt` names one schema dir to omit (the
/// current one for the live channel; `None` to include every schema, e.g. under
/// the dev channel). A missing `root` yields empty.
fn other_schema_dirs(root: &Utf8Path, exempt: Option<&str>) -> Vec<Utf8PathBuf> {
    let Ok(rd) = std::fs::read_dir(root.as_std_path()) else {
        return Vec::new();
    };
    let mut dirs: Vec<(u32, Utf8PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            if exempt == Some(name.as_str()) {
                return None;
            }
            let v = schema_version_of(&name)?;
            let dir = Utf8PathBuf::from_path_buf(e.path()).ok()?;
            if !has_any_db_file(&dir) {
                return None;
            }
            Some((v, dir))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    dirs.into_iter().map(|(_, d)| d).collect()
}

/// One removable stale-database unit inside an entry (NRN-286).
enum StaleDb {
    /// A whole schema directory (`<entry>/vN` or `<entry>/dev/vN`); the whole
    /// dir is the removable unit.
    SchemaDir(Utf8PathBuf),
    /// A legacy bare `cache.db` at a channel root (`<entry>` or `<entry>/dev`);
    /// only the db + WAL/SHM sidecars are removed, never the root (which holds
    /// the shared lock and/or other dbs).
    LegacyBare(Utf8PathBuf),
}

impl StaleDb {
    /// The directory that directly contains this unit's `cache.db`.
    fn db_root(&self) -> &Utf8Path {
        match self {
            StaleDb::SchemaDir(d) => d,
            StaleDb::LegacyBare(root) => root,
        }
    }

    /// Newest mtime among this unit's db files, gated on any db file existing —
    /// `None` when neither the main db NOR a `-wal`/`-shm` sidecar is present.
    /// A crash-orphaned sidecar (no `cache.db`) still ages and is reclaimable.
    fn newest_mtime(&self) -> Option<SystemTime> {
        let root = self.db_root();
        if !has_any_db_file(root) {
            return None;
        }
        let mut newest: Option<SystemTime> = None;
        for name in DB_FILES {
            if let Ok(md) = std::fs::metadata(root.join(name).as_std_path()) {
                newest = max_opt(newest, md.modified().ok());
            }
        }
        newest
    }

    /// Total on-disk bytes of this unit.
    fn bytes(&self) -> u64 {
        match self {
            StaleDb::SchemaDir(d) => measure(d).0,
            StaleDb::LegacyBare(root) => DB_FILES
                .iter()
                .filter_map(|n| std::fs::metadata(root.join(n).as_std_path()).ok())
                .map(|md| md.len())
                .sum(),
        }
    }

    /// Paths a dry run must exclude to project the entry's post-removal measure:
    /// the schema dir itself (as a prefix) or each legacy sidecar file.
    fn excluded_paths(&self) -> Vec<Utf8PathBuf> {
        match self {
            StaleDb::SchemaDir(d) => vec![d.clone()],
            StaleDb::LegacyBare(root) => DB_FILES.iter().map(|n| root.join(n)).collect(),
        }
    }

    /// Remove this unit from disk. Returns whether ALL of the unit's db files
    /// are gone afterward (each removed or already absent) — a failed removal of
    /// the main db OR any `-wal`/`-shm` sidecar returns `false`, so the caller
    /// records an error skip and no bytes-freed row, keeping accounting honest
    /// (a sidecar left on disk was never freed). Never removes the entry, the
    /// shared lock, or another db; an emptied non-entry channel dir (`dev/`) is
    /// cleaned up opportunistically.
    fn remove(&self, entry_dir: &Utf8Path) -> bool {
        match self {
            StaleDb::SchemaDir(d) => {
                let removed = std::fs::remove_dir_all(d.as_std_path()).is_ok();
                // Drop a now-empty channel dir (e.g. `dev/`) — never the entry
                // itself. `remove_dir` only succeeds on an empty dir, so a dir
                // still holding another schema stays.
                if let Some(parent) = d.parent() {
                    if parent != entry_dir {
                        let _ = std::fs::remove_dir(parent.as_std_path());
                    }
                }
                removed
            }
            StaleDb::LegacyBare(root) => {
                // All-or-nothing over the db + its sidecars, matching
                // `remove_dir_all`'s semantics for the SchemaDir arm: any file
                // that resists deletion (and isn't already gone) fails the unit.
                let mut removed = true;
                for name in DB_FILES {
                    match std::fs::remove_file(root.join(name).as_std_path()) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => removed = false,
                    }
                }
                // Drop a now-empty channel dir (e.g. `dev/`) — never the entry
                // itself. `remove_dir` only succeeds on an empty dir, so a root
                // still holding other content stays.
                if root != entry_dir {
                    let _ = std::fs::remove_dir(root.as_std_path());
                }
                removed
            }
        }
    }
}

/// Every stale-database unit inside an entry: all db locations EXCEPT the live
/// current-schema dir (`<entry>/v{current}`). Covers live non-current schema
/// dirs, dev schema dirs of any version, and legacy bare `cache.db` files at
/// either channel root.
fn scan_stale_dbs(entry_dir: &Utf8Path) -> Vec<StaleDb> {
    let current = crate::cache::identity::schema_segment();
    let dev = entry_dir.join(dev_subdir());
    let mut out = Vec::new();
    // Live channel: non-current schema dirs (the live current one is exempt).
    out.extend(
        other_schema_dirs(entry_dir, Some(&current))
            .into_iter()
            .map(StaleDb::SchemaDir),
    );
    // Legacy bare live db (including a crash-orphaned sidecar).
    if has_any_db_file(entry_dir) {
        out.push(StaleDb::LegacyBare(entry_dir.to_owned()));
    }
    // Dev channel: schema dirs of ANY version (None exempts nothing).
    out.extend(
        other_schema_dirs(&dev, None)
            .into_iter()
            .map(StaleDb::SchemaDir),
    );
    // Legacy bare dev db (including a crash-orphaned sidecar).
    if has_any_db_file(&dev) {
        out.push(StaleDb::LegacyBare(dev));
    }
    out
}

/// Count a skipped entry toward `counter` exactly once per sweep: the stale-db
/// TTL pass and the entry pass can both fail on the same entry, and the counters
/// report distinct entries, not attempts. `skipped_locked` is reserved for a
/// genuinely held lock (WouldBlock); `skipped_errors` covers everything else
/// (a lock file we can't open, a removal that failed).
fn mark_skipped(counter: &mut usize, seen: &mut HashSet<String>, hash: &str) {
    if seen.insert(hash.to_owned()) {
        *counter += 1;
    }
}

/// Per-sweep dedup sets backing `skipped_locked` / `skipped_errors`.
#[derive(Default)]
struct SkipSets {
    locked: HashSet<String>,
    errored: HashSet<String>,
}

/// Whether a db-file mtime has aged past [`STALE_DB_TTL`]. The single source of
/// the TTL comparison — every stale-db age check routes through here.
fn mtime_past_stale_ttl(mtime: SystemTime, now: SystemTime) -> bool {
    now.duration_since(mtime)
        .map(|age| age > STALE_DB_TTL)
        .unwrap_or(false)
}

/// Whether a stale-db unit's newest mtime has aged past [`STALE_DB_TTL`].
fn stale_db_past_ttl(sdb: &StaleDb, now: SystemTime) -> bool {
    sdb.newest_mtime()
        .is_some_and(|m| mtime_past_stale_ttl(m, now))
}

/// Generalized stale-database TTL pass (NRN-286), replacing the dev-only pass.
/// Every db location under an entry EXCEPT the live current-schema dir is aged
/// independently at [`STALE_DB_TTL`] and removed when past it, holding the
/// shared entry `.lock` (the same zero-timeout flock `evict_cache_entry` uses)
/// so an in-flight writer blocks the eviction. Only the stale db's dir/files
/// are removed — never the live current-schema db, the shared `.lock`, nor the
/// entry itself. The in-memory `Entry` is re-measured (real run) or projected
/// without touching disk (dry-run) so the remaining classification passes see
/// the shrunk entry identically on both paths: a now-empty entry falls through
/// to the Empty policy this same sweep, and stale bytes leave the cap-pass
/// survivor total. A held lock is a locked skip; a lock we can't open or a
/// failed removal is an error skip — each counted once per entry.
///
/// `protect` names one db dir to never treat as stale — the INVOKING binary's
/// own current db in the exempt entry (NRN-286): the stale-db pass runs on the
/// exempt entry too (so a single-vault user's leftover old-schema / legacy dbs
/// still reclaim), but must never reap the db that invocation is actively using.
fn evict_stale_dbs(
    report: &mut TreeReport,
    entry: &mut Entry,
    now: SystemTime,
    dry_run: bool,
    skips: &mut SkipSets,
    protect: Option<&Utf8Path>,
) {
    let stale: Vec<StaleDb> = scan_stale_dbs(&entry.dir)
        .into_iter()
        .filter(|s| protect.is_none_or(|p| p != s.db_root()))
        .collect();
    if stale.is_empty() {
        return;
    }

    if dry_run {
        // Pre-lock sample is authoritative for a preview (it races live writers
        // by nature). Project the entry's measure as if every past-TTL unit
        // were gone, so dry-run classifies and accounts exactly like a real run.
        let mut excluded: Vec<Utf8PathBuf> = Vec::new();
        for sdb in &stale {
            let Some(mtime) = sdb.newest_mtime() else {
                continue;
            };
            if mtime_past_stale_ttl(mtime, now) {
                let bytes = sdb.bytes();
                report.bytes_freed += bytes;
                report.evicted.push(EvictedEntry {
                    root: entry.root.clone(),
                    hash: entry.hash.clone(),
                    reason: EvictReason::StaleDb,
                    age_days: age_days(Some(mtime), now),
                    bytes,
                });
                excluded.extend(sdb.excluded_paths());
            }
        }
        if !excluded.is_empty() {
            let (bytes, newest, files) = measure_excluding(&entry.dir, &excluded);
            entry.bytes = bytes;
            entry.newest_mtime = newest;
            entry.file_count = files;
        }
        return;
    }

    // Real run: skip locking entirely when no unit is a candidate pre-lock.
    if !stale.iter().any(|s| stale_db_past_ttl(s, now)) {
        return;
    }
    match crate::cache::acquire_flock(&entry.dir.join(".lock"), Duration::ZERO) {
        Ok(_held) => {
            let mut had_error = false;
            for sdb in &stale {
                // TOCTOU guard: a writer can refresh a db between the pre-lock
                // sample and acquisition. Re-sample under the lock and skip this
                // unit if it is now gone or no longer past the TTL.
                let Some(mtime) = sdb.newest_mtime() else {
                    continue;
                };
                if mtime_past_stale_ttl(mtime, now) {
                    let bytes = sdb.bytes();
                    if sdb.remove(&entry.dir) {
                        report.bytes_freed += bytes;
                        report.evicted.push(EvictedEntry {
                            root: entry.root.clone(),
                            hash: entry.hash.clone(),
                            reason: EvictReason::StaleDb,
                            age_days: age_days(Some(mtime), now),
                            bytes,
                        });
                    } else {
                        had_error = true;
                    }
                }
            }
            // Re-measure actual disk state whether or not every removal fully
            // succeeded: a failed remove can still have deleted part of a
            // subtree, and later passes must see the truth.
            let (bytes, newest, files) = measure(&entry.dir);
            entry.bytes = bytes;
            entry.newest_mtime = newest;
            entry.file_count = files;
            if had_error {
                mark_skipped(&mut report.skipped_errors, &mut skips.errored, &entry.hash);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            mark_skipped(&mut report.skipped_locked, &mut skips.locked, &entry.hash);
        }
        // A lock we can't even open (permissions, IO): error skip.
        Err(_) => {
            mark_skipped(&mut report.skipped_errors, &mut skips.errored, &entry.hash);
        }
    }
}

/// Try-evict one cache entry: skip if its write lock is held. Returns
/// Ok(true) on eviction, Ok(false) on a genuinely held lock (WouldBlock),
/// Err for any other failure (a lock we can't open, a removal that failed);
/// the sweep stays best-effort and never escalates, but reports the two
/// skip kinds separately.
fn evict_cache_entry(entry: &Entry) -> std::io::Result<bool> {
    match crate::cache::acquire_flock(&entry.dir.join(".lock"), Duration::ZERO) {
        Ok(_held) => {
            std::fs::remove_dir_all(entry.dir.as_std_path())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) fn sweep(
    cache_tree: &Utf8Path,
    state_tree: &Utf8Path,
    opts: &PruneOptions,
) -> PruneReport {
    let now = SystemTime::now();
    let mut cache_entries = scan_tree(cache_tree);
    for e in &mut cache_entries {
        e.root = read_cache_root(&e.dir);
    }
    let cache_roots: HashMap<String, String> = cache_entries
        .iter()
        .filter_map(|e| e.root.clone().map(|r| (e.hash.clone(), r)))
        .collect();
    let mut state_entries = scan_tree(state_tree);
    for e in &mut state_entries {
        e.root = read_state_root(&e.dir, &cache_roots, &e.hash);
    }

    let exempt = |e: &Entry| opts.exempt_hash.as_deref() == Some(e.hash.as_str());

    // ---- cache tree ----
    let mut cache_report = TreeReport {
        scanned: cache_entries.len(),
        ..Default::default()
    };
    // Entries an eviction attempt failed on this sweep; keeps the skip
    // counters reporting distinct entries, not attempts.
    let mut skips = SkipSets::default();
    // Stale-database TTL pass (NRN-286): independently of the entry's own
    // freshness, evict every db location whose newest mtime has aged past
    // STALE_DB_TTL — dev dbs of any schema, live dbs of non-current schemas, and
    // legacy bare cache.db files. Runs regardless of the invoking binary's
    // channel. Unlike the entry-level passes below, it runs on the EXEMPT entry
    // too: a single-vault user always invokes prune against their own vault, so
    // skipping it would pin their leftover old-schema / legacy dbs forever. In
    // the exempt entry it protects only the invoking binary's OWN current db
    // (`opts.exempt_own_db_subpath`); every entry's live current-schema db is
    // excluded by `scan_stale_dbs` regardless. A now-empty entry falls through
    // to the Empty/dead-root policies below via the re-measure.
    for entry in &mut cache_entries {
        let protect = if exempt(entry) {
            Some(entry.dir.join(&opts.exempt_own_db_subpath))
        } else {
            None
        };
        evict_stale_dbs(
            &mut cache_report,
            entry,
            now,
            opts.dry_run,
            &mut skips,
            protect.as_deref(),
        );
    }
    let mut survivors: Vec<Entry> = Vec::new();
    for entry in cache_entries {
        if exempt(&entry) {
            survivors.push(entry);
            continue;
        }
        let reason = if entry.file_count == 0 {
            // Accepted race: a concurrent process can observe a just-created
            // entry dir before cache.db is written and its write lock is held,
            // classifying it as Empty and evicting it. The window is
            // milliseconds; the cost is one rebuild on next use. Accepted as
            // best-effort GC posture.
            Some(EvictReason::Empty)
        } else {
            match &entry.root {
                None => Some(EvictReason::Unreadable),
                Some(root) if root_is_dead(root) => Some(EvictReason::DeadRoot),
                // Duration-precision compare (NOT whole days): with
                // --retention 0d every non-exempt live entry is aged.
                Some(_) => match entry.newest_mtime.and_then(|m| now.duration_since(m).ok()) {
                    Some(age) if age > opts.retention => Some(EvictReason::Aged),
                    _ => None,
                },
            }
        };
        match reason {
            Some(reason) => evict_into(
                &mut cache_report,
                entry,
                reason,
                now,
                opts.dry_run,
                &mut skips,
            ),
            None => survivors.push(entry),
        }
    }
    // Cap pass: oldest-first over survivors (the exempt entry counts toward
    // the total but is never selected; the loop terminates when survivors
    // are exhausted, so an oversized exempt entry can't cause a futile loop).
    let mut total: u64 = survivors.iter().map(|e| e.bytes).sum();
    if total > opts.cap_bytes {
        // Oldest first; entries with no mtime sort oldest (None < Some); ties by hash.
        survivors.sort_by(|a, b| {
            a.newest_mtime
                .cmp(&b.newest_mtime)
                .then(a.hash.cmp(&b.hash))
        });
        for entry in survivors {
            if total <= opts.cap_bytes {
                break;
            }
            if exempt(&entry) {
                continue;
            }
            total -= entry.bytes;
            evict_into(
                &mut cache_report,
                entry,
                EvictReason::OverCap,
                now,
                opts.dry_run,
                &mut skips,
            );
        }
    }

    // ---- state tree ----
    let mut state_report = TreeReport {
        scanned: state_entries.len(),
        ..Default::default()
    };
    for entry in state_entries {
        if exempt(&entry) {
            continue;
        }
        if entry.file_count == 0 {
            evict_state_into(
                &mut state_report,
                entry,
                EvictReason::Empty,
                now,
                opts.dry_run,
            );
        } else {
            match &entry.root {
                None => state_report.kept_unknown += 1,
                Some(root) if root_is_dead(root) => evict_state_into(
                    &mut state_report,
                    entry,
                    EvictReason::DeadRoot,
                    now,
                    opts.dry_run,
                ),
                Some(_) => {}
            }
        }
    }

    PruneReport {
        dry_run: opts.dry_run,
        cache: cache_report,
        state: state_report,
    }
}

fn evict_into(
    report: &mut TreeReport,
    entry: Entry,
    reason: EvictReason,
    now: SystemTime,
    dry_run: bool,
    skips: &mut SkipSets,
) {
    if !dry_run {
        match evict_cache_entry(&entry) {
            Ok(true) => {}
            Ok(false) => {
                mark_skipped(&mut report.skipped_locked, &mut skips.locked, &entry.hash);
                return;
            }
            Err(_) => {
                mark_skipped(&mut report.skipped_errors, &mut skips.errored, &entry.hash);
                return;
            }
        }
    }
    push_evicted(report, entry, reason, now);
}

fn evict_state_into(
    report: &mut TreeReport,
    entry: Entry,
    reason: EvictReason,
    now: SystemTime,
    dry_run: bool,
) {
    if !dry_run && std::fs::remove_dir_all(entry.dir.as_std_path()).is_err() {
        return;
    }
    push_evicted(report, entry, reason, now);
}

fn push_evicted(report: &mut TreeReport, entry: Entry, reason: EvictReason, now: SystemTime) {
    report.bytes_freed += entry.bytes;
    report.evicted.push(EvictedEntry {
        root: entry.root,
        hash: entry.hash,
        reason,
        age_days: age_days(entry.newest_mtime, now),
        bytes: entry.bytes,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::{Utf8Path, Utf8PathBuf};
    use std::time::Duration;
    use tempfile::TempDir;

    fn schema_seg() -> String {
        crate::cache::identity::schema_segment()
    }

    /// Mint the CURRENT-schema cache db for a vault at
    /// `<tree>/<hash>/v{schema}/cache.db`, with a real schema and a
    /// `meta.vault_root` row pointing at `root`. Returns the ENTRY dir
    /// (`<tree>/<hash>`) — its `.lock` lives there and callers backdate/measure
    /// the whole entry. The db itself sits one `v{schema}` level below.
    fn mint_cache_entry(tree: &Utf8Path, hash: &str, root: &str) -> Utf8PathBuf {
        let entry = tree.join(hash);
        mint_db_at(&entry.join(schema_seg()), root);
        entry
    }

    /// Mint a real cache db (DDL + `meta.vault_root`) at `db_dir/cache.db`,
    /// creating `db_dir`. Used to plant dbs at arbitrary schema/channel/legacy
    /// locations inside an entry.
    fn mint_db_at(db_dir: &Utf8Path, root: &str) {
        std::fs::create_dir_all(db_dir.as_std_path()).unwrap();
        let conn = rusqlite::Connection::open(db_dir.join("cache.db").as_std_path()).unwrap();
        conn.execute_batch(crate::cache::schema::DDL).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('vault_root', ?)",
            rusqlite::params![root],
        )
        .unwrap();
    }

    fn mint_state_entry(tree: &Utf8Path, hash: &str, root: Option<&str>) -> Utf8PathBuf {
        let dir = tree.join(hash).join("events");
        std::fs::create_dir_all(dir.as_std_path()).unwrap();
        if let Some(root) = root {
            let line = serde_json::json!({"Attributes": {"norn.vault_root": root}});
            std::fs::write(dir.join("events-2026-06-01.jsonl"), format!("{line}\n")).unwrap();
        }
        dir.parent().unwrap().to_owned()
    }

    fn hashes(report: &[EvictedEntry]) -> Vec<&str> {
        report.iter().map(|e| e.hash.as_str()).collect()
    }

    fn entry_size(tree: &Utf8Path, hash: &str) -> u64 {
        measure(&tree.join(hash)).0
    }

    /// Backdate every file in `dir` (recursively) to `dur` in the past. Recurses
    /// so a db nested under a `v{schema}` segment is aged along with files
    /// written directly into the entry.
    fn backdate_dir(dir: &Utf8Path, dur: Duration) {
        let t = filetime::FileTime::from_system_time(std::time::SystemTime::now() - dur);
        for f in std::fs::read_dir(dir.as_std_path()).unwrap().flatten() {
            let path = f.path();
            if f.metadata().map(|m| m.is_dir()).unwrap_or(false) {
                if let Ok(sub) = Utf8PathBuf::from_path_buf(path.clone()) {
                    backdate_dir(&sub, dur);
                }
            } else {
                filetime::set_file_mtime(&path, t).unwrap();
            }
        }
    }

    const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const H3: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const H4: &str = "4444444444444444444444444444444444444444444444444444444444444444";

    fn opts(retention_days: u64) -> PruneOptions {
        PruneOptions {
            retention: Duration::from_secs(retention_days * 86_400),
            cap_bytes: u64::MAX,
            dry_run: false,
            exempt_hash: None,
            // Default: invoke as a live binary, so the exempt entry's own
            // current db is `v{schema}` (already excluded by scan_stale_dbs).
            exempt_own_db_subpath: schema_seg().into(),
        }
    }

    #[test]
    fn dead_root_entry_is_evicted_live_root_kept() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        mint_cache_entry(&cache_tree, H1, live_root);
        mint_cache_entry(&cache_tree, H2, "/nonexistent/vault/gone");
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.cache.evicted), vec![H2]);
        assert!(cache_tree.join(H1).as_std_path().exists());
        assert!(!cache_tree.join(H2).as_std_path().exists());
        assert!(report.cache.bytes_freed > 0);
    }

    /// The service dirs under the cache tree — `log/` (the launchd
    /// stdout/stderr sink, `norn service install`) and `run/` (the daemon's
    /// socket + lock) — are NOT vault entries and must survive a sweep even
    /// when every eviction pressure applies (zero retention, zero cap, dead
    /// entries all around). Their names are not 64-hex, so `is_entry_hash`
    /// filters them out of the scan; this guards that invariant.
    #[test]
    fn log_dir_under_the_cache_tree_survives_a_sweep() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        // A populated log/ dir (what `norn service install` provisions) and a
        // run/ dir (socket + lock), plus one genuinely-evictable dead entry so
        // the sweep demonstrably ran.
        let log_dir = cache_tree.join("log");
        std::fs::create_dir_all(log_dir.as_std_path()).unwrap();
        std::fs::write(log_dir.join("serve.log").as_std_path(), b"daemon output\n").unwrap();
        let run_dir = cache_tree.join("run");
        std::fs::create_dir_all(run_dir.as_std_path()).unwrap();
        std::fs::write(run_dir.join("norn.lock").as_std_path(), b"").unwrap();
        mint_cache_entry(&cache_tree, H1, "/nonexistent/vault/gone");

        // Maximum pressure: everything is over-age and over-cap.
        let report = sweep(
            &cache_tree,
            &state_tree,
            &PruneOptions {
                retention: Duration::ZERO,
                cap_bytes: 0,
                dry_run: false,
                exempt_hash: None,
                exempt_own_db_subpath: schema_seg().into(),
            },
        );

        assert_eq!(hashes(&report.cache.evicted), vec![H1], "the sweep ran");
        assert!(
            log_dir.join("serve.log").as_std_path().exists(),
            "the service log must survive a full-pressure prune"
        );
        assert!(
            run_dir.join("norn.lock").as_std_path().exists(),
            "the daemon run dir must survive a full-pressure prune"
        );
    }

    #[test]
    fn unreadable_and_empty_cache_entries_evicted() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        // Unreadable: a cache.db that is not a database.
        let dir = cache_tree.join(H1);
        std::fs::create_dir_all(dir.as_std_path()).unwrap();
        std::fs::write(dir.join("cache.db").as_std_path(), b"not sqlite").unwrap();
        // Empty: an entry dir with no files at all.
        std::fs::create_dir_all(cache_tree.join(H2).as_std_path()).unwrap();
        // Non-entry junk is ignored, not evicted.
        std::fs::write(cache_tree.join(".last-prune").as_std_path(), b"").unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        let mut got = hashes(&report.cache.evicted);
        got.sort();
        assert_eq!(got, vec![H1, H2]);
        assert!(cache_tree.join(".last-prune").as_std_path().exists());
        let h1_entry = report
            .cache
            .evicted
            .iter()
            .find(|e| e.hash == H1)
            .expect("H1 must be evicted");
        assert_eq!(h1_entry.reason, EvictReason::Unreadable);
        let h2_entry = report
            .cache
            .evicted
            .iter()
            .find(|e| e.hash == H2)
            .expect("H2 must be evicted");
        assert_eq!(h2_entry.reason, EvictReason::Empty);
    }

    /// An entry holding ONLY a dev-channel database (no live db) recovers its
    /// vault root from the dev schema dir (`<entry>/dev/v{schema}/cache.db`,
    /// candidate 4) and classifies normally — here dead-root, since the recorded
    /// vault is gone. Without candidate probing it would be misclassified
    /// `Unreadable`.
    #[test]
    fn dev_only_entry_classifies_by_its_dev_root() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        // <tree>/<H1>/dev/v{schema}/cache.db with a dead recorded root, no live db.
        let entry = cache_tree.join(H1);
        std::fs::create_dir_all(entry.as_std_path()).unwrap();
        mint_cache_entry(&entry, "dev", "/nonexistent/vault/gone");
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.cache.evicted), vec![H1]);
        assert_eq!(report.cache.evicted[0].reason, EvictReason::DeadRoot);
        assert_eq!(
            report.cache.evicted[0].root.as_deref(),
            Some("/nonexistent/vault/gone"),
            "the root must be recovered from the dev-channel db"
        );
    }

    /// NRN-286 (relaxed NRN-269 policy): a present-but-corrupt db in a preferred
    /// recovery slot must NOT strand the whole entry as `Unreadable` — the bounded
    /// candidate list simply falls through to the next readable db. Here the
    /// current live db (`v{schema}`, candidate 1) is corrupt and the legacy bare
    /// db (candidate 3) is healthy with a dead root, so the entry recovers that
    /// root and classifies dead-root, never Unreadable.
    #[test]
    fn corrupt_preferred_db_falls_through_to_next_candidate() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let entry = cache_tree.join(H1);
        // Corrupt current-schema live db (candidate 1)...
        std::fs::create_dir_all(entry.join(schema_seg()).as_std_path()).unwrap();
        std::fs::write(
            entry.join(schema_seg()).join("cache.db").as_std_path(),
            b"not sqlite",
        )
        .unwrap();
        // ...healthy legacy bare db recording a dead root (candidate 3).
        mint_db_at(&entry, "/nonexistent/vault/gone");
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.cache.evicted), vec![H1]);
        assert_eq!(
            report.cache.evicted[0].reason,
            EvictReason::DeadRoot,
            "a corrupt preferred db must fall through, not strand the entry Unreadable"
        );
        assert_eq!(
            report.cache.evicted[0].root.as_deref(),
            Some("/nonexistent/vault/gone")
        );
    }

    /// NRN-286: `read_cache_root` probes candidates in the documented order and
    /// returns the FIRST readable db's root — the current live schema dir wins
    /// over every other db present.
    #[test]
    fn root_recovery_probes_candidates_in_documented_order() {
        let trees = TempDir::new().unwrap();
        let entry = Utf8PathBuf::from_path_buf(trees.path().join(H1)).unwrap();
        // Current live schema dir (candidate 1) records the winning root.
        mint_db_at(&entry.join(schema_seg()), "/vault/current-live");
        // A legacy bare db (candidate 3) and a dev db (candidate 4) record other
        // roots that must NOT win while candidate 1 is readable.
        mint_db_at(&entry, "/vault/legacy-live");
        mint_db_at(&entry.join("dev").join(schema_seg()), "/vault/dev");
        assert_eq!(
            read_cache_root(&entry).as_deref(),
            Some("/vault/current-live"),
            "the current live schema dir must win the recovery order"
        );
    }

    #[test]
    fn aged_entry_evicted_recent_kept() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let old = mint_cache_entry(&cache_tree, H1, live_root);
        mint_cache_entry(&cache_tree, H2, live_root);
        // Backdate every file in H1 beyond the retention window.
        backdate_dir(&old, Duration::from_secs(100 * 86_400));
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.cache.evicted), vec![H1]);
        assert!(matches!(report.cache.evicted[0].reason, EvictReason::Aged));
    }

    #[test]
    fn cap_evicts_oldest_first_until_under() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        // Three live entries, each padded to ~1000 bytes, ages H1 oldest → H3 newest.
        for (i, h) in [H1, H2, H3].iter().enumerate() {
            let dir = mint_cache_entry(&cache_tree, h, live_root);
            std::fs::write(dir.join("pad").as_std_path(), vec![0u8; 1000]).unwrap();
            backdate_dir(&dir, Duration::from_secs((30 - i as u64 * 10) * 86_400));
            // 30d, 20d, 10d
        }
        // Entry sizes are dominated by the real cache.db (tens of KB), so
        // compute the cap from measured sizes, never from the pad constant:
        // cap = s3 + s2/2 forces evicting H1, then H2, then stopping.
        let s2 = entry_size(&cache_tree, H2);
        let s3 = entry_size(&cache_tree, H3);
        let mut o = opts(365); // age signal off (window wider than any fixture)
        o.cap_bytes = s3 + s2 / 2;
        let report = sweep(&cache_tree, &state_tree, &o);
        assert_eq!(hashes(&report.cache.evicted), vec![H1, H2]);
        assert!(matches!(
            report.cache.evicted[0].reason,
            EvictReason::OverCap
        ));
        assert!(cache_tree.join(H3).as_std_path().exists());
    }

    #[test]
    fn exempt_hash_never_evicted() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        mint_cache_entry(&cache_tree, H1, "/nonexistent/vault/gone");
        let mut o = opts(90);
        o.exempt_hash = Some(H1.to_string());
        let report = sweep(&cache_tree, &state_tree, &o);
        assert!(report.cache.evicted.is_empty());
        assert!(cache_tree.join(H1).as_std_path().exists());
    }

    #[test]
    fn exempt_hash_survives_cap_pass() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        // Three live entries, each padded to ~1000 bytes, ages H1 oldest → H3 newest.
        // H1 is the oldest — the cap pass would normally take it first.
        for (i, h) in [H1, H2, H3].iter().enumerate() {
            let dir = mint_cache_entry(&cache_tree, h, live_root);
            std::fs::write(dir.join("pad").as_std_path(), vec![0u8; 1000]).unwrap();
            backdate_dir(&dir, Duration::from_secs((30 - i as u64 * 10) * 86_400));
            // 30d, 20d, 10d
        }
        let s2 = entry_size(&cache_tree, H2);
        let s3 = entry_size(&cache_tree, H3);
        // cap forces the pass to evict entries until we drop below the cap.
        // With H1 exempt, the pass must take H2 (next oldest) instead.
        let mut o = opts(365); // retention wide: only the cap acts
        o.cap_bytes = s3 + s2 / 2;
        o.exempt_hash = Some(H1.to_string());
        let report = sweep(&cache_tree, &state_tree, &o);
        // H1 (exempt) must still exist on disk and must not appear in evicted.
        assert!(
            cache_tree.join(H1).as_std_path().exists(),
            "exempt H1 must survive cap pass"
        );
        assert!(
            report.cache.evicted.iter().all(|e| e.hash != H1),
            "H1 must not appear in evicted"
        );
        // H2 must be evicted with OverCap reason.
        let h2_entry = report
            .cache
            .evicted
            .iter()
            .find(|e| e.hash == H2)
            .expect("H2 must be evicted by cap pass");
        assert_eq!(h2_entry.reason, EvictReason::OverCap);
    }

    #[test]
    fn locked_entry_skipped() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let dir = mint_cache_entry(&cache_tree, H1, "/nonexistent/vault/gone");
        let _held = crate::cache::acquire_flock(&dir.join(".lock"), Duration::ZERO).unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert!(report.cache.evicted.is_empty());
        assert_eq!(report.cache.skipped_locked, 1);
        assert!(dir.as_std_path().exists());
    }

    #[test]
    fn state_tree_dead_root_evicted_unknown_kept_aged_kept() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        // Dead root via event-line recovery → evicted.
        mint_state_entry(&state_tree, H1, Some("/nonexistent/vault/gone"));
        // Unknown root (events dir exists but no event files) → kept + counted.
        mint_state_entry(&state_tree, H2, None);
        std::fs::write(
            state_tree
                .join(H2)
                .join("events")
                .join("not-an-event.txt")
                .as_std_path(),
            b"x",
        )
        .unwrap();
        // Live root recovered via cache twin (no event files needed) → kept,
        // even though its files are backdated beyond the retention window.
        // This exercises the "aged_kept" claim: state entries are never age-evicted.
        mint_cache_entry(&cache_tree, H3, live_root);
        mint_state_entry(&state_tree, H3, None);
        std::fs::write(
            state_tree
                .join(H3)
                .join("events")
                .join("keep.txt")
                .as_std_path(),
            b"x",
        )
        .unwrap();
        // Backdate every file under H3's state entry beyond the 90d retention window.
        backdate_dir(
            &state_tree.join(H3).join("events"),
            Duration::from_secs(100 * 86_400),
        );
        // Truly empty state entry → evicted.
        mint_state_entry(&state_tree, H4, None);
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        let mut got = hashes(&report.state.evicted);
        got.sort();
        assert_eq!(got, vec![H1, H4]);
        assert_eq!(report.state.kept_unknown, 1);
        assert!(state_tree.join(H2).as_std_path().exists());
        assert!(state_tree.join(H3).as_std_path().exists());
    }

    #[test]
    fn lock_only_state_entry_is_empty_and_evicted() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let dir = state_tree.join(H1);
        std::fs::create_dir_all(dir.as_std_path()).unwrap();
        std::fs::write(dir.join(".mutation.lock").as_std_path(), b"").unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.state.evicted), vec![H1]);
        assert_eq!(report.state.evicted[0].reason, EvictReason::Empty);
        assert_eq!(report.state.kept_unknown, 0);
        assert!(!dir.as_std_path().exists());
    }

    #[test]
    fn lock_only_cache_entry_is_empty_and_evicted() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let dir = cache_tree.join(H1);
        std::fs::create_dir_all(dir.as_std_path()).unwrap();
        std::fs::write(dir.join(".lock").as_std_path(), b"").unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.cache.evicted), vec![H1]);
        assert_eq!(report.cache.evicted[0].reason, EvictReason::Empty);
        assert!(!dir.as_std_path().exists());
    }

    #[test]
    fn dry_run_reports_but_deletes_nothing() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        mint_cache_entry(&cache_tree, H1, "/nonexistent/vault/gone");
        let mut o = opts(90);
        o.dry_run = true;
        let report = sweep(&cache_tree, &state_tree, &o);
        assert_eq!(hashes(&report.cache.evicted), vec![H1]);
        assert!(
            cache_tree.join(H1).as_std_path().exists(),
            "dry-run must not delete"
        );
        // Parity: the real run evicts exactly the dry-run set.
        let report2 = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report2.cache.evicted), vec![H1]);
        assert!(!cache_tree.join(H1).as_std_path().exists());
    }

    /// NRN-272/286 (a): a dev db aged past STALE_DB_TTL inside a live-active
    /// entry is evicted (`stale-db`) while the live current-schema db and shared
    /// `.lock` survive byte-identical and the entry itself is kept.
    #[test]
    fn stale_dev_db_evicted_live_db_and_lock_survive() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root);
        let live_db = entry.join(schema_seg()).join("cache.db");
        let live_bytes = std::fs::read(live_db.as_std_path()).unwrap();
        std::fs::write(entry.join(".lock").as_std_path(), b"live-lock").unwrap();
        // Dev db aged well past the 48h TTL.
        let dev = mint_cache_entry(&entry, "dev", live_root);
        backdate_dir(&dev, Duration::from_secs(3 * 86_400));

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        let dev_evict = report
            .cache
            .evicted
            .iter()
            .find(|e| e.hash == H1 && e.reason == EvictReason::StaleDb)
            .expect("stale dev db must be evicted as stale-db");
        assert!(dev_evict.bytes > 0);
        assert!(!entry.join("dev").as_std_path().exists(), "dev/ removed");
        assert!(live_db.as_std_path().exists(), "live db kept");
        assert_eq!(
            std::fs::read(live_db.as_std_path()).unwrap(),
            live_bytes,
            "live db byte-identical"
        );
        assert_eq!(
            std::fs::read(entry.join(".lock").as_std_path()).unwrap(),
            b"live-lock",
            "shared .lock byte-identical"
        );
        assert!(entry.as_std_path().exists(), "live-active entry survives");
    }

    /// NRN-272 (b): a fresh dev db (mtime within the TTL) is retained.
    #[test]
    fn fresh_dev_db_retained() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root);
        let dev = mint_cache_entry(&entry, "dev", live_root); // fresh
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert!(
            report
                .cache
                .evicted
                .iter()
                .all(|e| e.reason != EvictReason::StaleDb),
            "a fresh dev db must not be stale-db evicted"
        );
        assert!(
            dev.join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "fresh dev db retained"
        );
    }

    /// NRN-286 (supersedes NRN-272 (c)): the exempt (current-invocation) vault's
    /// stale non-own dbs ARE now reclaimed — only the invoking binary's OWN
    /// current db is protected. The old contract exempted the whole entry from
    /// the dev-TTL pass, which pinned a single-vault user's leftover dbs forever
    /// (they always invoke against the same, exempt, vault). Here a live invoker
    /// exempts its vault: its stale dev db is evicted, its own live current db
    /// survives.
    #[test]
    fn exempt_vault_stale_non_own_db_evicted_own_db_survives() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root); // own db: H1/v{schema}
        let dev = mint_cache_entry(&entry, "dev", live_root); // stale peer-channel db
        backdate_dir(&dev, Duration::from_secs(3 * 86_400));
        let mut o = opts(90); // exempt_own_db_subpath defaults to live v{schema}
        o.exempt_hash = Some(H1.to_string());
        let report = sweep(&cache_tree, &state_tree, &o);
        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "the exempt entry's stale dev db must now be reclaimed"
        );
        assert!(!entry.join("dev").as_std_path().exists(), "dev/ removed");
        assert!(
            entry
                .join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "the invoking binary's OWN current db survives"
        );
        assert!(entry.as_std_path().exists(), "the exempt entry survives");
    }

    /// NRN-286: the single-vault scenario the exemption change targets. A user
    /// always invokes prune against their own (exempt) vault; a leftover legacy
    /// bare `cache.db` aged past the TTL must reclaim rather than pin disk
    /// forever. The invoking binary's own current db survives.
    #[test]
    fn exempt_vault_legacy_bare_db_reclaimed_single_vault_case() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root); // own db: H1/v{schema}
        mint_db_at(&entry, live_root); // legacy bare H1/cache.db
        let legacy_db = entry.join("cache.db");
        let t = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(3 * 86_400),
        );
        filetime::set_file_mtime(legacy_db.as_std_path(), t).unwrap();
        let mut o = opts(90);
        o.exempt_hash = Some(H1.to_string()); // single-vault: own vault is exempt
        let report = sweep(&cache_tree, &state_tree, &o);
        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "the exempt vault's stale legacy bare db must be reclaimed"
        );
        assert!(!legacy_db.as_std_path().exists(), "legacy bare db removed");
        assert!(
            entry
                .join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "the invoking binary's OWN current db survives"
        );
    }

    /// NRN-272 (d): a dev-only entry whose dev db ages out is fully reclaimed —
    /// the stale-db eviction empties it and the existing Empty policy removes
    /// the entry on the same sweep.
    #[test]
    fn dev_only_entry_reclaimed_after_dev_ttl() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = cache_tree.join(H1);
        std::fs::create_dir_all(entry.as_std_path()).unwrap();
        // Dev-only: no live cache.db, dev db aged past the TTL.
        let dev = mint_cache_entry(&entry, "dev", live_root);
        backdate_dir(&dev, Duration::from_secs(3 * 86_400));
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "dev db evicted as stale-db"
        );
        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::Empty),
            "now-empty entry reclaimed via the Empty policy"
        );
        assert!(
            !entry.as_std_path().exists(),
            "dev-only entry fully reclaimed"
        );
    }

    /// NRN-272 (e): dev-TTL eviction is channel-agnostic — a sweep that exempts
    /// the invoker's own current vault (as a live-channel invocation does) still
    /// evicts a stale dev db belonging to a different vault.
    #[test]
    fn dev_ttl_eviction_applies_under_any_channel() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        // A different vault's entry carrying a stale dev db.
        let other_vault = TempDir::new().unwrap();
        let other_root = other_vault.path().to_str().unwrap();
        let entry1 = mint_cache_entry(&cache_tree, H1, other_root);
        let dev1 = mint_cache_entry(&entry1, "dev", other_root);
        backdate_dir(&dev1, Duration::from_secs(3 * 86_400));
        // The invoker's own current (live) vault — exempt.
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        mint_cache_entry(&cache_tree, H2, live_root);
        let mut o = opts(90);
        o.exempt_hash = Some(H2.to_string());
        let report = sweep(&cache_tree, &state_tree, &o);
        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "a live-channel sweep still evicts a foreign stale dev db"
        );
        assert!(!entry1.join("dev").as_std_path().exists(), "dev/ removed");
    }

    /// NRN-272 (G1): dry-run must predict exactly what a real sweep does when
    /// stale-db eviction interacts with the entry-level policies. Entry A is
    /// stale-db AND aged (dev row + whole-entry row, no double-counting);
    /// entry B is dev-only (dev row + Empty fall-through). Locks are pre-minted
    /// and backdated so lock-file creation can't skew the real run's mtimes.
    #[test]
    fn dry_run_matches_real_run_for_dev_stale_entries() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let past = Duration::from_secs(100 * 86_400);
        // A: live db + dev db, everything aged past retention AND the dev TTL.
        let a = mint_cache_entry(&cache_tree, H1, live_root);
        std::fs::write(a.join(".lock").as_std_path(), b"").unwrap();
        let a_dev = mint_cache_entry(&a, "dev", live_root);
        backdate_dir(&a_dev, past);
        backdate_dir(&a, past);
        // B: dev-only, dev db aged past the TTL.
        let b = cache_tree.join(H2);
        std::fs::create_dir_all(b.as_std_path()).unwrap();
        std::fs::write(b.join(".lock").as_std_path(), b"").unwrap();
        let b_dev = mint_cache_entry(&b, "dev", live_root);
        backdate_dir(&b_dev, past);
        backdate_dir(&b, past);

        let rows = |report: &PruneReport| {
            let mut v: Vec<(String, String, u64)> = report
                .cache
                .evicted
                .iter()
                .map(|e| (e.hash.clone(), format!("{:?}", e.reason), e.bytes))
                .collect();
            v.sort();
            v
        };
        let mut o = opts(90);
        o.dry_run = true;
        let dry = sweep(&cache_tree, &state_tree, &o);
        assert!(a.as_std_path().exists() && b.as_std_path().exists());
        let real = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(rows(&dry), rows(&real), "dry-run must mirror the real run");
        assert_eq!(
            dry.cache.bytes_freed, real.cache.bytes_freed,
            "dev bytes must not double-count in dry-run"
        );
        // Sanity: the expected reason pairs actually occurred.
        let reasons: Vec<&str> = rows(&real)
            .iter()
            .map(|(_, r, _)| r.as_str())
            .map(|r| match r {
                "StaleDb" => "StaleDb",
                "Aged" => "Aged",
                "Empty" => "Empty",
                other => panic!("unexpected reason {other}"),
            })
            .collect();
        assert_eq!(reasons.iter().filter(|r| **r == "StaleDb").count(), 2);
        assert_eq!(reasons.iter().filter(|r| **r == "Aged").count(), 1);
        assert_eq!(reasons.iter().filter(|r| **r == "Empty").count(), 1);
        assert!(!a.as_std_path().exists() && !b.as_std_path().exists());
    }

    /// Parity variant with NO pre-existing `.lock`: the real run's flock
    /// acquisition creates the lock file with mtime=now, and if that timestamp
    /// counted toward entry freshness an otherwise-aged entry would be kept by
    /// the real sweep while dry-run predicted Aged. Lock-file mtimes are
    /// locking metadata, not cache freshness — excluded from newest_mtime.
    #[test]
    fn dry_run_matches_real_run_without_preexisting_lock() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let past = Duration::from_secs(100 * 86_400);
        // Live db + stale dev db, everything aged past retention; no .lock.
        let a = mint_cache_entry(&cache_tree, H1, live_root);
        let a_dev = mint_cache_entry(&a, "dev", live_root);
        backdate_dir(&a_dev, past);
        backdate_dir(&a, past);

        let rows = |report: &PruneReport| {
            let mut v: Vec<(String, String)> = report
                .cache
                .evicted
                .iter()
                .map(|e| (e.hash.clone(), format!("{:?}", e.reason)))
                .collect();
            v.sort();
            v
        };
        let mut o = opts(90);
        o.dry_run = true;
        let dry = sweep(&cache_tree, &state_tree, &o);
        let real = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(
            rows(&dry),
            rows(&real),
            "lock-file creation during the real sweep must not defeat parity"
        );
        assert_eq!(dry.cache.bytes_freed, real.cache.bytes_freed);
        assert!(
            !a.as_std_path().exists(),
            "the aged entry must be reclaimed by the real run"
        );
    }

    /// NRN-272/286 (CR3): a stale-db removal that fails for a non-lock reason
    /// counts as an error skip, not a locked skip — the db stays, the entry is
    /// re-measured (removal-failure path) and classifies normally: no eviction
    /// rows, no bytes freed. Unix-gated: the failure is provoked by stripping
    /// write permission from the dev schema dir so its `cache.db` can't be
    /// unlinked.
    #[cfg(unix)]
    #[test]
    fn unremovable_dev_dir_counts_as_error_skip() {
        use std::os::unix::fs::PermissionsExt;
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root);
        let dev = mint_cache_entry(&entry, "dev", live_root);
        backdate_dir(&dev, Duration::from_secs(3 * 86_400));
        // Read-only dev schema dir: unlinking its cache.db fails, so
        // remove_dir_all of the schema dir fails.
        let dev_schema = dev.join(schema_seg());
        std::fs::set_permissions(
            dev_schema.as_std_path(),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        // Restore before TempDir drop so cleanup succeeds even on panic-free exit.
        std::fs::set_permissions(
            dev_schema.as_std_path(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(report.cache.skipped_errors, 1, "one error-skipped entry");
        assert_eq!(report.cache.skipped_locked, 0, "not misreported as locked");
        assert!(
            dev_schema.join("cache.db").as_std_path().exists(),
            "dev db still present"
        );
        assert!(
            report.cache.evicted.is_empty(),
            "the re-measured entry classifies normally: nothing evicted"
        );
        assert_eq!(report.cache.bytes_freed, 0);
        assert!(entry.as_std_path().exists());
    }

    /// NRN-286: a legacy bare unit whose SIDECAR resists deletion fails the whole
    /// unit — error skip, no eviction row, no bytes claimed for a file still on
    /// disk (accounting honesty). Provoked portably by planting a `cache.db-wal`
    /// that is a non-empty DIRECTORY, so `remove_file` errors on it even though
    /// the real `cache.db` unlinks fine.
    #[test]
    fn legacy_bare_sidecar_removal_failure_counts_as_error_skip() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root); // H1/v{schema} keeps entry + root
                                                                  // Legacy bare at the entry root: a real cache.db plus a `cache.db-wal`
                                                                  // that is a non-empty directory (undeletable by remove_file).
        std::fs::write(entry.join("cache.db").as_std_path(), b"legacy").unwrap();
        let wal_dir = entry.join("cache.db-wal");
        std::fs::create_dir_all(wal_dir.as_std_path()).unwrap();
        std::fs::write(wal_dir.join("blocker").as_std_path(), b"x").unwrap();
        backdate_dir(&entry, Duration::from_secs(3 * 86_400)); // past TTL, within retention
                                                               // backdate_dir sets file mtimes only; age the sidecar dir itself too so
                                                               // the unit's newest-mtime is genuinely past the TTL.
        let old = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(3 * 86_400),
        );
        filetime::set_file_mtime(wal_dir.as_std_path(), old).unwrap();

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        assert_eq!(
            report.cache.skipped_errors, 1,
            "the undeletable sidecar must count as one error skip"
        );
        assert!(
            report
                .cache
                .evicted
                .iter()
                .all(|e| e.reason != EvictReason::StaleDb),
            "no stale-db eviction row for a unit that did not fully remove"
        );
        assert_eq!(
            report.cache.bytes_freed, 0,
            "no bytes claimed while a sidecar remains on disk"
        );
        assert!(
            wal_dir.as_std_path().exists(),
            "the undeletable sidecar stays"
        );
        assert!(
            entry
                .join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "the current-schema db survives"
        );
    }

    /// NRN-272 (G4): a held entry `.lock` blocks dev eviction — the writer-
    /// exclusion property. Dead root makes the entry pass attempt a whole-entry
    /// eviction on the same lock too, pinning skipped_locked to count distinct
    /// blocked entries (exactly 1), not blocked attempts (G3).
    #[test]
    fn held_entry_lock_blocks_dev_eviction_and_counts_once() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, "/nonexistent/vault/gone");
        let dev = mint_cache_entry(&entry, "dev", "/nonexistent/vault/gone");
        backdate_dir(&dev, Duration::from_secs(3 * 86_400));
        let dev_db = dev.join(schema_seg()).join("cache.db");
        let dev_bytes = std::fs::read(dev_db.as_std_path()).unwrap();
        let _held = crate::cache::acquire_flock(&entry.join(".lock"), Duration::ZERO).unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert!(
            report.cache.evicted.is_empty(),
            "nothing evicted under lock"
        );
        assert_eq!(
            report.cache.skipped_locked, 1,
            "one blocked entry counts once, not per attempt"
        );
        assert_eq!(
            std::fs::read(dev_db.as_std_path()).unwrap(),
            dev_bytes,
            "dev db byte-identical under a held writer lock"
        );
        assert!(entry.as_std_path().exists());
    }

    /// NRN-286: a LIVE database at a non-current schema version (`<entry>/vN`,
    /// N != current) aged past STALE_DB_TTL is evicted as `stale-db`, while the
    /// live current-schema db (`<entry>/v{current}`) and the entry survive. This
    /// is the self-healing downgrade case: an obsolete-schema db left behind by
    /// a newer binary ages out instead of pinning disk forever.
    #[test]
    fn stale_non_current_live_schema_dir_evicted_current_survives() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        // Current-schema live db (fresh) + a non-current schema dir (aged past TTL).
        let entry = mint_cache_entry(&cache_tree, H1, live_root);
        let old_schema = "v1";
        assert_ne!(
            old_schema,
            schema_seg(),
            "fixture must be a non-current schema"
        );
        let old_dir = entry.join(old_schema);
        mint_db_at(&old_dir, live_root);
        backdate_dir(&old_dir, Duration::from_secs(3 * 86_400));

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "the non-current schema db must be evicted as stale-db"
        );
        assert!(
            !old_dir.as_std_path().exists(),
            "the stale schema dir is removed"
        );
        assert!(
            entry
                .join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "the current-schema live db survives"
        );
        assert!(entry.as_std_path().exists(), "the entry survives");
    }

    /// NRN-286 migration + GC: a legacy bare `<entry>/cache.db` (the pre-schema-
    /// segment layout) aged past STALE_DB_TTL is evicted as `stale-db` while the
    /// current-schema db built beside it survives. Only the legacy db's files
    /// are removed — the shared lock and the current db are untouched.
    #[test]
    fn legacy_bare_db_evicted_after_ttl_current_survives() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root); // <H1>/v{current}
        std::fs::write(entry.join(".lock").as_std_path(), b"lock").unwrap();
        // Legacy bare db at the entry root, aged past the TTL.
        mint_db_at(&entry, live_root); // writes <H1>/cache.db (+ no schema segment)
        let legacy_db = entry.join("cache.db");
        // Backdate only the legacy db files (not the current schema dir).
        let t = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(3 * 86_400),
        );
        filetime::set_file_mtime(legacy_db.as_std_path(), t).unwrap();

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "the legacy bare db must be evicted as stale-db"
        );
        assert!(
            !legacy_db.as_std_path().exists(),
            "the legacy bare cache.db is removed"
        );
        assert!(
            entry
                .join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "the current-schema db survives"
        );
        assert_eq!(
            std::fs::read(entry.join(".lock").as_std_path()).unwrap(),
            b"lock",
            "the shared .lock is untouched"
        );
        assert!(entry.as_std_path().exists());
    }

    /// NRN-286: evicting a legacy bare DEV db (`<entry>/dev/cache.db`) removes
    /// the now-empty `dev/` channel dir too, not just the db files — otherwise
    /// an emptied `dev/` (no cache.db → never re-detected, live db keeps the
    /// entry alive) would orphan permanently.
    #[test]
    fn legacy_dev_bare_db_eviction_removes_empty_dev_dir() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root); // H1/v{schema} keeps the entry
                                                                  // Legacy bare dev db directly under `dev/`, aged past the TTL.
        let dev = entry.join("dev");
        mint_db_at(&dev, live_root);
        backdate_dir(&dev, Duration::from_secs(3 * 86_400));

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        assert!(
            report
                .cache
                .evicted
                .iter()
                .any(|e| e.hash == H1 && e.reason == EvictReason::StaleDb),
            "the legacy dev bare db must be evicted"
        );
        assert!(
            !dev.as_std_path().exists(),
            "the emptied dev/ channel dir must be removed, not orphaned"
        );
        assert!(entry.as_std_path().exists(), "the entry survives");
    }

    /// NRN-286: a crash-orphaned `-wal`/`-shm` sidecar with no `cache.db` is
    /// still a reclaimable stale-db location. A `vN` dir holding only a `-wal`,
    /// aged past the TTL, is evicted rather than pinning its bytes forever.
    #[test]
    fn orphaned_sidecar_only_schema_dir_evicted() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = mint_cache_entry(&cache_tree, H1, live_root); // current db recovers the root
                                                                  // A non-current schema dir holding ONLY an orphaned -wal sidecar.
        let orphan = entry.join("v1");
        assert_ne!("v1", schema_seg(), "fixture must be a non-current schema");
        std::fs::create_dir_all(orphan.as_std_path()).unwrap();
        std::fs::write(orphan.join("cache.db-wal").as_std_path(), vec![0u8; 512]).unwrap();
        backdate_dir(&orphan, Duration::from_secs(3 * 86_400));

        let report = sweep(&cache_tree, &state_tree, &opts(90));

        let evict = report
            .cache
            .evicted
            .iter()
            .find(|e| e.hash == H1 && e.reason == EvictReason::StaleDb)
            .expect("an orphaned-sidecar-only schema dir must be evicted");
        assert!(evict.bytes > 0, "its bytes are reclaimed");
        assert!(!orphan.as_std_path().exists(), "the orphan dir is removed");
        assert!(
            entry
                .join(schema_seg())
                .join("cache.db")
                .as_std_path()
                .exists(),
            "the current-schema db survives"
        );
    }

    #[test]
    fn missing_trees_yield_empty_report() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("nope-cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("nope-state")).unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(report.cache.scanned, 0);
        assert_eq!(report.state.scanned, 0);
        assert_eq!(report.total_bytes_freed(), 0);
    }
}
