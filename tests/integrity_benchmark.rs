//! NRN-83/232 acceptance benchmark — the daemon initiative's founding-bug proof,
//! extended from reads to routed WRITES.
//!
//! The founding bug: `Cache::open` runs `PRAGMA integrity_check` on every open,
//! an O(db-size) cost that dwarfs the actual query at scale. ADR 0005's
//! resolution relocated the check to the warm `norn serve` daemon
//! (open-once / verify-once); routed reads inherit the already-verified state.
//! NRN-229 then routed `norn set` (and the other mutation commands) through the
//! same daemon. This harness proves the verify-once win end-to-end at 50k-doc
//! scale for BOTH halves of the daemon's traffic — reads and writes — in one
//! daemon lifetime, re-runnably.
//!
//! ## The structural (non-timing) acceptance observable
//!
//! `src/cache/open.rs` emits one `norn trace: integrity_check` stderr line per
//! `PRAGMA integrity_check` execution WHEN `NORN_TRACE_INTEGRITY_CHECK` is set.
//! Because a `Cache::open` on an EXISTING db always runs the check, this marker
//! is a deterministic, cross-process count of how many times a code path pays it:
//!
//!   * A **direct** invocation reopens the cache every time. `count`/`find`/`get`
//!     open it once per call → one marker per call. `set` (unlike the reads)
//!     opens the cache TWICE per direct call — `crate::cache::command::load_graph_index`
//!     (the planning `GraphIndex`) and `crate::cache::command::open_for_query` (target
//!     resolution) are two separate `Cache::open_with_index` calls in the direct
//!     dispatch (`src/lib.rs`, `Command::Set`; tracked as seed NRN-s15) — so the
//!     harness asserts EXACTLY two markers per direct `set` call, a hard pin
//!     that fails on drift in either direction (a third open, or the two opens
//!     collapsing into one).
//!   * The **warm daemon** opens the cache once and holds it → ONE marker across
//!     every routed call, no matter how many reads OR writes arrive. `vault.set`
//!     collapses the same two needs (index + query cache) into a SINGLE
//!     `ctx.query_cache()` + `cache.load_graph_index()` call over its resident
//!     connection (NRN-130), so the routed write path never re-pays the check
//!     the direct path pays twice.
//!
//! So the acceptance criterion is asserted structurally, not by timing: after N
//! routed reads AND M routed writes (plus one post-apply verification read) the
//! daemon's stderr carries exactly ONE integrity_check marker total, while each
//! direct call carries its own (constant, per-shape) non-zero count. Timings are
//! collected as supporting operator evidence, printed as a table under
//! `--nocapture`. The `count_served` markers (one per tools/call the daemon
//! actually serves) prove the routed calls were genuinely served warm and did
//! not silently fall back to a direct open — so the one-marker result is a real
//! verify-once win, not a vacuous zero-traffic artifact.
//!
//! ## Write shape
//!
//! The write phases exercise `set <doc> --field bench_status=<value> --yes` —
//! the routable `set` surface (NRN-229; `--field-json`/`--push`/`--pop`/
//! `--body-from-stdin` stay gated to Direct and would make routing vacuous).
//! `bench_status` is a field the synthetic vault's generated docs never declare
//! and the vault carries no `.norn/config.yaml` (see `bench_util::generate_vault`),
//! so it is schema-UNKNOWN: the set applies cleanly (exit 0) with a harmless
//! `unknown field` warning, never a `--force` bypass. Each iteration writes a
//! distinct value (`v0`, `v1`, …) so no call is a no-op. The direct-write phase
//! targets `doc-000001` and the routed-write phase targets `doc-000002` —
//! distinct from each other and from the read phase's `get` target
//! (`doc-000000`) — so the three phases never race on the same file.
//!
//! Run it explicitly (it is `#[ignore]`-gated; the 50k build takes real seconds):
//!
//! ```text
//! cargo test --release --test integrity_benchmark -- --ignored --nocapture
//! ```
//!
//! Scale/seed are env-overridable: `NORN_BENCH_DOCS` (default 50000),
//! `NORN_BENCH_SEED` (default 83), `NORN_BENCH_ITERS` (default 5),
//! `NORN_BENCH_READER_CLIENTS` (default 4), and
//! `NORN_BENCH_READS_PER_CLIENT` (default 3).

#![cfg(unix)]

#[path = "bench_util/mod.rs"]
mod bench_util;
#[path = "serve_util/mod.rs"]
mod serve_util;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
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

/// Outcome of one `norn` invocation: exit code plus wall-clock, the count of
/// integrity_check markers on its OWN stderr (direct runs carry them here), and
/// captured stdout (used by the post-apply state check to parse `get --format
/// json`; every other call site ignores it).
struct RunResult {
    code: i32,
    elapsed: Duration,
    integrity_markers: usize,
    stdout: Vec<u8>,
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
        stdout: out.stdout,
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

/// Frontmatter field used for the write phases: absent from every generated doc
/// and from the vault's (nonexistent) schema config, so a set only warns
/// (`unknown field`) rather than refusing — see the module doc's "Write shape"
/// section.
const BENCH_FIELD: &str = "bench_status";

/// Direct-write target: distinct from the read phase's `get doc-000000` and
/// from the routed-write target below, so no phase races another's file.
const DIRECT_SET_DOC: &str = "doc-000001";

/// Routed-write target: distinct from `DIRECT_SET_DOC` and from `doc-000000`.
const ROUTED_SET_DOC: &str = "doc-000002";

/// Build the routable `set <doc> --field bench_status=<value> --yes` argv for
/// iteration `i` (value `v{i}`), so repeated sets are never no-ops.
fn set_args(doc: &str, i: usize) -> Vec<String> {
    vec![
        "set".to_string(),
        doc.to_string(),
        "--field".to_string(),
        format!("{BENCH_FIELD}=v{i}"),
        "--yes".to_string(),
    ]
}

#[test]
#[ignore = "50k-doc acceptance benchmark; run explicitly with --ignored --nocapture"]
fn integrity_check_acceptance_50k() {
    let n = env_usize("NORN_BENCH_DOCS", 50_000);
    let seed = env_u64("NORN_BENCH_SEED", 83);
    let iters = env_usize("NORN_BENCH_ITERS", 5);
    let reader_clients = env_usize("NORN_BENCH_READER_CLIENTS", 4);
    let reads_per_client = env_usize("NORN_BENCH_READS_PER_CLIENT", 3);
    assert!(
        iters >= 2,
        "need at least 2 iterations to separate cold/warm"
    );
    assert!(
        n >= 3,
        "benchmark needs ≥3 docs: reads doc-000000, direct-set doc-000001, \
         routed-set doc-000002"
    );
    assert!(reader_clients > 0, "need at least 1 concurrent reader");
    assert!(reads_per_client > 0, "need at least 1 read per client");

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
    // uses (the cache dir is vault-hash-derived under the home), passing the
    // private cache home EXPLICITLY — a pure function of the same value the
    // spawned children receive via `.env()`, with zero process-env mutation in
    // this test binary. A missing db here means the build above did not do
    // what this harness thinks it did — fail loudly rather than reporting a
    // 0-byte size.
    let vault_utf8 = camino::Utf8Path::from_path(vault).expect("utf8 vault path");
    let cache_home_utf8 = camino::Utf8Path::from_path(&cache_home).expect("utf8 cache home");
    let cache_dir = norn_run::resolve_cache_dir_in(cache_home_utf8, vault_utf8)
        .expect("resolve cache dir for vault");
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

    // ---- Direct write baseline: `set` BEFORE any daemon exists -----------
    // This MUST run before the daemon spawns below — a live socket would route
    // these calls instead of exercising the no-daemon direct write path.
    //
    // Unlike the single-open reads, the direct `set` dispatch pays
    // integrity_check EXACTLY TWICE per call: `src/lib.rs`'s `Command::Set` arm
    // opens the cache once via `cache::command::load_graph_index` (the planning
    // `GraphIndex`) and again via `cache::command::open_for_query` (target
    // resolution) — two separate `Cache::open_with_index` sites. The value 2 is
    // pinned hard (not first-call-captured) so uniform drift in EITHER
    // direction fails loudly: a 2→3 regression (a third open) AND a 2→1
    // improvement (the two opens collapsed, e.g. mirroring the daemon-side
    // NRN-130 consolidation — a change this benchmark should celebrate by
    // updating the pin, not absorb silently). Tracked as seed NRN-s15.
    const DIRECT_SET_MARKERS_PER_CALL: usize = 2;
    let mut direct_set_samples = Vec::with_capacity(iters);
    for i in 0..iters {
        let args = set_args(DIRECT_SET_DOC, i);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = run_norn(&cache_home, &state_home, vault, &arg_refs);
        assert_eq!(r.code, 0, "direct set (iter {i}) must exit 0");
        assert_eq!(
            r.integrity_markers, DIRECT_SET_MARKERS_PER_CALL,
            "direct set (iter {i}) must pay integrity_check exactly TWICE \
             (load_graph_index + open_for_query in src/lib.rs's Command::Set arm; \
             NRN-s15) — trust preserved on the no-daemon write path; got {}",
            r.integrity_markers
        );
        direct_set_samples.push(r.elapsed);
    }
    let direct_set_median = median(direct_set_samples);
    let direct_set_integrity_total = DIRECT_SET_MARKERS_PER_CALL * iters;

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

    // Routing was real, not a vacuous fall-back to direct: each READ shape was
    // served `iters` times by the daemon so far (before any write traffic).
    for (label, served) in &served_counts {
        assert_eq!(
            *served, iters,
            "routed {label} must have been SERVED {iters} times by the daemon, got {served}; \
             a lower count means it silently ran direct and the acceptance result below \
             would be vacuous"
        );
    }

    // ---- Routed writes: SAME daemon lifetime as the routed reads above ----
    // `set` on `ROUTED_SET_DOC` — distinct from every doc touched above — using
    // the SAME routable shape (`--field KEY=VALUE --yes`) NRN-229 routes.
    let mut routed_set_warm_samples = Vec::with_capacity(iters - 1);
    for i in 0..iters {
        let args = set_args(ROUTED_SET_DOC, i);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = run_norn(&cache_home, &state_home, vault, &arg_refs);
        assert_eq!(r.code, 0, "routed set (iter {i}) must exit 0");
        assert_eq!(
            r.integrity_markers, 0,
            "routed set (iter {i}) must not run integrity_check in the CLI process"
        );
        if i > 0 {
            routed_set_warm_samples.push(r.elapsed);
        }
    }
    let routed_set_median = median(routed_set_warm_samples);

    // Non-vacuous routing for the write phase: the daemon actually SERVED every
    // routed set, not a silent fall-back to Direct.
    let routed_set_served = count_served(&daemon_stderr, "vault.set");
    assert_eq!(
        routed_set_served, iters,
        "routed set must have been SERVED {iters} times by the daemon, got {routed_set_served}; \
         a lower count means it silently ran direct and the acceptance result below would be vacuous"
    );

    // ---- Post-apply state check: the routed writes genuinely landed --------
    // One more routed `get` (JSON) of the routed-write target must show the
    // FINAL iteration's value. This call is itself routed too, so it is folded
    // into the `vault.get` served-count arithmetic below rather than ignored.
    let final_value = format!("v{}", iters - 1);
    let post_apply_get = run_norn(
        &cache_home,
        &state_home,
        vault,
        &["get", ROUTED_SET_DOC, "--format", "json"],
    );
    assert_eq!(post_apply_get.code, 0, "post-apply routed get must exit 0");
    assert_eq!(
        post_apply_get.integrity_markers, 0,
        "post-apply routed get must not run integrity_check in the CLI process"
    );
    let post_apply_json: serde_json::Value = serde_json::from_slice(&post_apply_get.stdout)
        .unwrap_or_else(|e| {
            panic!(
                "post-apply get must emit valid JSON: {e}\nstdout: {}",
                String::from_utf8_lossy(&post_apply_get.stdout)
            )
        });
    let post_apply_value = post_apply_json[0]["frontmatter"][BENCH_FIELD].as_str();
    assert_eq!(
        post_apply_value,
        Some(final_value.as_str()),
        "post-apply get of {ROUTED_SET_DOC} must show the FINAL routed-write value \
         ({final_value}), proving the writes genuinely landed through the daemon; got {post_apply_json}"
    );

    // `vault.get` was served `iters` times during the read phase, plus this one
    // post-apply verification call — keep the arithmetic honest rather than
    // re-baselining the count.
    let get_served_total = count_served(&daemon_stderr, "vault.get");
    assert_eq!(
        get_served_total,
        iters + 1,
        "vault.get must have been served {iters} times in the read phase plus 1 for the \
        post-apply verification get, got {get_served_total}"
    );

    // ---- Concurrent routed reader fanout: SAME warm daemon --------------
    // K client threads synchronize at a barrier before EACH read, then each
    // spawns one real routed CLI process. This yields K concurrent processes
    // per round for R rounds while retaining one latency sample per child.
    let fanout_expected = reader_clients * reads_per_client;
    let count_served_before_fanout = count_served(&daemon_stderr, "vault.count");
    let barrier = Arc::new(Barrier::new(reader_clients));
    let fanout_cache_home = cache_home.as_path();
    let fanout_state_home = state_home.as_path();
    let fanout_start = Instant::now();
    let fanout_results = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(reader_clients);
        for client in 0..reader_clients {
            let barrier = Arc::clone(&barrier);
            handles.push(scope.spawn(move || {
                let mut results = Vec::with_capacity(reads_per_client);
                for _ in 0..reads_per_client {
                    barrier.wait();
                    results.push(run_norn(
                        fanout_cache_home,
                        fanout_state_home,
                        vault,
                        &["count"],
                    ));
                }
                (client, results)
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().expect("fanout client thread must not panic"))
            .collect::<Vec<_>>()
    });
    let fanout_makespan = fanout_start.elapsed();

    let mut fanout_samples = Vec::with_capacity(fanout_expected);
    for (client, results) in fanout_results {
        for (read, result) in results.into_iter().enumerate() {
            assert_eq!(
                result.code, 0,
                "fanout client {client} read {read} must exit 0"
            );
            assert_eq!(
                result.integrity_markers, 0,
                "fanout client {client} read {read} must not run integrity_check in the CLI process"
            );
            fanout_samples.push(result.elapsed);
        }
    }
    assert_eq!(
        fanout_samples.len(),
        fanout_expected,
        "fanout latency accounting must contain exactly K*R samples"
    );
    let count_served_after_fanout = count_served(&daemon_stderr, "vault.count");
    let count_served_fanout_delta = count_served_after_fanout
        .checked_sub(count_served_before_fanout)
        .expect("daemon vault.count served count must not decrease");
    assert_eq!(
        count_served_fanout_delta, fanout_expected,
        "daemon vault.count served-count delta must be exactly K*R"
    );

    fanout_samples.sort();
    let fanout_median = fanout_samples[fanout_samples.len() / 2];
    let fanout_p95_rank = (fanout_samples.len() * 95).div_ceil(100);
    let fanout_p95 = fanout_samples[fanout_p95_rank - 1];
    let fanout_max = *fanout_samples.last().expect("fanout samples are non-empty");

    // ---- STRUCTURAL ACCEPTANCE ASSERTION --------------------------------
    // The daemon opened the cache once and held it: across ALL routed traffic —
    // sequential reads, writes, the post-apply verification read, AND the
    // concurrent reader fanout — its stderr carries exactly ONE
    // integrity_check marker. If routed calls paid the check per invocation
    // (the founding bug, un-fixed, or a regression that never inherited the
    // verify-once win), this would be `total_routed_all` instead.
    let total_routed_all = total_routed_calls + iters + 1 + fanout_expected;
    let daemon_markers = daemon_integrity_markers(&daemon_stderr);
    assert_eq!(
        daemon_markers,
        1,
        "ACCEPTANCE: the warm daemon must pay integrity_check exactly ONCE across \
         all {total_routed_all} routed reads+writes (verify-once); got {daemon_markers}. \
         daemon stderr:\n{}",
        std::fs::read_to_string(&daemon_stderr).unwrap_or_default()
    );

    // Regression guard tally: direct reads verified every time. (The direct-set
    // side needs no tally here — its per-call count is hard-pinned to
    // DIRECT_SET_MARKERS_PER_CALL inside the loop above.)
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
    if routed_set_median >= direct_set_median {
        println!(
            "WARN: routed warm set median ({routed_set_median:?}) was not faster than direct \
             ({direct_set_median:?}); timing is evidence only — the structural verify-once gates \
             above still held"
        );
    }

    // ---- Evidence table --------------------------------------------------
    println!("\n================= NRN-83/232 integrity_check acceptance ==================");
    println!("documents generated      : {n}  (seed {seed})");
    println!("vault generation         : {gen_elapsed:?}");
    println!("cold cache build (count) : {build_elapsed:?}");
    println!("cache.db size            : {} bytes", cache_db_bytes);
    println!("iterations per shape     : {iters}");
    println!(
        "concurrent reader fanout : {reader_clients} clients × {reads_per_client} reads = \
         {fanout_expected} routed count calls"
    );
    println!("write field / target     : {BENCH_FIELD} on {DIRECT_SET_DOC} (direct) / {ROUTED_SET_DOC} (routed)");
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
    println!(
        "{:<8} {:>18?} {:>18?} {:>10}",
        "set", direct_set_median, routed_set_median, routed_set_served
    );
    println!("--------------------------------------------------------------------------");
    println!(
        "integrity_check markers  : direct reads = {} ({} per call), direct writes = {} \
         ({} per call), daemon = {} (verify-once across reads+writes)",
        direct_integrity_total,
        1,
        direct_set_integrity_total,
        DIRECT_SET_MARKERS_PER_CALL,
        daemon_markers
    );
    println!(
        "routed calls served      : {total_routed_calls} reads + {iters} writes + 1 \
         post-apply verification get + {fanout_expected} concurrent reads = {total_routed_all} total"
    );
    println!(
        "fanout latency evidence  : median {fanout_median:?}, p95 {fanout_p95:?}, \
         max {fanout_max:?}, makespan {fanout_makespan:?}"
    );
    println!("==========================================================================\n");
}
