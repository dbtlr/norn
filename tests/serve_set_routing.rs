//! End-to-end proof that `norn set` routes through the warm daemon (NRN-229) —
//! the FIRST routed mutation, and the template the four cascade commands (PR B)
//! copy.
//!
//! The load-bearing invariant mirrors the read suites (`serve_count_routing`):
//! routed and direct output must be BYTE-IDENTICAL for every format and mode.
//! Because a mutation is stateful, each case seeds TWO identical vault copies —
//! one run direct (a private `XDG_CACHE_HOME` with no daemon socket), one run
//! routed (the daemon's cache home) — and asserts:
//!
//! 1. the `(stdout, stderr, exit_code)` triple is byte-identical (modulo the
//!    non-deterministic telemetry `trace_id`, which is redacted on BOTH sides —
//!    two DIRECT applies also mint different trace ids, so identity there is
//!    impossible by construction, not a routing defect);
//! 2. the POST-state vault is byte-for-byte identical across the two copies
//!    (every file, not just the mutated doc);
//! 3. the daemon actually SERVED the call (its per-call `served vault.set` marker),
//!    so a silent fall-back to Direct — which would make (1) and (2) pass
//!    vacuously — goes red.
//!
//! One contention case pins the routed apply as SAFE under a held lock (non-zero
//! exit, nothing written) WITHOUT asserting byte-identity to Direct: the two
//! legitimately diverge today (a routed apply that fails after send surfaces
//! `post-send-uncertain` / exit 1; a direct apply that cannot take the lock exits
//! 2 with the lock-timeout line). Evidence over a forced assertion.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// A fresh vault tempdir with a NON-dot prefix. `TempDir::new()` names dirs
/// `.tmpXXXX` (leading dot), and norn's walker skips dot-prefixed path
/// components — so a hidden vault root makes every doc invisible. The read suites
/// dodge this the same way (a `Builder` prefix for the vault; plain `TempDir` is
/// fine for the XDG cache/state homes, which norn never walks for docs).
fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-set-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

/// A seed vault: a `task` whose `status` is schema-constrained to an enum (so a
/// disallowed value refuses with the coded `value-not-allowed` NRN-221 refusal),
/// plus an untouched `note`. The `note` and the config prove the WHOLE post-state
/// vault — not just the mutated doc — is identical across copies.
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            ".norn/config.yaml",
            "validate:\n  rules:\n    - name: task-status\n      match:\n        \
             frontmatter:\n          type: task\n      allowed_values:\n        \
             status:\n          - backlog\n          - active\n          - done\n",
        ),
        (
            "task.md",
            "---\ntype: task\nstatus: backlog\ntitle: Task One\n---\nTask body\n",
        ),
        (
            "note.md",
            "---\ntype: note\ntitle: A Note\n---\nNote body\n",
        ),
    ]
}

/// Run `norn --cwd <vault> set <args>` with the given cache/state homes. Stdin is
/// forced to `/dev/null` (never a terminal) so the "non-TTY without --yes =
/// implicit dry-run preview" path is exercised deterministically, regardless of
/// how the test binary itself was launched.
fn run_set(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        // A generous handshake budget so a late-scheduled daemon still answers the
        // probe under CI load (the per-shape served-count proof turns a silent
        // fall-back into a hard failure, so this is the difference between a real
        // regression and scheduler jitter). Harmless on direct runs (no socket).
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("set")
        .args(args)
        .output()
        .expect("run norn set");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Redact the non-deterministic telemetry `trace_id` from `set` output so a routed
/// apply can be compared to a direct one. Newline-preserving and applied to BOTH
/// sides, so a genuine trailing-newline / layout divergence still surfaces.
///
/// Two carriers: the records `trace: <id>` footer line, and the JSON report's
/// `"trace_id":"<id>"` field. Dry-run/preview/refusal outputs carry an empty
/// trace id, so the transform is a consistent no-op there.
fn redact_trace(bytes: &[u8]) -> String {
    let input = String::from_utf8_lossy(bytes);
    let mut result = String::with_capacity(input.len());
    for segment in input.split_inclusive('\n') {
        let (line, nl) = match segment.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (segment, ""),
        };
        if line.starts_with("trace: ") {
            result.push_str("trace: <redacted>");
        } else {
            result.push_str(&redact_json_trace_id(line));
        }
        result.push_str(nl);
    }
    result
}

fn redact_json_trace_id(line: &str) -> String {
    const KEY: &str = "\"trace_id\":\"";
    if let Some(start) = line.find(KEY) {
        let after = start + KEY.len();
        if let Some(end_rel) = line[after..].find('"') {
            let mut s = String::with_capacity(line.len());
            s.push_str(&line[..after]);
            s.push_str("<redacted>");
            s.push_str(&line[after + end_rel..]);
            return s;
        }
    }
    line.to_string()
}

/// The routable shape matrix: {records, json} × {applied via --yes, dry-run via
/// --dry-run, preview via json-without-yes, coded refusal, unknown-doc refusal},
/// plus a `--remove` apply. Each entry is `(name, args, expected_exit)`; direct is
/// the byte-identity oracle, and `expected_exit` is a coarse sanity anchor.
fn shape_matrix() -> Vec<(&'static str, Vec<&'static str>, i32)> {
    vec![
        // ── applied via --yes ──
        (
            "records apply",
            vec!["task", "--field", "status=active", "--yes"],
            0,
        ),
        (
            "json apply",
            vec![
                "task",
                "--field",
                "status=active",
                "--yes",
                "--format",
                "json",
            ],
            0,
        ),
        (
            "remove apply",
            vec!["task", "--remove", "title", "--yes"],
            0,
        ),
        // ── dry-run via --dry-run ──
        (
            "records dry-run",
            vec!["task", "--field", "status=active", "--dry-run"],
            0,
        ),
        (
            "json dry-run",
            vec![
                "task",
                "--field",
                "status=active",
                "--dry-run",
                "--format",
                "json",
            ],
            0,
        ),
        // ── preview via json-without-yes (implicit dry-run) ──
        (
            "json preview (no --yes)",
            vec!["task", "--field", "status=active", "--format", "json"],
            0,
        ),
        // ── records preview via non-TTY-without-yes (implicit dry-run) ──
        (
            "records preview (non-tty)",
            vec!["task", "--field", "status=active"],
            0,
        ),
        // ── coded schema refusal (NRN-221: value-not-allowed), records + json ──
        (
            "records refusal (value-not-allowed)",
            vec!["task", "--field", "status=bogus", "--yes"],
            2,
        ),
        (
            "json refusal (value-not-allowed)",
            vec![
                "task",
                "--field",
                "status=bogus",
                "--yes",
                "--format",
                "json",
            ],
            2,
        ),
        // ── unknown-doc refusal (target-not-found), records + json ──
        (
            "records refusal (unknown doc)",
            vec!["nonexistent", "--field", "status=active", "--yes"],
            2,
        ),
        (
            "json refusal (unknown doc)",
            vec![
                "nonexistent",
                "--field",
                "status=active",
                "--yes",
                "--format",
                "json",
            ],
            2,
        ),
    ]
}

/// Every routable shape: routed output is byte-identical to direct (triple +
/// full post-state vault), and each routed shape was SERVED exactly once.
#[test]
fn routed_set_is_byte_identical_to_direct() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let mut expected_served = 0usize;
    for (name, args, expected_exit) in shape_matrix() {
        // Two identical fresh copies: one direct, one routed.
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        // Direct: a private cache/state home with no daemon socket.
        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let (d_out, d_err, d_code) = run_set(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            &args,
        );
        assert_eq!(
            d_code,
            expected_exit,
            "[{name}] direct exit code sanity ({expected_exit} expected), got {d_code}; stderr: {}",
            String::from_utf8_lossy(&d_err)
        );

        // Routed: the CLI's cache home IS the daemon's, so its probe routes.
        let (r_out, r_err, r_code) = run_set(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            &args,
        );

        // (1) triple identity (trace ids redacted on both sides).
        assert_eq!(
            redact_trace(&r_out),
            redact_trace(&d_out),
            "[{name}] routed stdout must match direct\nrouted: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_out),
            String::from_utf8_lossy(&d_out),
        );
        assert_eq!(
            r_err,
            d_err,
            "[{name}] routed stderr must match direct\nrouted: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_err),
            String::from_utf8_lossy(&d_err),
        );
        assert_eq!(
            r_code, d_code,
            "[{name}] routed exit code must match direct"
        );

        // (2) whole post-state vault identity.
        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] the whole post-state vault must be byte-identical across direct and routed",
        );

        // (3) the daemon served exactly one more vault.set for this shape.
        expected_served += 1;
        let served = count_served(&daemon.stderr_path, "vault.set");
        assert_eq!(
            served,
            expected_served,
            "[{name}] the daemon must have SERVED this routed set (running total {expected_served}), \
             got {served}; a silent fall-back to Direct would make identity pass vacuously.\n\
             daemon stderr:\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

/// With NO daemon, every shape runs the direct path cleanly (the fallback-is-total
/// half of the invariant): the routing seam returns `None` and today's behavior
/// is unchanged.
#[test]
fn no_daemon_runs_direct() {
    let seed = seeded_vault_files();
    for (name, args, expected_exit) in shape_matrix() {
        let vault = fresh_vault();
        write_vault(vault.path(), &seed);
        let cache = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (_out, err, code) = run_set(cache.path(), state.path(), vault.path(), &args);
        assert_eq!(
            code,
            expected_exit,
            "[{name}] set should exit {expected_exit} with no daemon, got {code}; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
}

/// Recursively find the first file named `name` under `root`.
fn find_file_named(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file_named(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

/// Contention: a routed apply while the vault's mutation lock is held externally
/// must be SAFE — a non-zero exit and NOTHING written. This is the one case where
/// routed and direct legitimately diverge (routed → `post-send-uncertain`/exit 1;
/// direct → lock-timeout/exit 2), so it pins only the safety, not byte-identity.
///
/// The debug-only `NORN_MUTATION_LOCK_TIMEOUT_MS` knob keeps the daemon's
/// contended acquire at ~150ms instead of the real 5s.
#[test]
fn routed_apply_under_held_lock_is_safe() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    let apply_args = &["task", "--field", "status=active", "--yes"];

    // 1. A warm-up routed apply materializes the state dir + `.mutation.lock` the
    //    daemon uses (under its OWN state home), then restore the doc so a second
    //    write would be detectable.
    let (_o, warmup_err, warmup_code) = run_set(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        warmup_code,
        0,
        "warm-up routed apply should succeed; stderr: {}",
        String::from_utf8_lossy(&warmup_err)
    );
    write_vault(vault.path(), &seed); // restore status: backlog

    // 2. Hold that lock exclusively (flock, the same mechanism `MutationLock`
    //    uses) so the next routed apply must contend and time out.
    let lock_path = find_file_named(&daemon.state_home, ".mutation.lock")
        .expect("the warm-up apply must have created the mutation lock file");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open the mutation lock file");
    use std::os::unix::io::AsRawFd;
    // SAFETY: a valid open fd; LOCK_EX|LOCK_NB acquires without blocking.
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(
        rc,
        0,
        "test setup: could not hold the mutation lock: {}",
        std::io::Error::last_os_error()
    );

    // 3. The routed apply now contends → the daemon times out → the CLI must fail
    //    safely: non-zero exit, and the file UNTOUCHED (status still backlog).
    let (_out, err, code) = run_set(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_ne!(
        code,
        0,
        "a routed apply under a held lock must NOT succeed; stderr: {}",
        String::from_utf8_lossy(&err)
    );
    let on_disk = std::fs::read_to_string(vault.path().join("task.md")).unwrap();
    assert!(
        on_disk.contains("status: backlog") && !on_disk.contains("status: active"),
        "a contended routed apply must write NOTHING; task.md on disk:\n{on_disk}"
    );

    // The daemon served the contended call (the marker fires before the handler),
    // proving it was the daemon — not a local fall-back — that refused it.
    let served = count_served(&daemon.stderr_path, "vault.set");
    assert!(
        served >= 2,
        "the daemon must have served both the warm-up and the contended apply, got {served}"
    );

    // SAFETY: release the flock via the owning fd before it drops.
    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    drop(lock_file);
}

/// `--config` / `--no-cache-refresh` force Direct even with a live daemon — the
/// same `routing_forced_direct` guard the read seam applies (the wire speaks
/// canonical vault roots only: the daemon loads each vault's own default config
/// and always serves a refreshed warm cache, so routing either flag would
/// silently ignore it).
///
/// The `--config` shape is made semantically load-bearing, not just a
/// served-count probe: the vault's own config refuses `status: frozen`
/// (`allowed_values`), while the alternate config passed via `--config` allows
/// it. Direct honors the alternate config and APPLIES (exit 0, `frozen` on
/// disk); a regression that routed this shape would have the daemon enforce the
/// vault's own config and REFUSE (exit 2, nothing written) — divergence on all
/// three identity axes, not merely a count change.
#[test]
fn forced_direct_flags_never_route() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    // An alternate config OUTSIDE the vault that widens `allowed_values` to
    // include `frozen` — the value the vault's own config refuses.
    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt-config.yaml");
    std::fs::write(
        &alt_config,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        \
         frontmatter:\n          type: task\n      allowed_values:\n        \
         status:\n          - backlog\n          - active\n          - done\n          - frozen\n",
    )
    .unwrap();
    let alt_config_str = alt_config.to_str().expect("utf8 alt config path");

    // (name, args, direct-must-apply): both shapes must run Direct on both
    // sides (served count stays 0) and stay byte-identical.
    let config_args = vec![
        "task",
        "--field",
        "status=frozen",
        "--yes",
        "--config",
        alt_config_str,
    ];
    let no_refresh_args = vec![
        "task",
        "--field",
        "status=active",
        "--yes",
        "--no-cache-refresh",
    ];
    let shapes: Vec<(&str, &[&str], bool)> = vec![
        ("--config alt-config apply", &config_args, true),
        ("--no-cache-refresh apply", &no_refresh_args, false),
    ];

    for (name, args, direct_must_apply) in shapes {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let (d_out, d_err, d_code) = run_set(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            args,
        );
        if direct_must_apply {
            assert_eq!(
                d_code,
                0,
                "[{name}] direct must APPLY under the alternate config (frozen allowed); stderr: {}",
                String::from_utf8_lossy(&d_err)
            );
            let on_disk = std::fs::read_to_string(direct_vault.path().join("task.md")).unwrap();
            assert!(
                on_disk.contains("status: frozen"),
                "[{name}] the alternate config must have taken effect on disk:\n{on_disk}"
            );
        }

        // Same invocation against the daemon's cache home: the forced-Direct
        // guard must keep it off the wire entirely.
        let (r_out, r_err, r_code) = run_set(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            args,
        );
        assert_eq!(
            redact_trace(&r_out),
            redact_trace(&d_out),
            "[{name}] stdout must match direct\nrouted-home: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_out),
            String::from_utf8_lossy(&d_out),
        );
        assert_eq!(
            r_err,
            d_err,
            "[{name}] stderr must match direct\nrouted-home: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_err),
            String::from_utf8_lossy(&d_err),
        );
        assert_eq!(r_code, d_code, "[{name}] exit code must match direct");
        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] the whole post-state vault must be byte-identical",
        );

        // The load-bearing guard assertion: the daemon must NEVER have served
        // a vault.set for a forced-Direct flag.
        let served = count_served(&daemon.stderr_path, "vault.set");
        assert_eq!(
            served,
            0,
            "[{name}] --config / --no-cache-refresh must force Direct: the daemon served {served} \
             vault.set call(s).\ndaemon stderr:\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}
