//! Concurrency / load coverage for the `norn serve` daemon (NRN-93).
//!
//! Two properties that only show up under real concurrency:
//!
//! - Pings stay responsive while heavy `vault.find` queries are in flight. The
//!   daemon runs vault work on `spawn_blocking` threads and answers pings off the
//!   accept loop, so a ping must not starve behind seconds-long queries (the
//!   pre-`spawn_blocking` failure mode).
//! - Concurrent first-touch `hello`s for the SAME vault open the context exactly
//!   once (per-entry `OnceCell`): no "database is locked", no double-open crash.

#[path = "serve_util/mod.rs"]
mod serve_util;

use std::time::{Duration, Instant};

use serde_json::json;
use serve_util::*;
use tempfile::TempDir;

/// A vault with `n` small `type: note` docs (each with a short body).
fn seed_many_notes(prefix: &str, n: usize) -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    for i in 0..n {
        std::fs::write(
            tmp.path().join(format!("doc{i:04}.md")),
            format!("---\ntype: note\ntitle: Doc {i}\n---\nbody text for document number {i}\n"),
        )
        .unwrap();
    }
    tmp
}

fn find_all_notes(conn: &mut Conn) -> Vec<serde_json::Value> {
    // Full-body dump over every note is the heaviest read the tool offers.
    find_documents(&conn.call_tool(
        "vault.find",
        serde_json::json!({ "eq": ["type:note"], "limit": 10000, "all_cols": true }),
    ))
}

/// Under sustained heavy query load on 4 connections, control pings keep
/// answering promptly, and every query completes successfully.
#[test]
fn ping_stays_responsive_under_query_load() {
    const DOCS: usize = 500;
    const CONNS: usize = 4;
    const LOAD_WINDOW: Duration = Duration::from_secs(3);

    let daemon = spawn_ready_daemon();
    let vault = seed_many_notes("norn-serve-load-", DOCS);

    // Open + warm each connection (one find pays the first-touch integrity check
    // once, so the load loop measures steady-state, not cold start).
    let mut conns: Vec<Conn> = (0..CONNS)
        .map(|_| {
            let mut c = connect_and_hello(&daemon.socket_path, vault.path());
            c.initialize();
            assert_eq!(
                find_all_notes(&mut c).len(),
                DOCS,
                "warm-up find must see all docs"
            );
            c
        })
        .collect();

    // Fire sustained finds on every connection for the load window.
    let deadline = Instant::now() + LOAD_WINDOW;
    let workers: Vec<_> = conns
        .drain(..)
        .map(|mut c| {
            std::thread::spawn(move || {
                let mut queries = 0usize;
                while Instant::now() < deadline {
                    assert_eq!(
                        find_all_notes(&mut c).len(),
                        DOCS,
                        "every find under load must return the full set"
                    );
                    queries += 1;
                }
                queries
            })
        })
        .collect();

    // While queries are in flight, time fresh-connection pings. Each must answer
    // well within a generous CI bound.
    for i in 0..5 {
        let (pong, elapsed) = try_ping_timed(&daemon.socket_path);
        let pong = pong.unwrap_or_else(|| panic!("ping #{i} got no pong under load"));
        assert_eq!(pong["norn_control"], "pong", "ping #{i}: {pong}");
        assert!(
            elapsed < Duration::from_secs(2),
            "ping #{i} took {elapsed:?} under load — must stay responsive (< 2s)"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // All query workers finished cleanly and did real work.
    for (i, w) in workers.into_iter().enumerate() {
        let queries = w.join().expect("query worker panicked");
        assert!(queries > 0, "worker {i} ran no queries");
    }
}

/// Four threads `hello` the SAME vault with no pre-built cache; the per-entry
/// OnceCell means one shared open, and all four finds succeed.
#[test]
fn concurrent_same_vault_first_touch() {
    const CONNS: usize = 4;
    const DOCS: usize = 40;

    let daemon = spawn_ready_daemon();
    let vault = seed_many_notes("norn-serve-firsttouch-", DOCS);

    let socket = daemon.socket_path.clone();
    let vault_path = vault.path().to_path_buf();

    let workers: Vec<_> = (0..CONNS)
        .map(|_| {
            let socket = socket.clone();
            let vault_path = vault_path.clone();
            std::thread::spawn(move || {
                let mut c = connect_and_hello(&socket, &vault_path);
                c.initialize();
                find_documents(&c.call_tool(
                    "vault.find",
                    serde_json::json!({ "eq": ["type:note"], "limit": 10000 }),
                ))
                .len()
            })
        })
        .collect();

    for (i, w) in workers.into_iter().enumerate() {
        let count = w.join().expect("concurrent first-touch worker panicked");
        assert_eq!(
            count, DOCS,
            "worker {i}: concurrent first-touch find must return all {DOCS} docs"
        );
    }
}

/// NRN-253 (read concurrency LIVE end-to-end): several clients hit ONE warm vault
/// at once — most issuing reads, one issuing a mutation — and all succeed with
/// correct results. Before this commit `call_lock` serialized every warm tool body
/// one-at-a-time per vault; now warm reads run concurrently and a mutation
/// interleaves with them. This is the daemon-level proof; the fine-grained overlap
/// / coalescing seams are unit-tested against `VaultEnv` (NRN-256 owns the
/// benchmark, so this stays a handful of calls, not a load run).
#[test]
fn concurrent_reads_and_a_mutation_on_one_warm_vault_all_succeed() {
    const READERS: usize = 4;
    const READS_EACH: usize = 3;
    const DOCS: usize = 20;

    let daemon = spawn_ready_daemon();
    let vault = seed_many_notes("norn-serve-rw-", DOCS);

    // Warm the vault once so the concurrent phase runs against a live generation.
    let mut warm = connect_and_hello(&daemon.socket_path, vault.path());
    warm.initialize();
    assert_eq!(
        find_all_notes(&mut warm).len(),
        DOCS,
        "warm-up find must see all docs"
    );

    let socket = daemon.socket_path.clone();
    let vault_path = vault.path().to_path_buf();

    // READERS reader threads, each on its OWN connection to the same warm vault.
    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let socket = socket.clone();
            let vault_path = vault_path.clone();
            std::thread::spawn(move || {
                let mut c = connect_and_hello(&socket, &vault_path);
                c.initialize();
                let mut last = 0usize;
                for _ in 0..READS_EACH {
                    last = find_all_notes(&mut c).len();
                }
                last
            })
        })
        .collect();

    // One mutator thread, concurrent with the readers: set doc0000's title.
    let mutator = {
        let socket = socket.clone();
        let vault_path = vault_path.clone();
        std::thread::spawn(move || {
            let mut c = connect_and_hello(&socket, &vault_path);
            c.initialize();
            let resp = c.call_tool(
                "vault.set",
                json!({
                    "target": "doc0000",
                    "field": ["title=Mutated Concurrently"],
                    "confirm": true,
                }),
            );
            assert!(
                resp.get("error").is_none(),
                "concurrent vault.set must not error: {resp}"
            );
            resp
        })
    };

    // Every reader saw the full set (a title change never drops a doc), and the
    // mutation succeeded.
    for (i, r) in readers.into_iter().enumerate() {
        let count = r.join().expect("reader thread panicked");
        assert_eq!(
            count, DOCS,
            "reader {i}: every concurrent find must return all {DOCS} docs"
        );
    }
    let _ = mutator.join().expect("mutator thread panicked");

    // The mutation actually landed on disk (proof it was served, not a no-op).
    let body = read_to_string(&vault_path.join("doc0000.md"));
    assert!(
        body.contains("Mutated Concurrently"),
        "the concurrent vault.set must have written the new title, got:\n{body}"
    );
}
