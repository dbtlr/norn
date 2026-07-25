//! Regression proof for the NRN-465 false stall: a routed CLI mutation must NOT
//! fail `post-send-uncertain` while the warm daemon rebuilds/refreshes a large
//! vault's cache.
//!
//! Before the fix, the client stall watchdog declared the daemon wedged whenever
//! a busy writer's opaque progress sequence stayed unchanged for the stall budget.
//! A freshness refresh reparses the WHOLE vault on any single content change, and
//! that reparse advanced the sequence only at op boundaries — so on a large vault
//! it froze the sequence for the entire multi-second reparse, tripped the budget,
//! and the client gave up with exit 1 even though the daemon was healthy and
//! applied the write moments later (the write landed but was never acked).
//!
//! The fix advances the sequence from inside the reparse's work loops. This test
//! pins the behavior deterministically WITHOUT a multi-second wait by shrinking the
//! client stall budget via `NORN_SERVICE_STALL_BUDGET_MS`: a staleness-triggered
//! whole-vault reparse of the seeded vault exceeds that small budget, so the
//! pre-fix client would false-stall (exit 1, empty stdout) while the fixed client
//! waits through the reparse (exit 0, applied, envelope received). The budget
//! override is set ONLY on the mutating call; the warm-up read keeps the full
//! default budget so the cold FIRST build never stalls.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, norn_bin, spawn_ready_daemon_with_log};

use std::path::Path;
use std::process::{Command, Stdio};

/// Number of small filler docs. Large enough that the staleness-triggered
/// whole-vault reparse is a real, multi-hundred-millisecond op — so under the
/// small stall budget below the pre-fix client would false-stall, and the fixed
/// client's per-batch progress ticks are what keep it waiting. Kept modest so
/// setup + two whole-vault builds (cold warm-up, then the refresh reparse) stay
/// well under the suite's per-test budget.
const FILLER_DOCS: usize = 8000;

/// The shrunk client stall budget for the mutating call (ms). Small enough that a
/// frozen sequence during the reparse trips it pre-fix; the fix advances the
/// sequence many times within this window so it never does.
const STALL_BUDGET_MS: &str = "400";

/// Pre-write the lazy-sweep throttle marker so norn invocations under this cache
/// home never spawn a detached GC sweep child that could race the test (mirrors
/// the other serve suites).
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    std::fs::create_dir_all(&tree).expect("throttle-marker dir");
    std::fs::write(tree.join(".last-prune"), b"").expect("throttle marker");
}

/// Seed the vault: one settable `task` doc plus `FILLER_DOCS` tiny notes that make
/// the whole-vault reparse expensive.
fn seed_vault(root: &Path) {
    std::fs::create_dir_all(root).expect("vault dir");
    std::fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: backlog\ntitle: Task One\n---\nTask body\n",
    )
    .expect("write task.md");
    for i in 0..FILLER_DOCS {
        std::fs::write(
            root.join(format!("note-{i}.md")),
            format!("---\ntype: note\ntitle: Note {i}\n---\nBody of note {i}.\n"),
        )
        .expect("write filler note");
    }
}

/// Run a routed `norn` subcommand against `vault` under the daemon's cache home.
/// `stall_budget_ms` is threaded to the client when `Some`, mirroring the
/// `NORN_SERVICE_STALL_BUDGET_MS` override.
fn run_norn(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    stall_budget_ms: Option<&str>,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    prewrite_prune_marker(cache_home);
    let mut cmd = Command::new(norn_bin());
    cmd.env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        // Generous handshake so a late-scheduled daemon still answers the probe
        // under CI load (a silent fall-back to Direct would defeat the proof).
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .args(args);
    if let Some(ms) = stall_budget_ms {
        cmd.env("NORN_SERVICE_STALL_BUDGET_MS", ms);
    }
    let out = cmd.output().expect("run norn");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

#[test]
fn routed_mutation_waits_through_large_vault_reparse() {
    let daemon = spawn_ready_daemon_with_log(&[]);

    let vault_tmp = tempfile::Builder::new()
        .prefix("norn-stall-vault-")
        .tempdir()
        .expect("vault tempdir");
    let vault = vault_tmp.path();
    seed_vault(vault);

    // Warm the daemon: the first routed read triggers the cold FIRST build of the
    // whole vault. Full (default) stall budget so that build never stalls.
    let (_out, warm_err, warm_code) = run_norn(
        &daemon.cache_home,
        &daemon.state_home,
        vault,
        None,
        &["count"],
    );
    assert_eq!(
        warm_code,
        0,
        "warm-up count should succeed; stderr: {}",
        String::from_utf8_lossy(&warm_err)
    );
    assert!(
        count_served(&daemon.stderr_path, "vault.count") >= 1,
        "warm-up count must be SERVED by the daemon (not a silent Direct fall-back)"
    );

    // Make the vault stale: rewrite one filler doc's CONTENT (different length, so
    // the cheap mtime+size check trips) — the next request's freshness probe now
    // sees stale and refreshes, reparsing the WHOLE vault.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(
        vault.join("note-0.md"),
        "---\ntype: note\ntitle: Note 0 EDITED\n---\nThis body was edited to force a stale refresh.\n",
    )
    .expect("edit filler note");

    // The routed mutation, under the SHRUNK stall budget. Its freshness refresh
    // reparses the whole vault; pre-fix the frozen sequence trips the budget and
    // the client exits 1 (post-send-uncertain, empty stdout). Post-fix the
    // sequence advances per batch of parsed files, so the client waits it out.
    let (out, err, code) = run_norn(
        &daemon.cache_home,
        &daemon.state_home,
        vault,
        Some(STALL_BUDGET_MS),
        &["set", "task", "--field", "status=active", "--yes"],
    );
    let stderr = String::from_utf8_lossy(&err);

    assert_eq!(
        code, 0,
        "routed set must succeed through the reparse, not false-stall (exit {code}); stderr: {stderr}"
    );
    assert!(
        !stderr.contains("post-send-uncertain"),
        "routed set must not report the false post-send-uncertain stall; stderr: {stderr}"
    );
    assert!(
        !out.is_empty(),
        "the tool response envelope must have been RECEIVED (non-empty stdout)"
    );
    assert!(
        count_served(&daemon.stderr_path, "vault.set") >= 1,
        "the mutation must have been SERVED by the daemon; stderr: {stderr}"
    );

    // The field was actually applied on disk.
    let task = std::fs::read_to_string(vault.join("task.md")).expect("read task.md");
    assert!(
        task.contains("status: active"),
        "the mutation must be applied on disk; got:\n{task}"
    );
}
