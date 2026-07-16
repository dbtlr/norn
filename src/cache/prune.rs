//! Cross-vault GC for the global cache and state trees.
//!
//! Scans `<XDG_CACHE_HOME>/norn/` and `<XDG_STATE_HOME>/norn/` for per-vault
//! hash entries, recovers each entry's recorded vault root, classifies, and
//! evicts. Cache entries are disposable (evicted on any signal); state entries
//! hold the append-only mutation event stream and are evicted only when their
//! vault root no longer exists (or the entry is empty).

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use camino::{Utf8Path, Utf8PathBuf};
use serde::{Serialize, Serializer};

/// Internal, non-configurable total-size cap for the cache tree (1 GiB).
pub(crate) const CACHE_TREE_SIZE_CAP_BYTES: u64 = 1024 * 1024 * 1024;
/// Lazy-sweep throttle: at most one sweep per interval.
pub(crate) const PRUNE_THROTTLE: Duration = Duration::from_secs(24 * 3_600);
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
        let opts = PruneOptions {
            retention: cache_cfg
                .as_ref()
                .and_then(|c| c.retention)
                .unwrap_or(crate::standards::DEFAULT_CACHE_RETENTION),
            cap_bytes: CACHE_TREE_SIZE_CAP_BYTES,
            dry_run: false,
            exempt_hash: crate::cache::vault_identity_hash(cwd),
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
    /// Hash of the running command's own vault entry; never evicted.
    pub exempt_hash: Option<String>,
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
/// children contribute nothing. norn's own lock files contribute bytes and
/// mtime but are excluded from the file count.
fn measure(dir: &Utf8Path) -> (u64, Option<SystemTime>, usize) {
    let mut bytes = 0u64;
    let mut newest: Option<SystemTime> = None;
    let mut files = 0usize;
    let Ok(entries) = std::fs::read_dir(dir.as_std_path()) else {
        return (0, None, 0);
    };
    for e in entries.flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            if let Ok(sub) = Utf8PathBuf::from_path_buf(e.path()) {
                let (b, m, f) = measure(&sub);
                bytes += b;
                newest = max_opt(newest, m);
                files += f;
            }
        } else {
            bytes += md.len();
            if !e.file_name().to_str().is_some_and(is_lock_file) {
                files += 1;
            }
            newest = max_opt(newest, md.modified().ok());
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

/// Read `meta.vault_root` from an entry's cache.db, read-only. None on any failure.
///
/// Falls back to the dev-channel database (`<entry>/dev/cache.db`, NRN-269)
/// only when the live database is ABSENT, so a dev-only entry recovers its root
/// and ages out by mtime under the normal retention policy rather than being
/// misclassified `Unreadable` and evicted immediately. A PRESENT-but-unreadable
/// (corrupt) live db deliberately does not fall back: the entry stays
/// `Unreadable` and evicts promptly, and no second SQLite open is paid on a
/// known-corrupt db. The whole entry dir (live db, `dev/`, lock) is removed
/// together on eviction, so both channels age as one unit.
fn read_cache_root(entry_dir: &Utf8Path) -> Option<String> {
    let live_db = entry_dir.join("cache.db");
    if live_db.as_std_path().exists() {
        return read_root_from_db(&live_db);
    }
    read_root_from_db(&entry_dir.join("dev").join("cache.db"))
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

/// Try-evict one cache entry: skip if its write lock is held. Returns
/// Ok(true) on eviction, Ok(false) on locked-skip.
fn evict_cache_entry(entry: &Entry) -> std::io::Result<bool> {
    match crate::cache::acquire_flock(&entry.dir.join(".lock"), Duration::ZERO) {
        Ok(_held) => {
            std::fs::remove_dir_all(entry.dir.as_std_path())?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        // A lock we can't even open (permissions, IO): treat as locked-skip;
        // the sweep is best-effort and never escalates.
        Err(_) => Ok(false),
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
            Some(reason) => evict_into(&mut cache_report, entry, reason, now, opts.dry_run),
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
) {
    if !dry_run {
        match evict_cache_entry(&entry) {
            Ok(true) => {}
            Ok(false) => {
                report.skipped_locked += 1;
                return;
            }
            Err(_) => return, // best-effort: a failed removal is silently skipped
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

    /// Mint a cache entry: <tree>/<hash>/cache.db with a real schema and a
    /// meta.vault_root row pointing at `root`.
    fn mint_cache_entry(tree: &Utf8Path, hash: &str, root: &str) -> Utf8PathBuf {
        let dir = tree.join(hash);
        std::fs::create_dir_all(dir.as_std_path()).unwrap();
        let conn = rusqlite::Connection::open(dir.join("cache.db").as_std_path()).unwrap();
        conn.execute_batch(crate::cache::schema::DDL).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('vault_root', ?)",
            rusqlite::params![root],
        )
        .unwrap();
        dir
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

    /// NRN-269: an entry holding ONLY a dev-channel database (no live
    /// `cache.db`) recovers its vault root from `dev/cache.db` and classifies
    /// normally — here dead-root, since the recorded vault is gone. Without the
    /// fallback it would be misclassified `Unreadable`.
    #[test]
    fn dev_only_entry_classifies_by_its_dev_root() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        // Mint the db inside dev/ by minting a nested entry, then re-homing:
        // <tree>/<H1>/dev/cache.db with a dead recorded root.
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

    /// NRN-269: a PRESENT-but-corrupt live db must stay `Unreadable` (and evict
    /// promptly) even when a healthy dev db sits alongside — the fallback is
    /// gated on the live db being absent, not merely unreadable.
    #[test]
    fn corrupt_live_db_stays_unreadable_despite_healthy_dev_db() {
        let trees = TempDir::new().unwrap();
        let cache_tree = Utf8PathBuf::from_path_buf(trees.path().join("cache")).unwrap();
        let state_tree = Utf8PathBuf::from_path_buf(trees.path().join("state")).unwrap();
        let live_vault = TempDir::new().unwrap();
        let live_root = live_vault.path().to_str().unwrap();
        let entry = cache_tree.join(H1);
        // Healthy dev db recording a LIVE root...
        mint_cache_entry(&entry, "dev", live_root);
        // ...but the live-channel db is corrupt.
        std::fs::write(entry.join("cache.db").as_std_path(), b"not sqlite").unwrap();
        let report = sweep(&cache_tree, &state_tree, &opts(90));
        assert_eq!(hashes(&report.cache.evicted), vec![H1]);
        assert_eq!(report.cache.evicted[0].reason, EvictReason::Unreadable);
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
        let past = std::time::SystemTime::now() - Duration::from_secs(100 * 86_400);
        for f in std::fs::read_dir(old.as_std_path()).unwrap().flatten() {
            filetime::set_file_mtime(f.path(), filetime::FileTime::from_system_time(past)).unwrap();
        }
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
            let age = Duration::from_secs((30 - i as u64 * 10) * 86_400); // 30d, 20d, 10d
            let t = filetime::FileTime::from_system_time(std::time::SystemTime::now() - age);
            for f in std::fs::read_dir(dir.as_std_path()).unwrap().flatten() {
                filetime::set_file_mtime(f.path(), t).unwrap();
            }
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
            let age = Duration::from_secs((30 - i as u64 * 10) * 86_400); // 30d, 20d, 10d
            let t = filetime::FileTime::from_system_time(std::time::SystemTime::now() - age);
            for f in std::fs::read_dir(dir.as_std_path()).unwrap().flatten() {
                filetime::set_file_mtime(f.path(), t).unwrap();
            }
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
        let past = std::time::SystemTime::now() - Duration::from_secs(100 * 86_400);
        let ft = filetime::FileTime::from_system_time(past);
        for f in std::fs::read_dir(state_tree.join(H3).join("events").as_std_path())
            .unwrap()
            .flatten()
        {
            filetime::set_file_mtime(f.path(), ft).unwrap();
        }
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
