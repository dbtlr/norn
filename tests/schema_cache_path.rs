//! NRN-286: the cache database is schema-qualified (`<entry>/v{schema}/cache.db`),
//! so mixed norn versions coexist — each builds and uses its own cache — and the
//! retired "schema newer than this binary supports; upgrade norn" hard error can
//! never fire again.
//!
//! Standing tripwire for the incident + downgrade shapes: a "newer binary's" db
//! is planted next to the current binary's schema dir (`<entry>/v999/cache.db`).
//! The current binary must open, query, and rebuild its OWN db without erroring
//! and WITHOUT touching the foreign db (asserted byte-identical), and never emit
//! the upgrade-required message.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const UPGRADE_MSG: &str = "newer than this binary supports";

/// The single 64-hex vault entry dir under `<cache_home>/norn/`.
fn find_entry_dir(cache_home: &Path) -> PathBuf {
    let norn = cache_home.join("norn");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&norn)
        .expect("cache tree must exist after a run")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.len() == 64 && n.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .collect();
    assert_eq!(hits.len(), 1, "exactly one vault entry expected: {hits:?}");
    hits.remove(0)
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    std::fs::create_dir_all(&tree).expect("NRN-287 sweep isolation: pre-write throttle-marker dir");
    std::fs::write(tree.join(".last-prune"), b"")
        .expect("NRN-287 sweep isolation: pre-write throttle marker");
}

fn norn(
    bin: &Path,
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> std::process::Output {
    prewrite_prune_marker(cache_home);
    Command::new(bin)
        // Force live so the db path is deterministic (`<entry>/v{schema}/cache.db`);
        // the test binary would otherwise resolve dev.
        .env("NORN_CACHE_CHANNEL", "live")
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .arg("--cwd")
        .arg(vault)
        .args(args)
        .output()
        .expect("run norn")
}

#[test]
fn foreign_newer_schema_db_coexists_and_is_never_touched_or_upgraded() {
    let bin = Path::new(env!("CARGO_BIN_EXE_norn"));
    let xdg = TempDir::new().unwrap();
    let cache_home = xdg.path().join("cache");
    let state_home = xdg.path().join("state");
    // Non-dot prefix: `TempDir::new()` yields a `.tmp…` leading-dot dir, which
    // norn's walker treats as hidden and skips — the vault would index empty.
    let vault = tempfile::Builder::new()
        .prefix("norn-vault-")
        .tempdir()
        .unwrap();
    std::fs::write(vault.path().join("a.md"), "---\ntype: note\n---\nbody\n").unwrap();

    // 1. First run creates the current-schema cache at `<entry>/v{schema}/cache.db`.
    let out = norn(
        bin,
        &cache_home,
        &state_home,
        vault.path(),
        &["cache", "status", "--format", "json"],
    );
    assert!(
        out.status.success(),
        "initial cache status failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let entry = find_entry_dir(&cache_home);

    // 2. Plant a "newer binary's" real cache db in its own schema dir. A genuinely
    //    newer binary writes here; the current binary must never read or write it.
    let foreign_dir = entry.join("v999");
    std::fs::create_dir_all(&foreign_dir).unwrap();
    let foreign_db = foreign_dir.join("cache.db");
    {
        let conn = rusqlite::Connection::open(&foreign_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta (key, value) VALUES ('schema_version', '999');
             CREATE TABLE future_only (x INTEGER);",
        )
        .unwrap();
    }
    let foreign_bytes = std::fs::read(&foreign_db).unwrap();

    // 3. Force a full rebuild of the current db with the foreign db present — the
    //    "downgrade" shape. Must succeed and never emit the upgrade message.
    let out = norn(
        bin,
        &cache_home,
        &state_home,
        vault.path(),
        &["cache", "rebuild"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "cache rebuild failed with a foreign schema db present: {stderr}"
    );
    assert!(
        !stderr.contains(UPGRADE_MSG),
        "the retired upgrade-required error must never fire: {stderr}"
    );
    assert!(
        !stdout.contains(UPGRADE_MSG),
        "the retired upgrade-required error must never fire: {stdout}"
    );

    // 4. The current binary opens and queries its OWN rebuilt cache: cache status
    //    reports the current schema, the live channel, and the indexed document —
    //    a read against the freshly-rebuilt db, never the foreign one.
    let out = norn(
        bin,
        &cache_home,
        &state_home,
        vault.path(),
        &["cache", "status", "--format", "json"],
    );
    assert!(
        out.status.success(),
        "cache status failed with a foreign schema db present: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["channel"], "live");
    assert!(
        v["doc_count"].as_u64().unwrap() >= 1,
        "the rebuilt cache must hold the vault's document: {v}"
    );
    assert_ne!(
        v["schema_version"], 999,
        "the current binary must use its OWN schema, not the foreign db's"
    );
    // The reported cache path is schema-qualified and is NOT the foreign db.
    let reported = v["cache_path"].as_str().unwrap();
    assert!(reported.ends_with("cache.db"));
    assert!(
        !reported.contains("v999"),
        "current binary must not report the foreign db path: {reported}"
    );

    // 6. The foreign db is byte-for-byte untouched throughout.
    assert_eq!(
        std::fs::read(&foreign_db).unwrap(),
        foreign_bytes,
        "the foreign (newer-schema) db must never be read, migrated, or deleted"
    );
}
