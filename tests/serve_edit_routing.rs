//! End-to-end proof that `norn edit` routes through the warm daemon (NRN-229 PR
//! A) — the second routed mutation, copying `serve_set_routing.rs`'s shape.
//!
//! Same invariant as the `set` suite: routed and direct output must be
//! BYTE-IDENTICAL for every format and mode. Each case seeds TWO identical
//! vault copies — one run direct (a private `XDG_CACHE_HOME` with no daemon
//! socket), one run routed (the daemon's cache home) — and asserts:
//!
//! 1. the `(stdout, stderr, exit_code)` triple is byte-identical (modulo the
//!    non-deterministic telemetry `trace_id`, redacted on both sides);
//! 2. the POST-state vault is byte-for-byte identical across the two copies
//!    (every file, not just the mutated doc);
//! 3. the daemon actually SERVED the call (its per-call `served vault.edit`
//!    marker), so a silent fall-back to Direct goes red instead of passing
//!    vacuously.
//!
//! One contention case pins the routed apply as SAFE under a held lock
//! (non-zero exit, nothing written) WITHOUT asserting byte-identity to Direct —
//! same documented divergence as `set`'s contention case (routed →
//! `post-send-uncertain`/exit 1; direct → lock-timeout/exit 2).

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// A fresh vault tempdir with a NON-dot prefix (see `serve_set_routing.rs` for
/// why: `TempDir::new()` names dirs `.tmpXXXX`, and norn's walker skips
/// dot-prefixed path components).
fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-edit-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

/// A seed vault: a `note` with a str_replace anchor and a heading section (so
/// both op kinds — content-anchored and structural — are exercised), plus an
/// untouched `other` doc. The `other` doc proves the WHOLE post-state vault —
/// not just the mutated doc — is identical across copies.
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "note.md",
            "---\ntype: note\ntitle: A Note\n---\n# Note\n\nHello world\n\n## Section\n\nSection body\n",
        ),
        (
            "other.md",
            "---\ntype: note\ntitle: Other\n---\nOther body\n",
        ),
    ]
}

/// Run `norn --cwd <vault> edit <args>` with the given cache/state homes.
/// Stdin is forced to `/dev/null` (never a terminal) so the "non-TTY without
/// --yes = implicit dry-run preview" path is exercised deterministically.
fn run_edit(
    cache_home: &Path,
    state_home: &Path,
    vault: &Path,
    args: &[&str],
) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_STATE_HOME", state_home)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("edit")
        .args(args)
        .output()
        .expect("run norn edit");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Redact the non-deterministic telemetry `trace_id` from `edit` output — the
/// identical transform `serve_set_routing.rs` uses (records `trace: <id>`
/// footer + the JSON report's `"trace_id":"<id>"` field).
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

/// The routable shape matrix: {records, json} × {applied via --yes, dry-run
/// via --dry-run, preview via json-without-yes, records preview via
/// non-tty-without-yes, a coded refusal (anchor-not-found), unknown-doc
/// refusal}. Each entry is `(name, args, expected_exit)`; direct is the
/// byte-identity oracle, and `expected_exit` is a coarse sanity anchor.
fn shape_matrix() -> Vec<(&'static str, Vec<&'static str>, i32)> {
    vec![
        // ── applied via --yes (single-op sugar) ──
        (
            "records apply",
            vec![
                "note",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
                "--yes",
            ],
            0,
        ),
        (
            "json apply",
            vec![
                "note",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
                "--yes",
                "--format",
                "json",
            ],
            0,
        ),
        // ── dry-run via --dry-run ──
        (
            "records dry-run",
            vec![
                "note",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
                "--dry-run",
            ],
            0,
        ),
        (
            "json dry-run",
            vec![
                "note",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
                "--dry-run",
                "--format",
                "json",
            ],
            0,
        ),
        // ── preview via json-without-yes (implicit dry-run) ──
        (
            "json preview (no --yes)",
            vec![
                "note",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
                "--format",
                "json",
            ],
            0,
        ),
        // ── records preview via non-TTY-without-yes (implicit dry-run) ──
        (
            "records preview (non-tty)",
            vec![
                "note",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
            ],
            0,
        ),
        // ── coded refusal (NRN-220: anchor-not-found), records + json ──
        (
            "records refusal (anchor-not-found)",
            vec![
                "note",
                "--str-replace",
                "NONEXISTENT ANCHOR",
                "--new",
                "x",
                "--yes",
            ],
            2,
        ),
        (
            "json refusal (anchor-not-found)",
            vec![
                "note",
                "--str-replace",
                "NONEXISTENT ANCHOR",
                "--new",
                "x",
                "--yes",
                "--format",
                "json",
            ],
            2,
        ),
        // ── unknown-doc refusal (target-not-found), records + json ──
        (
            "records refusal (unknown doc)",
            vec![
                "nonexistent",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
                "--yes",
            ],
            2,
        ),
        (
            "json refusal (unknown doc)",
            vec![
                "nonexistent",
                "--str-replace",
                "Hello world",
                "--new",
                "Goodbye world",
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
fn routed_edit_is_byte_identical_to_direct() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let mut expected_served = 0usize;
    for (name, args, expected_exit) in shape_matrix() {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let (d_out, d_err, d_code) = run_edit(
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

        let (r_out, r_err, r_code) = run_edit(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            &args,
        );

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

        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] the whole post-state vault must be byte-identical across direct and routed",
        );

        expected_served += 1;
        let served = count_served(&daemon.stderr_path, "vault.edit");
        assert_eq!(
            served,
            expected_served,
            "[{name}] the daemon must have SERVED this routed edit (running total {expected_served}), \
             got {served}; a silent fall-back to Direct would make identity pass vacuously.\n\
             daemon stderr:\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

/// A multi-op batch (`--edits-json`, not the single-op sugar) applies
/// atomically and routes identically — proving the routing seam carries the
/// WHOLE resolved ops array, in order, not just the single-op sugar shape.
#[test]
fn routed_edit_multi_op_batch_is_byte_identical() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let edits_json = r#"[{"op":"str_replace","old":"Hello world","new":"Goodbye world"},{"op":"append_to_section","heading":"Section","content":"- appended item"}]"#;
    let args = vec!["note", "--edits-json", edits_json, "--yes"];

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_edit(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        &args,
    );
    assert_eq!(
        d_code,
        0,
        "direct multi-op apply should succeed; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );

    let (r_out, r_err, r_code) = run_edit(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        &args,
    );

    assert_eq!(redact_trace(&r_out), redact_trace(&d_out));
    assert_eq!(r_err, d_err);
    assert_eq!(r_code, d_code);
    assert_eq!(
        dir_snapshot(direct_vault.path()),
        dir_snapshot(routed_vault.path()),
        "multi-op batch: the whole post-state vault must be byte-identical across direct and routed",
    );

    let served = count_served(&daemon.stderr_path, "vault.edit");
    assert_eq!(
        served, 1,
        "the daemon must have served exactly the one routed multi-op edit"
    );

    // Both ops actually landed (not just the first).
    let content = std::fs::read_to_string(routed_vault.path().join("note.md")).unwrap();
    assert!(
        content.contains("Goodbye world"),
        "str_replace must land:\n{content}"
    );
    assert!(
        content.contains("appended item"),
        "append_to_section must land:\n{content}"
    );
}

/// With NO daemon, every shape runs the direct path cleanly (the
/// fallback-is-total half of the invariant): the routing seam returns `None`
/// and today's behavior is unchanged.
#[test]
fn no_daemon_runs_direct() {
    let seed = seeded_vault_files();
    for (name, args, expected_exit) in shape_matrix() {
        let vault = fresh_vault();
        write_vault(vault.path(), &seed);
        let cache = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (_out, err, code) = run_edit(cache.path(), state.path(), vault.path(), &args);
        assert_eq!(
            code,
            expected_exit,
            "[{name}] edit should exit {expected_exit} with no daemon, got {code}; stderr: {}",
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

/// Contention: a routed apply while the vault's mutation lock is held
/// externally must be SAFE — a non-zero exit and NOTHING written. Same
/// documented divergence as `set`'s contention case (routed →
/// `post-send-uncertain`/exit 1; direct → lock-timeout/exit 2), so this pins
/// only the safety, not byte-identity.
#[test]
fn routed_apply_under_held_lock_is_safe() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    let apply_args = &[
        "note",
        "--str-replace",
        "Hello world",
        "--new",
        "Goodbye world",
        "--yes",
    ];

    // 1. A warm-up routed apply materializes the state dir + `.mutation.lock`
    //    the daemon uses (under its OWN state home), then restore the doc so a
    //    second write would be detectable.
    let (_o, warmup_err, warmup_code) = run_edit(
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
    write_vault(vault.path(), &seed); // restore "Hello world"

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

    // 3. The routed apply now contends → the daemon times out → the CLI must
    //    fail safely: non-zero exit, and the file UNTOUCHED.
    let (_out, err, code) = run_edit(
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
    let on_disk = std::fs::read_to_string(vault.path().join("note.md")).unwrap();
    assert!(
        on_disk.contains("Hello world") && !on_disk.contains("Goodbye world"),
        "a contended routed apply must write NOTHING; note.md on disk:\n{on_disk}"
    );

    // The daemon served the contended call (the marker fires before the
    // handler), proving it was the daemon — not a local fall-back — that
    // refused it.
    let served = count_served(&daemon.stderr_path, "vault.edit");
    assert!(
        served >= 2,
        "the daemon must have served both the warm-up and the contended apply, got {served}"
    );

    // SAFETY: release the flock via the owning fd before it drops.
    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    drop(lock_file);
}

/// `--config` / `--no-cache-refresh` force Direct even with a live daemon — the
/// same `routing_forced_direct` guard the read seam and `set` routing apply
/// (the wire speaks canonical vault roots only: the daemon loads each vault's
/// own default config and always serves a refreshed warm cache, so routing
/// either flag would silently ignore it).
///
/// The `--config` shape is made semantically load-bearing, not just a
/// served-count probe: the vault's OWN config `files.ignore`s the target doc,
/// while the alternate config passed via `--config` ignores nothing. Direct
/// honors the alternate config, resolves the target, and APPLIES (exit 0,
/// edited bytes on disk); a regression that routed this shape would have the
/// daemon load the vault's own config, never see the doc, and REFUSE
/// (target-not-found, exit 2, nothing written) — divergence on all three
/// identity axes, not merely a count change.
#[test]
fn forced_direct_flags_never_route() {
    let daemon = spawn_ready_daemon_with_log(&[]);

    // Seed for the --config shape: the vault's own config ignores note.md.
    let ignoring_seed: Vec<(&str, &str)> = vec![
        (".norn/config.yaml", "files:\n  ignore:\n    - note.md\n"),
        (
            "note.md",
            "---\ntype: note\ntitle: A Note\n---\n# Note\n\nHello world\n\n## Section\n\nSection body\n",
        ),
        (
            "other.md",
            "---\ntype: note\ntitle: Other\n---\nOther body\n",
        ),
    ];
    // An alternate config OUTSIDE the vault that ignores nothing, so the
    // target resolves and the edit applies on the direct path.
    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt-config.yaml");
    std::fs::write(&alt_config, "files:\n  ignore: []\n").unwrap();
    let alt_config_str = alt_config.to_str().expect("utf8 alt config path");

    let config_args = vec![
        "note",
        "--str-replace",
        "Hello world",
        "--new",
        "Goodbye world",
        "--yes",
        "--config",
        alt_config_str,
    ];
    let no_refresh_args = vec![
        "note",
        "--str-replace",
        "Hello world",
        "--new",
        "Goodbye world",
        "--yes",
        "--no-cache-refresh",
    ];
    // (name, seed, args, direct-must-apply): the --no-cache-refresh shape uses
    // the plain seed (no config) so its target is resolvable on the direct path.
    type ForcedDirectShape<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str], bool);
    let plain_seed = seeded_vault_files();
    let shapes: Vec<ForcedDirectShape> = vec![
        (
            "--config alt-config apply",
            &ignoring_seed,
            &config_args,
            true,
        ),
        (
            "--no-cache-refresh apply",
            &plain_seed,
            &no_refresh_args,
            false,
        ),
    ];

    for (name, seed, args, direct_must_apply) in shapes {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), seed);
        write_vault(routed_vault.path(), seed);

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let (d_out, d_err, d_code) = run_edit(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            args,
        );
        if direct_must_apply {
            assert_eq!(
                d_code,
                0,
                "[{name}] direct must APPLY under the alternate config (note.md not ignored); stderr: {}",
                String::from_utf8_lossy(&d_err)
            );
            let on_disk = std::fs::read_to_string(direct_vault.path().join("note.md")).unwrap();
            assert!(
                on_disk.contains("Goodbye world"),
                "[{name}] the alternate config must have taken effect on disk:\n{on_disk}"
            );
        }

        // Same invocation against the daemon's cache home: the forced-Direct
        // guard must keep it off the wire entirely.
        let (r_out, r_err, r_code) = run_edit(
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
        // a vault.edit for a forced-Direct flag.
        let served = count_served(&daemon.stderr_path, "vault.edit");
        assert_eq!(
            served,
            0,
            "[{name}] --config / --no-cache-refresh must force Direct: the daemon served {served} \
             vault.edit call(s).\ndaemon stderr:\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}
