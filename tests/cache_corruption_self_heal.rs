//! NRN-275: query-time cache corruption self-heals through the real CLI path.
//!
//! The trusting direct-read open skips `PRAGMA integrity_check`, so byte
//! corruption a query first touches AFTER `open_for_query` has returned surfaces
//! outside the in-place retry. This proves the top-level CLI error seam evicts the
//! corrupt cache so the NEXT invocation opens Fresh and rebuilds — the read never
//! wedges into a state that fails identically until a manual `norn cache rebuild`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn norn(cache_home: &Path, state_home: &Path, vault: &Path, args: &[&str]) -> Output {
    prewrite_prune_marker(cache_home);
    Command::new(env!("CARGO_BIN_EXE_norn"))
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .arg("--cwd")
        .arg(vault)
        .args(args)
        .output()
        .expect("run norn")
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

/// Recursively find the single `cache.db` under a private cache home.
fn find_cache_db(cache_home: &Path) -> PathBuf {
    fn walk(dir: &Path, out: &mut Option<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|n| n == "cache.db") {
                *out = Some(path);
            }
        }
    }
    let mut found = None;
    walk(cache_home, &mut found);
    found.expect("cache.db must exist after the build")
}

/// Zero the b-tree root page of `table`, leaving the rest of the database — the
/// header, `meta`, and every other table — intact. Reading `table` then raises
/// `SQLITE_CORRUPT`, while an open's `meta` SELECTs still succeed.
fn zero_table_root_page(db: &Path, table: &str) {
    use std::io::{Seek, SeekFrom, Write};

    let (page_size, rootpage): (i64, i64) = {
        let conn = rusqlite::Connection::open(db).unwrap();
        let page_size = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        let rootpage = conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        (page_size, rootpage)
    };
    // Drop any WAL/SHM so the main-db page we zero is the authoritative one.
    let _ = std::fs::remove_file(db.with_extension("db-wal"));
    let _ = std::fs::remove_file(db.with_extension("db-shm"));

    let mut f = std::fs::OpenOptions::new().write(true).open(db).unwrap();
    f.seek(SeekFrom::Start(((rootpage - 1) * page_size) as u64))
        .unwrap();
    f.write_all(&vec![0u8; page_size as usize]).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn query_time_corruption_self_heals_on_next_invocation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let base = tmp.path();
    let vault = base.join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    // `a.md` carries headings so the `headings` table is populated and read by
    // `get`; `b.md` links to it so `links` is non-empty too.
    std::fs::write(
        vault.join("a.md"),
        "---\ntitle: A\n---\n# A\n## Section\nbody\n",
    )
    .unwrap();
    std::fs::write(vault.join("b.md"), "# B\n[[a]]\n").unwrap();

    let cache_home = base.join("cache");
    let state_home = base.join("state");
    std::fs::create_dir_all(&cache_home).unwrap();
    std::fs::create_dir_all(&state_home).unwrap();

    // Build the cache (no daemon exists under this private home → runs Direct).
    let build = norn(&cache_home, &state_home, &vault, &["count"]);
    assert!(
        build.status.success(),
        "build must succeed: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    // Corrupt the `headings` root page. The freshness refresh scans `documents`,
    // not `headings`, so `open_for_query` returns a handle cleanly and the
    // corruption only surfaces when `get` reads headings at QUERY time — after the
    // open, outside the in-place retry.
    let db = find_cache_db(&cache_home);
    zero_table_root_page(&db, "headings");

    // First invocation: the query hits the malformed page and fails closed, but
    // the CLI error seam classifies the corruption and evicts the cache.
    let first = norn(&cache_home, &state_home, &vault, &["get", "a"]);
    assert!(
        !first.status.success(),
        "the first get must fail closed on query-time corruption; stdout: {}",
        String::from_utf8_lossy(&first.stdout)
    );
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_err.contains("cache is corrupted"),
        "the error seam must emit the corruption/rebuild notice; stderr: {first_err}"
    );

    // Second invocation: the evicted cache is rebuilt from the vault and the read
    // succeeds — proving the corruption did not wedge every subsequent run.
    let second = norn(&cache_home, &state_home, &vault, &["get", "a"]);
    assert!(
        second.status.success(),
        "the second get must self-heal after eviction; stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second_out = String::from_utf8_lossy(&second.stdout);
    assert!(
        second_out.contains("a.md") || second_out.contains("title"),
        "the healed read must return document A; stdout: {second_out}"
    );
}
