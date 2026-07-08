//! End-to-end proof that `norn count` routes through the warm daemon
//! byte-identically (NRN-94), and falls back to a direct open when no daemon is
//! present.
//!
//! The load-bearing invariant: routed and direct output must be BYTE-IDENTICAL
//! for every format and arg shape. We capture the direct output (a CLI run whose
//! private `XDG_CACHE_HOME` has no daemon socket) and the routed output (a CLI
//! run whose `XDG_CACHE_HOME` is the running daemon's) and diff them exactly.
//! The daemon's stderr is captured to a file so we can prove routing actually
//! happened rather than silently falling back — which would make the diff
//! trivially pass. The proof is per-CALL (NRN-94 review F6): the daemon emits one
//! "served vault.count" line for each tools/call it actually serves, so we assert
//! the count equals the number of routed shapes (an envelope break that kills
//! routing after the vault is open goes red), and that `--no-cache-refresh` does
//! NOT increment it (F1).

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, norn_bin, socket_path_for, wait_for_ready, ChildGuard};

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// Seed a vault: 2 `type: note`, 1 `type: task` with a `status`, so total,
/// single-key, and multi-key grouping all have something to count.
fn seed_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-count-route-vault-")
        .tempdir()
        .expect("tempdir");
    std::fs::write(
        tmp.path().join("note1.md"),
        "---\ntype: note\nstatus: active\ntitle: Note One\n---\nbody one\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("note2.md"),
        "---\ntype: note\nstatus: backlog\ntitle: Note Two\n---\nbody two\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("task1.md"),
        "---\ntype: task\nstatus: backlog\ntitle: Task One\n---\nbody task\n",
    )
    .unwrap();
    tmp
}

/// Run `norn --cwd <vault> count <extra_args>` with the given cache/state homes.
/// Returns `(stdout, stderr, exit_code)`.
fn run_count(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    extra_args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        // F8: a generous handshake budget so a daemon scheduled late under CI
        // load still answers the probe. Without it the 250ms default can trip
        // and a shape silently falls back to Direct — which the per-shape
        // routing proof below now catches as a HARD failure, so an unbounded
        // handshake budget here is the difference between a real regression and
        // a scheduler-jitter flake. Harmless on direct runs (no socket to find).
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .arg("--cwd")
        .arg(vault)
        .arg("count")
        .args(extra_args)
        .output()
        .expect("run norn count");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// The arg shapes exercised — one per `CountOutput` variant × format, plus a
/// filtered case, so byte-identity covers the whole reconstruct+render surface.
fn arg_shapes() -> Vec<Vec<&'static str>> {
    vec![
        vec![],                                          // total, text
        vec!["--format", "json"],                        // total, json
        vec!["--by", "type"],                            // grouped, text
        vec!["--by", "type", "--format", "json"],        // grouped, json
        vec!["--by", "type,status", "--format", "json"], // multi, json
        vec!["--eq", "type:note", "--format", "json"],   // filtered, json
        vec!["--by", "status", "--eq", "type:note"],     // filtered grouped, text
    ]
}

/// With NO daemon, every shape runs the direct path cleanly (exit 0) — the
/// fallback-is-total half of the invariant.
#[test]
fn no_daemon_runs_direct() {
    let vault = seed_vault();
    let cache = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    for shape in arg_shapes() {
        let (stdout, _stderr, code) = run_count(cache.path(), state.path(), vault.path(), &shape);
        assert_eq!(code, 0, "count {shape:?} should exit 0 with no daemon");
        assert!(
            !stdout.is_empty(),
            "count {shape:?} should print something with no daemon"
        );
    }
}

/// NRN-218: a dynamic-field predicate (`count --type note`) must ROUTE warm now,
/// byte-identically to direct, and an UNKNOWN dynamic field (`count
/// --nonexistentfield x`) must be REFUSED daemon-side with the exact stderr +
/// exit code the direct field-universe gate produces (served, not bounced).
#[test]
fn routed_dynamic_field_count_matches_direct() {
    let vault = seed_vault();

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();

    // KNOWN dynamic field: `--type note` desugars to `--eq type:note`.
    let known = &["--type", "note"][..];
    let d_known = run_count(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        known,
    );
    assert_eq!(d_known.2, 0, "direct known dynamic-field count exits 0");

    // UNKNOWN dynamic field: refused by the gate (exit 1).
    let unknown = &["--nonexistentfield", "x"][..];
    let d_unknown = run_count(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        unknown,
    );
    assert_eq!(d_unknown.2, 1, "direct unknown dynamic-field count exits 1");
    assert!(
        String::from_utf8_lossy(&d_unknown.1).contains("unknown field `nonexistentfield`"),
        "direct unknown field carries the gate message, got: {:?}",
        String::from_utf8_lossy(&d_unknown.1)
    );

    // Spawn a daemon on its own cache home, capturing stderr.
    let daemon_root = tempfile::Builder::new().prefix("nc-").tempdir().unwrap();
    let cache_home = daemon_root.path().join("c");
    let state_home = daemon_root.path().join("s");
    let stderr_path = daemon_root.path().join("err");
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(norn_bin())
        .arg("serve")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn norn serve");
    let _guard = ChildGuard(child);
    let socket = socket_path_for(&cache_home);
    wait_for_ready(&socket, Duration::from_secs(10));

    let r_known = run_count(&cache_home, &state_home, vault.path(), known);
    assert_eq!(
        r_known, d_known,
        "routed KNOWN dynamic-field count must match direct on (stdout, stderr, code)"
    );

    let r_unknown = run_count(&cache_home, &state_home, vault.path(), unknown);
    assert_eq!(
        r_unknown, d_unknown,
        "routed UNKNOWN dynamic-field count must be byte-identical to direct\nrouted: {:?}\ndirect: {:?}",
        (
            String::from_utf8_lossy(&r_unknown.0),
            String::from_utf8_lossy(&r_unknown.1),
            r_unknown.2
        ),
        (
            String::from_utf8_lossy(&d_unknown.0),
            String::from_utf8_lossy(&d_unknown.1),
            d_unknown.2
        ),
    );

    // Both dynamic-predicate counts (known + the refusal) must have been SERVED.
    let served = count_served(&stderr_path, "vault.count");
    assert_eq!(
        served,
        2,
        "the daemon must have served BOTH dynamic-field counts (known + unknown), got {served}; \
         daemon stderr:\n{}",
        std::fs::read_to_string(&stderr_path).unwrap_or_default()
    );
}

/// Routed output is byte-identical to direct output for every shape, and the
/// daemon actually served the request (proven via its stderr log).
#[test]
fn routed_count_is_byte_identical_to_direct() {
    let vault = seed_vault();

    // ── Direct captures: a private cache home with no daemon socket. Capture
    //    the FULL triple (stdout, stderr, exit code) per shape so routing can be
    //    proven identical on all three, not just stdout (NRN-94 review F7). ──
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let direct: Vec<(Vec<u8>, Vec<u8>, i32)> = arg_shapes()
        .iter()
        .map(|shape| {
            run_count(
                direct_cache.path(),
                direct_state.path(),
                vault.path(),
                shape,
            )
        })
        .collect();

    // ── Spawn a daemon on its own cache home, capturing its stderr. Keep the
    //    prefix + subdir names SHORT: the control socket lands at
    //    `<cache_home>/norn/run/norn.sock`, which must fit macOS's ~104-byte
    //    `sun_path` limit under the long `/var/folders/...` temp base. ──
    let daemon_root = tempfile::Builder::new().prefix("nc-").tempdir().unwrap();
    let cache_home = daemon_root.path().join("c");
    let state_home = daemon_root.path().join("s");
    let stderr_path = daemon_root.path().join("err");
    let stderr_file = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(norn_bin())
        .arg("serve")
        .env("XDG_CACHE_HOME", &cache_home)
        .env("XDG_STATE_HOME", &state_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn norn serve");
    let _guard = ChildGuard(child);
    let socket = socket_path_for(&cache_home);
    {
        // Inline the readiness wait so a startup failure surfaces the daemon's
        // own stderr instead of a bare "not ready" panic.
        use std::time::Instant;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if socket.exists() {
                break;
            }
            if Instant::now() >= deadline {
                let log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                panic!("daemon socket never appeared at {socket:?}; daemon stderr:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    wait_for_ready(&socket, Duration::from_secs(10));

    // ── Routed captures: the CLI's cache home IS the daemon's, so its probe
    //    finds the live socket and routes. Assert the FULL triple matches direct
    //    (F7): stdout, stderr, AND exit code, byte-for-byte, per shape. ──
    for (shape, (direct_stdout, direct_stderr, direct_code)) in
        arg_shapes().iter().zip(direct.iter())
    {
        let (routed_stdout, routed_stderr, routed_code) =
            run_count(&cache_home, &state_home, vault.path(), shape);

        assert_eq!(
            routed_stdout,
            *direct_stdout,
            "routed stdout must be byte-identical to direct for count {shape:?}\n\
             routed: {:?}\n direct: {:?}",
            String::from_utf8_lossy(&routed_stdout),
            String::from_utf8_lossy(direct_stdout),
        );
        assert_eq!(
            routed_stderr,
            *direct_stderr,
            "routed stderr must be byte-identical to direct for count {shape:?}\n\
             routed: {:?}\n direct: {:?}",
            String::from_utf8_lossy(&routed_stderr),
            String::from_utf8_lossy(direct_stderr),
        );
        assert_eq!(
            routed_code, *direct_code,
            "routed exit code must match direct for count {shape:?}"
        );
    }

    // ── Per-shape routing proof (NRN-94 review F6). The daemon emits one
    //    "served vault.count" line for EACH actually-served tools/call (unlike
    //    "opened vault", which fires once at hello time and so cannot detect an
    //    envelope break that kills routing after the vault is open). Assert the
    //    count equals the number of routed shapes: if even ONE shape silently
    //    fell back to Direct, or an rmcp change killed the tool call entirely,
    //    this is short and the test goes red. ──
    let daemon_log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        daemon_log.contains("opened vault"),
        "the daemon must have opened the vault at least once, got stderr:\n{daemon_log}"
    );
    let served = count_served(&stderr_path, "vault.count");
    assert_eq!(
        served,
        arg_shapes().len(),
        "the daemon must have SERVED exactly one vault.count per routed shape \
         ({} expected), got {served}; daemon stderr:\n{daemon_log}",
        arg_shapes().len(),
    );

    // ── F1: `--no-cache-refresh` must NOT route (the daemon always serves a
    //    freshly-refreshed cache, so routing that flag could differ from direct
    //    on a stale cache). Prove it two ways: output is byte-identical to a
    //    DIRECT `--no-cache-refresh` run, and the daemon's served counter does
    //    NOT increment (no new tools/call reached it). ──
    let ncr = &["--no-cache-refresh"][..];
    let direct_ncr = run_count(direct_cache.path(), direct_state.path(), vault.path(), ncr);
    let routed_ncr = run_count(&cache_home, &state_home, vault.path(), ncr);
    assert_eq!(
        routed_ncr, direct_ncr,
        "--no-cache-refresh must produce the same (stdout, stderr, code) as a direct run"
    );
    let served_after = count_served(&stderr_path, "vault.count");
    assert_eq!(
        served_after, served,
        "--no-cache-refresh must NOT route: the daemon's served counter must not \
         increment (was {served}, now {served_after})"
    );
}
