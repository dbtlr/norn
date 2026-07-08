//! NRN-83 acceptance benchmark — the daemon initiative's founding-bug proof.
//!
//! The founding bug: `Cache::open` runs `PRAGMA integrity_check` on every open,
//! an O(db-size) cost that dwarfs the actual query at scale. ADR 0005's
//! resolution relocated the check to the warm `norn serve` daemon
//! (open-once / verify-once); routed reads inherit the already-verified state.
//! This harness proves that end-to-end at 50k-doc scale, re-runnably.
//!
//! ## The structural (non-timing) acceptance observable
//!
//! `src/cache/open.rs` emits one `norn trace: integrity_check` stderr line per
//! `PRAGMA integrity_check` execution WHEN `NORN_TRACE_INTEGRITY_CHECK` is set.
//! Because a `Cache::open` on an EXISTING db always runs the check, this marker
//! is a deterministic, cross-process count of how many times a code path pays it:
//!
//!   * A **direct** invocation reopens the cache every time → one marker PER call.
//!   * The **warm daemon** opens the cache once and holds it → ONE marker across
//!     every routed read, no matter how many `count`/`find`/`get` calls arrive.
//!
//! So the acceptance criterion is asserted structurally, not by timing: after N
//! routed reads the daemon's stderr carries exactly ONE integrity_check marker,
//! while N direct reads carry N. Timings are collected as supporting operator
//! evidence, printed as a table under `--nocapture`. The `count_served` markers
//! (one per tools/call the daemon actually serves) prove the routed reads were
//! genuinely served warm and did not silently fall back to a direct open — so
//! the one-marker result is a real verify-once win, not a vacuous zero-traffic
//! artifact.
//!
//! Run it explicitly (it is `#[ignore]`-gated; the 50k build takes real seconds):
//!
//! ```text
//! cargo test --release --test integrity_benchmark -- --ignored --nocapture
//! ```
//!
//! Scale/seed are env-overridable: `NORN_BENCH_DOCS` (default 50000),
//! `NORN_BENCH_SEED` (default 83), `NORN_BENCH_ITERS` (default 5).

#![cfg(unix)]

#[path = "bench_util/mod.rs"]
mod bench_util;
#[path = "serve_util/mod.rs"]
mod serve_util;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bench_util::generate_vault;
use serve_util::{count_served, norn_bin, socket_path_for, wait_for_ready, ChildGuard};

/// The stable marker `src/cache/open.rs` emits per integrity_check under trace.
const INTEGRITY_MARKER: &str = "norn trace: integrity_check";

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Outcome of one `norn` invocation: exit code plus wall-clock and the count of
/// integrity_check markers on its OWN stderr (direct runs carry them here).
struct RunResult {
    code: i32,
    elapsed: Duration,
    integrity_markers: usize,
}

/// Run `norn --cwd <vault> <args…>` with a private cache/state home and the
/// integrity-check trace enabled. Returns the timed result.
fn run_norn(cache_home: &Path, state_home: &Path, vault: &Path, args: &[&str]) -> RunResult {
    let start = Instant::now();
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .env("NORN_TRACE_INTEGRITY_CHECK", "1")
        .arg("--cwd")
        .arg(vault)
        .args(args)
        .output()
        .expect("run norn");
    let elapsed = start.elapsed();
    let markers = String::from_utf8_lossy(&out.stderr)
        .matches(INTEGRITY_MARKER)
        .count();
    RunResult {
        code: out.status.code().unwrap_or(-1),
        elapsed,
        integrity_markers: markers,
    }
}

/// Spawn a daemon on `cache_home`, with integrity-check tracing on and its
/// stderr captured to `<root>/err`. Returns the guard, the stderr path, and the
/// tempdir root (kept alive so the socket path stays valid).
fn spawn_daemon_traced(cache_home: &Path, state_home: &Path) -> (ChildGuard, PathBuf) {
    let stderr_path = cache_home
        .parent()
        .expect("cache_home has a parent")
        .join("daemon-err");
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(norn_bin())
        .arg("serve")
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .env("NORN_TRACE_INTEGRITY_CHECK", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn norn serve");
    let guard = ChildGuard(child);
    wait_for_ready(&socket_path_for(cache_home), Duration::from_secs(10));
    (guard, stderr_path)
}

fn daemon_integrity_markers(stderr_path: &Path) -> usize {
    std::fs::read_to_string(stderr_path)
        .unwrap_or_default()
        .matches(INTEGRITY_MARKER)
        .count()
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// The three routed-read shapes exercised. `get` targets the first generated
/// doc (guaranteed to exist); `find` is filtered + limited so its JSON output
/// stays small while still driving a full indexed query.
fn read_shapes() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("count", vec!["count"]),
        (
            "find",
            vec![
                "find",
                "--eq",
                "type:note",
                "--limit",
                "5",
                "--format",
                "json",
            ],
        ),
        ("get", vec!["get", "doc-000000"]),
    ]
}

#[test]
#[ignore = "50k-doc acceptance benchmark; run explicitly with --ignored --nocapture"]
fn integrity_check_acceptance_50k() {
    let n = env_usize("NORN_BENCH_DOCS", 50_000);
    let seed = env_u64("NORN_BENCH_SEED", 83);
    let iters = env_usize("NORN_BENCH_ITERS", 5);
    assert!(
        iters >= 2,
        "need at least 2 iterations to separate cold/warm"
    );

    // ---- Generate the synthetic vault -----------------------------------
    let vault_tmp = tempfile::Builder::new()
        .prefix("nb-vault-")
        .tempdir()
        .unwrap();
    let gen_start = Instant::now();
    generate_vault(vault_tmp.path(), n, seed);
    let gen_elapsed = gen_start.elapsed();
    let vault = vault_tmp.path();

    // One shared cache home for direct baseline THEN daemon: no daemon socket
    // exists during the direct phase (so those calls run direct); the daemon
    // binds the same home afterward and routes against the already-built cache.
    // Short prefix keeps the socket path under macOS's ~104-byte sun_path.
    let home_tmp = tempfile::Builder::new().prefix("nb-").tempdir().unwrap();
    let cache_home = home_tmp.path().join("c");
    let state_home = home_tmp.path().join("s");
    std::fs::create_dir_all(&cache_home).unwrap();
    std::fs::create_dir_all(&state_home).unwrap();

    // Point THIS process's cache home at the private tempdir too, so the
    // in-process `cache_dir_for` call below resolves the same cache dir the
    // spawned `norn` children use (they receive it via explicit `.env()`).
    // Safe: this is the only test running under `--ignored`, and the
    // bench_util unit tests never read XDG_CACHE_HOME.
    std::env::set_var("XDG_CACHE_HOME", &cache_home);

    // ---- Build the cache once (Fresh open: no integrity_check yet) -------
    let build_start = Instant::now();
    let build = run_norn(&cache_home, &state_home, vault, &["count"]);
    let build_elapsed = build_start.elapsed();
    assert_eq!(build.code, 0, "initial cache build (count) must exit 0");
    assert_eq!(
        build.integrity_markers, 0,
        "the Fresh-open build must NOT run integrity_check (no db existed yet)"
    );
    // Resolve the vault's cache.db with the SAME identity mapping production
    // uses (the cache dir is vault-hash-derived under the home). A missing db
    // here means the build above did not do what this harness thinks it did —
    // fail loudly rather than reporting a 0-byte size.
    let vault_utf8 = camino::Utf8Path::from_path(vault).expect("utf8 vault path");
    let cache_dir = norn_run::resolve_cache_dir(vault_utf8).expect("resolve cache dir for vault");
    let cache_db = cache_dir.join("cache.db");
    let cache_db_bytes = std::fs::metadata(cache_db.as_std_path())
        .unwrap_or_else(|e| panic!("cache.db must exist at {cache_db} after the build: {e}"))
        .len();

    // ---- Direct baseline: each call reopens → pays integrity_check -------
    // This is ALSO the no-daemon regression guard (deliverable 3): a direct read
    // at 50k docs must STILL verify — exactly one integrity_check marker per call.
    let mut direct_medians: Vec<(&str, Duration)> = Vec::new();
    let mut direct_integrity_total = 0usize;
    for (label, args) in read_shapes() {
        let mut samples = Vec::with_capacity(iters);
        for _ in 0..iters {
            let r = run_norn(&cache_home, &state_home, vault, &args);
            assert_eq!(r.code, 0, "direct {label} must exit 0");
            assert_eq!(
                r.integrity_markers, 1,
                "direct {label} at {n} docs must pay EXACTLY one integrity_check \
                 (trust preserved on the no-daemon path)"
            );
            direct_integrity_total += r.integrity_markers;
            samples.push(r.elapsed);
        }
        direct_medians.push((label, median(samples)));
    }

    // ---- Routed: spawn the daemon on the SAME (already-built) cache home --
    let (_guard, daemon_stderr) = spawn_daemon_traced(&cache_home, &state_home);

    let mut routed_medians: Vec<(&str, Duration)> = Vec::new();
    let mut served_counts: Vec<(&str, usize)> = Vec::new();
    let mut total_routed_calls = 0usize;
    for (label, args) in read_shapes() {
        // Warm-only latency: skip the first call, whose cost may include the
        // daemon's single warm open. Calls 2..=iters are steady-state warm.
        let mut warm_samples = Vec::with_capacity(iters - 1);
        for i in 0..iters {
            let r = run_norn(&cache_home, &state_home, vault, &args);
            assert_eq!(r.code, 0, "routed {label} must exit 0");
            // A routed call carries NO integrity marker on its OWN stderr — the
            // check (if any) happens daemon-side, counted separately below.
            assert_eq!(
                r.integrity_markers, 0,
                "routed {label} must not run integrity_check in the CLI process"
            );
            if i > 0 {
                warm_samples.push(r.elapsed);
            }
            total_routed_calls += 1;
        }
        routed_medians.push((label, median(warm_samples)));

        let tool = match label {
            "count" => "vault.count",
            "find" => "vault.find",
            "get" => "vault.get",
            other => panic!("unmapped shape {other}"),
        };
        served_counts.push((label, count_served(&daemon_stderr, tool)));
    }

    // ---- STRUCTURAL ACCEPTANCE ASSERTION --------------------------------
    // The daemon opened the cache once and held it: across ALL routed reads its
    // stderr carries exactly ONE integrity_check marker. If routed reads paid the
    // check per invocation (the founding bug, un-fixed), this would be
    // total_routed_calls instead.
    let daemon_markers = daemon_integrity_markers(&daemon_stderr);
    assert_eq!(
        daemon_markers,
        1,
        "ACCEPTANCE: the warm daemon must pay integrity_check exactly ONCE across \
         all {total_routed_calls} routed reads (verify-once); got {daemon_markers}. \
         daemon stderr:\n{}",
        std::fs::read_to_string(&daemon_stderr).unwrap_or_default()
    );

    // Routing was real, not a vacuous fall-back to direct: each shape was served
    // `iters` times by the daemon.
    for (label, served) in &served_counts {
        assert_eq!(
            *served, iters,
            "routed {label} must have been SERVED {iters} times by the daemon, got {served}; \
             a lower count means it silently ran direct and the one-marker result above is vacuous"
        );
    }

    // Regression guard tally: direct reads verified every time.
    assert_eq!(
        direct_integrity_total,
        iters * read_shapes().len(),
        "every direct read must have paid integrity_check (no-daemon trust guard)"
    );

    // Supporting timing evidence only — NOT an acceptance gate. The structural
    // marker assertions above are the gates; wall-clock is machine- and
    // load-dependent (small NORN_BENCH_DOCS overrides, loaded CI machines), so
    // a slower routed median WARNs into the --nocapture record instead of
    // failing the run.
    let direct_count_med = direct_medians[0].1;
    let routed_count_med = routed_medians[0].1;
    if routed_count_med >= direct_count_med {
        println!(
            "WARN: routed warm count median ({routed_count_med:?}) was not faster than direct \
             ({direct_count_med:?}); timing is evidence only — the structural verify-once gates \
             above still held"
        );
    }

    // ---- Evidence table --------------------------------------------------
    println!("\n==================== NRN-83 integrity_check acceptance ====================");
    println!("documents generated      : {n}  (seed {seed})");
    println!("vault generation         : {gen_elapsed:?}");
    println!("cold cache build (count) : {build_elapsed:?}");
    println!("cache.db size            : {} bytes", cache_db_bytes);
    println!("iterations per shape     : {iters}");
    println!("--------------------------------------------------------------------------");
    println!(
        "{:<8} {:>18} {:>18} {:>10}",
        "shape", "direct median", "routed(warm) med", "served"
    );
    for i in 0..read_shapes().len() {
        let (label, d) = direct_medians[i];
        let (_, r) = routed_medians[i];
        let (_, served) = served_counts[i];
        println!("{label:<8} {:>18?} {:>18?} {:>10}", d, r, served);
    }
    println!("--------------------------------------------------------------------------");
    println!(
        "integrity_check markers  : direct = {} (one per read), daemon = {} (verify-once)",
        direct_integrity_total, daemon_markers
    );
    println!("routed reads served      : {total_routed_calls} total across all shapes");
    println!("==========================================================================\n");
}
