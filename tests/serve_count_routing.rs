//! End-to-end proof that `norn count` routes through the warm daemon
//! byte-identically (NRN-94), and falls back to a direct open when no daemon is
//! present.
//!
//! The load-bearing invariant: routed and direct output must be BYTE-IDENTICAL
//! for every format and arg shape. We capture the direct output (a CLI run whose
//! private `XDG_CACHE_HOME` has no daemon socket) and the routed output (a CLI
//! run whose `XDG_CACHE_HOME` is the running daemon's) and diff them exactly.
//! The daemon's stderr is captured to a file so we can prove routing actually
//! happened (a served `hello` opens the vault warm and logs it) rather than
//! silently falling back — which would make the diff trivially pass.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{norn_bin, socket_path_for, wait_for_ready, ChildGuard};

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

/// Routed output is byte-identical to direct output for every shape, and the
/// daemon actually served the request (proven via its stderr log).
#[test]
fn routed_count_is_byte_identical_to_direct() {
    let vault = seed_vault();

    // ── Direct captures: a private cache home with no daemon socket. ──
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let direct: Vec<(Vec<u8>, i32)> = arg_shapes()
        .iter()
        .map(|shape| {
            let (stdout, _stderr, code) = run_count(
                direct_cache.path(),
                direct_state.path(),
                vault.path(),
                shape,
            );
            (stdout, code)
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
    //    finds the live socket and routes. ──
    for (shape, (direct_stdout, direct_code)) in arg_shapes().iter().zip(direct.iter()) {
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
            routed_code, *direct_code,
            "routed exit code must match direct for count {shape:?}"
        );
        assert!(
            routed_stderr.is_empty(),
            "a routed read must be silent on the CLI's stderr for count {shape:?}, got: {}",
            String::from_utf8_lossy(&routed_stderr)
        );
    }

    // ── Prove routing actually happened: the daemon logs the vault open on the
    //    first served request. If the CLI had silently fallen back to Direct the
    //    daemon would never have opened the vault. ──
    let daemon_log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        daemon_log.contains("opened vault"),
        "the daemon must have served at least one routed request (its log should \
         show a vault open), got stderr:\n{daemon_log}"
    );
}
