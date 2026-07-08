//! End-to-end proof that `norn find` / `norn get` route through the warm daemon
//! byte-identically (NRN-222), including the CLI-side gates and exit signals
//! that the wire does not carry natively.
//!
//! Mirrors `serve_count_routing.rs`: direct output is captured against a private
//! cache home with no daemon socket; routed output against the running daemon's
//! cache home. Routed and direct must match on the FULL (stdout, stderr, exit)
//! triple.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{norn_bin, socket_path_for, wait_for_ready, ChildGuard};

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// Seed a vault: 2 `type: note`, 1 `type: task`.
fn seed_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-fg-route-vault-")
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

/// Run `norn --cwd <vault> <args…>` with the given cache/state homes.
/// Returns `(stdout, stderr, exit_code)`.
fn run_norn(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        // Generous handshake budget so a daemon scheduled late under CI load
        // still answers the probe (see serve_count_routing.rs).
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .arg("--cwd")
        .arg(vault)
        .args(args)
        .output()
        .expect("run norn");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Spawn a daemon on a private cache home, capturing its stderr to a file.
/// Returns (guard, cache_home, state_home, stderr_path, root_tmp).
fn spawn_daemon_logged() -> (
    ChildGuard,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    TempDir,
) {
    // Short prefix/subdirs: the socket must fit macOS's ~104-byte sun_path.
    let daemon_root = tempfile::Builder::new().prefix("nf-").tempdir().unwrap();
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
    let guard = ChildGuard(child);
    let socket = socket_path_for(&cache_home);
    wait_for_ready(&socket, Duration::from_secs(10));
    (guard, cache_home, state_home, stderr_path, daemon_root)
}

/// A `get` whose target does not resolve must be SERVED by the daemon — exit 1
/// derived from the wire's `error:` note — not bounced to a second, direct
/// execution (NRN-222 review F3). `vault.get` maps the not-found signal to
/// `isError: true`; the routed client must accept that result (it carries the
/// full structuredContent) and reproduce the CLI contract from it, instead of
/// treating it as a transport failure and re-running the whole query directly.
#[test]
fn routed_get_not_found_exits_1_without_direct_fallback() {
    let vault = seed_vault();

    // Direct baseline (no daemon socket in this cache home).
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let args = &["get", "no-such-doc"][..];
    let (d_stdout, d_stderr, d_code) =
        run_norn(direct_cache.path(), direct_state.path(), vault.path(), args);
    assert_eq!(d_code, 1, "direct get of a missing target exits 1");
    assert!(
        String::from_utf8_lossy(&d_stderr).contains("error:"),
        "direct get carries the error: note on stderr"
    );

    // Routed: byte-identical triple against a live daemon.
    let (_guard, cache_home, state_home, _stderr_path, _root) = spawn_daemon_logged();
    let (r_stdout, r_stderr, r_code) = run_norn(&cache_home, &state_home, vault.path(), args);
    assert_eq!(r_code, d_code, "routed get must exit 1 like direct");
    assert_eq!(r_stdout, d_stdout, "routed get stdout must match direct");
    assert_eq!(r_stderr, d_stderr, "routed get stderr must match direct");

    // The routing proof: with --verbose, a fall-back to Direct prints a
    // "using direct execution" diagnostic. The isError result carries the full
    // structuredContent, so the routed client must handle it WITHOUT falling
    // back — a fallback here means the failing get executed twice.
    let verbose_args = &["--verbose", "get", "no-such-doc"][..];
    let (_v_stdout, v_stderr, v_code) =
        run_norn(&cache_home, &state_home, vault.path(), verbose_args);
    let v_stderr_text = String::from_utf8_lossy(&v_stderr);
    assert_eq!(v_code, 1, "verbose routed get still exits 1");
    assert!(
        !v_stderr_text.contains("using direct execution"),
        "a not-found get must be served by the daemon, not re-executed \
         directly; verbose stderr shows a fallback:\n{v_stderr_text}"
    );
}

/// The missing-predicate help gate must hold on the routed path exactly as on
/// the direct path: a bare `norn find` (no predicates, no --all) — and its
/// `--text ""` twin, which `has_predicate` treats as no predicate — prints the
/// find help to stderr and exits 2 instead of dumping the vault through the
/// daemon (NRN-222 review F1).
#[test]
fn routed_find_respects_missing_predicate_gate() {
    let vault = seed_vault();

    // Direct baselines (no daemon socket in this cache home).
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let gate_shapes: Vec<Vec<&str>> = vec![vec!["find"], vec!["find", "--text", ""]];
    let direct: Vec<_> = gate_shapes
        .iter()
        .map(|shape| {
            run_norn(
                direct_cache.path(),
                direct_state.path(),
                vault.path(),
                shape,
            )
        })
        .collect();
    for (shape, (stdout, stderr, code)) in gate_shapes.iter().zip(direct.iter()) {
        assert_eq!(*code, 2, "direct bare {shape:?} must exit 2 (help gate)");
        assert!(
            stdout.is_empty(),
            "direct bare {shape:?} must print nothing to stdout"
        );
        assert!(
            !stderr.is_empty(),
            "direct bare {shape:?} must print help to stderr"
        );
    }

    // Routed: same commands against a live daemon must behave identically.
    let (_guard, cache_home, state_home, _stderr_path, _root) = spawn_daemon_logged();
    for (shape, (direct_stdout, direct_stderr, direct_code)) in
        gate_shapes.iter().zip(direct.iter())
    {
        let (stdout, stderr, code) = run_norn(&cache_home, &state_home, vault.path(), shape);
        assert_eq!(
            code, *direct_code,
            "routed bare {shape:?} must exit 2 like direct (help gate), got {code}"
        );
        assert_eq!(
            stdout,
            *direct_stdout,
            "routed bare {shape:?} stdout must match direct\nrouted: {:?}",
            String::from_utf8_lossy(&stdout)
        );
        assert_eq!(
            stderr, *direct_stderr,
            "routed bare {shape:?} stderr must match direct"
        );
    }
}
