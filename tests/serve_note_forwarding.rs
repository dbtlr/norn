//! End-to-end proof that a routed read under REAL lock contention re-emits the
//! daemon-side operator note on the CLI's stderr byte-identically to the direct
//! path (NRN-215), and that the daemon's own stderr keeps the note as its
//! operational-log line.
//!
//! Contention is staged genuinely on both sides: the test holds an exclusive
//! flock on the vault's cache `.lock` file while the read runs, so the implicit
//! `index_incremental` refresh times out through the real code path. The
//! test-only `NORN_CACHE_LOCK_TIMEOUT_MS` override (see `src/cache/lock.rs`)
//! shrinks the 5s production timeout to 200ms so the suite doesn't stall.

#![cfg(unix)]
// Debug-profile only: `norn_bin()` spawns the binary built alongside this test
// (`target/<profile>/norn`), and the `NORN_CACHE_LOCK_TIMEOUT_MS` override the
// spawned processes rely on is compiled out of release builds
// (`#[cfg(debug_assertions)]` in `src/cache/lock.rs`) — under a release test
// profile the contended acquires would stall the full 5s production timeout and
// overrun the routed-read budget. The test crate compiles with the same profile
// as the binary, so this cfg gate tracks it exactly.
#![cfg(debug_assertions)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, norn_bin, read_to_string, socket_path_for, wait_for_ready};

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use fs2::FileExt;
use tempfile::TempDir;

/// The exact operator note both surfaces emit on write-lock contention — one
/// shared constant in `src/cache.rs`; this literal is the wire-frozen contract.
const NOTE: &str = "vault: another cache operation is in progress; using current cache state";

/// The test-only write-lock timeout override (`src/cache/lock.rs`).
const LOCK_TIMEOUT_ENV: &str = "NORN_CACHE_LOCK_TIMEOUT_MS";

fn seed_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-note-fwd-vault-")
        .tempdir()
        .expect("tempdir");
    std::fs::write(
        tmp.path().join("note1.md"),
        "---\ntype: note\nstatus: active\n---\nbody one\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("task1.md"),
        "---\ntype: task\nstatus: backlog\n---\nbody task\n",
    )
    .unwrap();
    tmp
}

/// Run `norn --cwd <vault> <args…>` against the given cache/state homes with
/// the short lock-timeout override set. Returns `(stdout, stderr, exit_code)`.
fn run_norn(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .env(LOCK_TIMEOUT_ENV, "200")
        // Generous handshake budget for CI load (see serve_count_routing.rs).
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .arg("--cwd")
        .arg(vault)
        .args(args)
        .output()
        .expect("run norn");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Find the single per-vault cache dir under `<cache_home>/norn/` (the one
/// holding `cache.db`) and return its `.lock` path. The daemon's `run/` and
/// `log/` subdirs never hold a `cache.db`, so this is unambiguous.
fn cache_lock_path(cache_home: &Path) -> PathBuf {
    let tree = cache_home.join("norn");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&tree)
        .unwrap_or_else(|e| panic!("read {tree:?}: {e}"))
        .filter_map(|entry| {
            let dir = entry.ok()?.path();
            dir.join("cache.db").exists().then(|| dir.join(".lock"))
        })
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "expected exactly one vault cache dir under {tree:?}, got {hits:?}"
    );
    hits.pop().unwrap()
}

/// Hold an exclusive flock on the vault's cache `.lock` — the same advisory
/// lock `Cache::index_incremental` acquires — so the next implicit refresh
/// times out through the real contention path.
fn hold_cache_lock(cache_home: &Path) -> File {
    let path = cache_lock_path(cache_home);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap_or_else(|e| panic!("open {path:?}: {e}"));
    file.try_lock_exclusive()
        .unwrap_or_else(|e| panic!("flock {path:?}: {e}"));
    file
}

/// Rewrite `task1.md` with a body of a DIFFERENT length so the next read sees
/// the vault as STALE (a size change is detected regardless of mtime
/// granularity).
///
/// Staging staleness before each contended run is REQUIRED post-NRN-253: the
/// warm daemon's read pipeline now runs a read-only freshness PROBE first and
/// only touches the write path (`index_incremental`, which acquires the
/// WriteLock) when the probe reports stale — a FRESH routed read never contends
/// the held flock and would emit no note. The direct CLI path still acquires
/// the WriteLock unconditionally (`open_for_query` → `index_incremental` locks
/// before change detection), so a fresh DIRECT read notes contention while a
/// fresh ROUTED read does not; that fresh-read direct/routed divergence is
/// tracked separately as NRN-260. Staging a real change on BOTH sides keeps
/// this test on the common ground: both sides genuinely attempt a refresh, both
/// time out on the held flock, and the note-parity assertions stay byte-exact.
///
/// The mutated doc is the `task` (not a `note`), so the asserted
/// `count --eq type:note` stdout is unchanged whether or not a refresh lands —
/// the staged change perturbs freshness, never the counted set.
fn stage_staleness(vault: &Path, marker: &str) {
    std::fs::write(
        vault.join("task1.md"),
        format!("---\ntype: task\nstatus: backlog\n---\nbody task {marker}\n"),
    )
    .unwrap();
}

/// A routed read under genuine daemon-side lock contention produces the SAME
/// (stdout, stderr, exit) triple as a direct read under the same contention —
/// the forwarded note included — and the note also lands in the daemon's own
/// stderr log (both surfaces, NRN-215). Each contended run is staged against a
/// STALE vault (see [`stage_staleness`] — required post-NRN-253, cf. NRN-260).
/// Debug-profile only — see the crate-level `cfg(debug_assertions)` gate above.
#[test]
fn routed_contended_read_forwards_the_note_byte_identically() {
    let vault = seed_vault();
    let args = &["count", "--eq", "type:note"][..];

    // ── Direct baseline (private cache home, no daemon socket) ──────────────
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();

    // Warm-up run builds the cache (uncontended — no note).
    let (warm_stdout, warm_stderr, warm_code) =
        run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    assert_eq!(warm_code, 0, "warm-up count must succeed");
    assert!(
        warm_stderr.is_empty(),
        "uncontended direct count must emit no note, got {:?}",
        String::from_utf8_lossy(&warm_stderr)
    );

    // Contended run: stage staleness (so the refresh is a genuine attempt, not a
    // no-change pass — see stage_staleness), then hold the cache write lock; the
    // refresh times out and the command proceeds against current cache state
    // with the stderr note.
    stage_staleness(vault.path(), "staged for the direct contended run");
    let held_direct = hold_cache_lock(direct_cache.path());
    let (d_stdout, d_stderr, d_code) =
        run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    drop(held_direct);
    assert_eq!(d_code, 0, "contended direct count still succeeds");
    assert_eq!(
        d_stdout, warm_stdout,
        "contended direct count serves the current cache state"
    );
    assert_eq!(
        String::from_utf8_lossy(&d_stderr),
        format!("{NOTE}\n"),
        "contended direct count emits exactly the note"
    );

    // ── Routed: daemon on its own private cache home, stderr to a file ──────
    // Short prefix/subdirs: the socket must fit macOS's ~104-byte sun_path.
    let daemon_root = tempfile::Builder::new().prefix("nn-").tempdir().unwrap();
    let cache_home = daemon_root.path().join("c");
    let state_home = daemon_root.path().join("s");
    let stderr_path = daemon_root.path().join("err");
    let stderr_file = File::create(&stderr_path).unwrap();
    let child = Command::new(norn_bin())
        .arg("serve")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        // The DAEMON acquires the write lock on routed reads, so the short
        // timeout must be in ITS environment.
        .env(LOCK_TIMEOUT_ENV, "200")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn norn serve");
    let _guard = serve_util::ChildGuard(child);
    wait_for_ready(&socket_path_for(&cache_home), Duration::from_secs(10));

    // Warm-up routed run builds the DAEMON's cache (uncontended — no note) and
    // proves routing is live (served marker).
    let (r_warm_stdout, r_warm_stderr, r_warm_code) =
        run_norn(&cache_home, &state_home, vault.path(), args);
    assert_eq!(r_warm_code, 0, "routed warm-up count must succeed");
    assert!(
        r_warm_stderr.is_empty(),
        "uncontended routed count must emit no note, got {:?}",
        String::from_utf8_lossy(&r_warm_stderr)
    );
    assert_eq!(
        r_warm_stdout, warm_stdout,
        "routed warm-up stdout must match direct"
    );
    assert_eq!(
        count_served(&stderr_path, "vault.count"),
        1,
        "the warm-up count must have been served by the daemon; stderr:\n{}",
        read_to_string(&stderr_path)
    );

    // Contended routed run: stage staleness AGAIN (the routed warm-up indexed the
    // previous staging; without a fresh change the warm probe reports Fresh and
    // the daemon never touches the WriteLock — NRN-253/NRN-260, see
    // stage_staleness), then hold the DAEMON's cache write lock. The daemon's
    // warm refresh times out, captures the note, forwards it in the envelope,
    // and the routed CLI re-emits it — the full (stdout, stderr, exit) triple
    // must equal the contended DIRECT triple, byte for byte.
    stage_staleness(vault.path(), "restaged for the routed contended run");
    let held_daemon = hold_cache_lock(&cache_home);
    let (r_stdout, r_stderr, r_code) = run_norn(&cache_home, &state_home, vault.path(), args);
    drop(held_daemon);
    assert_eq!(r_code, d_code, "routed contended exit must match direct");
    assert_eq!(
        r_stdout, d_stdout,
        "routed contended stdout must match direct"
    );
    assert_eq!(
        String::from_utf8_lossy(&r_stderr),
        String::from_utf8_lossy(&d_stderr),
        "routed contended stderr must carry the forwarded note byte-identically"
    );

    // Positive routing proof: the contended read was SERVED, not bounced to a
    // direct fallback that would have produced the note locally.
    assert_eq!(
        count_served(&stderr_path, "vault.count"),
        2,
        "the contended count must also have been served by the daemon; stderr:\n{}",
        read_to_string(&stderr_path)
    );

    // Both surfaces (NRN-215): the daemon's OWN stderr keeps the note as its
    // operational-log line, alongside the served markers.
    let daemon_log = read_to_string(&stderr_path);
    assert!(
        daemon_log.contains(NOTE),
        "the daemon's stderr must keep the contention note as its log line; got:\n{daemon_log}"
    );
}
