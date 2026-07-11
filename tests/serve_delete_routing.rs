//! End-to-end proof that `norn delete` routes through the warm daemon (NRN-229
//! PR B) — byte-identical to Direct, including the first REAL cascade on the wire
//! (`--rewrite-to`, which redirects a backlink across a second file). Copies the
//! `serve_move_routing` template.
//!
//! Since NRN-237 `delete --format records` routes for ALL flag combinations —
//! the renderer's index-derived incoming-link data (count / files / resolved
//! redirect target) rides the wire report as the delete op's `link_impact`, so
//! the `--rewrite-to` and `--allow-broken-links` records shapes reproduce Direct
//! byte-for-byte. Only a non-`.md`/stem target (NRN-239) stays gated to Direct.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-delete-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

/// Seed: `doc.md` (delete target, has a backlink), `alt.md` (redirect target),
/// `linker.md` linking `[[doc]]` (the cascade), `orphan.md` (no backlinks — the
/// clean records delete), and a `note.md` witness.
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("doc.md", "---\ntype: note\n---\nDoc body\n"),
        ("alt.md", "---\ntype: note\n---\nAlt body\n"),
        (
            "linker.md",
            "---\ntype: note\n---\nLinks to [[doc]] here.\n",
        ),
        ("orphan.md", "---\ntype: note\n---\nNo incoming links\n"),
        ("note.md", "---\ntype: note\n---\nWitness body\n"),
    ]
}

fn run_delete(cache: &Path, state: &Path, vault: &Path, args: &[&str]) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("delete")
        .args(args)
        .output()
        .expect("run norn delete");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

fn redact(bytes: &[u8]) -> String {
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
            let mut s = line.to_string();
            for key in ["trace_id", "vault_root", "plan_hash"] {
                s = redact_json_string_field(&s, key);
            }
            result.push_str(&s);
        }
        result.push_str(nl);
    }
    result
}

fn redact_json_string_field(line: &str, key: &str) -> String {
    let needle = format!("\"{key}\": \"");
    if let Some(start) = line.find(&needle) {
        let after = start + needle.len();
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

/// `(name, args, expected_exit, routes)`.
fn shape_matrix() -> Vec<(&'static str, Vec<&'static str>, i32, bool)> {
    vec![
        // clean records delete (no flags, no backlinks) — routable records path.
        (
            "records apply (orphan)",
            vec!["orphan.md", "--yes"],
            0,
            true,
        ),
        (
            "records dry-run (orphan)",
            vec!["orphan.md", "--dry-run"],
            0,
            true,
        ),
        (
            "records preview (non-tty, orphan)",
            vec!["orphan.md"],
            0,
            true,
        ),
        // json cascade apply: delete doc, redirect the [[doc]] backlink to alt.
        (
            "json apply (rewrite-to cascade)",
            vec!["doc.md", "--rewrite-to", "alt", "--yes", "--format", "json"],
            0,
            true,
        ),
        (
            "json dry-run (rewrite-to cascade)",
            vec![
                "doc.md",
                "--rewrite-to",
                "alt",
                "--dry-run",
                "--format",
                "json",
            ],
            0,
            true,
        ),
        (
            "json preview (rewrite-to, no --yes)",
            vec!["doc.md", "--rewrite-to", "alt", "--format", "json"],
            0,
            true,
        ),
        // json apply with allow-broken-links (deletes, leaves the link broken).
        (
            "json apply (allow-broken-links)",
            vec![
                "doc.md",
                "--allow-broken-links",
                "--yes",
                "--format",
                "json",
            ],
            0,
            true,
        ),
        // coded refusal: doc has an incoming link, no flag → backlinks-present.
        (
            "records refusal (backlinks-present)",
            vec!["doc.md", "--yes"],
            2,
            true,
        ),
        (
            "json refusal (backlinks-present)",
            vec!["doc.md", "--yes", "--format", "json"],
            2,
            true,
        ),
        // NRN-237: records + --rewrite-to now ROUTES — the renderer's index data
        // (incoming count / files / resolved redirect) rides the wire report's
        // link_impact, so routed reproduces direct byte-for-byte.
        (
            "records rewrite-to (routed)",
            vec!["doc.md", "--rewrite-to", "alt", "--yes"],
            0,
            true,
        ),
        // NRN-237: records + --allow-broken-links now ROUTES (link_impact carries
        // the incoming count/files the "⚠ N links now broken" line needs).
        (
            "records allow-broken (routed)",
            vec!["doc.md", "--allow-broken-links", "--yes"],
            0,
            true,
        ),
        // unknown-target refusal: no doc on disk → gated Direct.
        (
            "unknown-target refusal (gated Direct)",
            vec!["nonexistent.md", "--yes"],
            2,
            false,
        ),
    ]
}

#[test]
fn routed_delete_is_byte_identical_to_direct() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let mut expected_served = 0usize;
    for (name, args, expected_exit, routes) in shape_matrix() {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let (d_out, d_err, d_code) = run_delete(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            &args,
        );
        assert_eq!(
            d_code,
            expected_exit,
            "[{name}] direct exit sanity; stderr: {}",
            String::from_utf8_lossy(&d_err)
        );

        let (r_out, r_err, r_code) = run_delete(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            &args,
        );

        assert_eq!(
            redact(&r_out),
            redact(&d_out),
            "[{name}] routed stdout must match direct\nrouted: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_out),
            String::from_utf8_lossy(&d_out),
        );
        assert_eq!(
            redact(&r_err),
            redact(&d_err),
            "[{name}] routed stderr must match direct\nrouted: {:?}\ndirect: {:?}",
            String::from_utf8_lossy(&r_err),
            String::from_utf8_lossy(&d_err),
        );
        assert_eq!(r_code, d_code, "[{name}] routed exit must match direct");

        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] the whole post-state vault must be byte-identical",
        );

        if routes {
            expected_served += 1;
        }
        let served = count_served(&daemon.stderr_path, "vault.delete");
        assert_eq!(
            served,
            expected_served,
            "[{name}] served count must be {expected_served}, got {served}\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

#[test]
fn no_daemon_runs_direct() {
    let seed = seeded_vault_files();
    for (name, args, expected_exit, _routes) in shape_matrix() {
        let vault = fresh_vault();
        write_vault(vault.path(), &seed);
        let cache = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (_o, err, code) = run_delete(cache.path(), state.path(), vault.path(), &args);
        assert_eq!(
            code,
            expected_exit,
            "[{name}] delete should exit {expected_exit} with no daemon, got {code}; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
}

/// Stem-shadowing regression (the delete twin of `move`'s): the vault holds
/// `foo.md` AND an extensionless non-doc file `foo`. `norn delete foo --yes`
/// makes a bare exists() check true, but the CLI arm deletes the index-RESOLVED
/// `foo.md` while `vault.delete` would apply the RAW `foo` (refusing) — routed
/// and direct would diverge on exit code AND on-disk state. The `.md`-extension
/// gate must keep this shape Direct (served == 0) with full identity.
#[test]
fn stem_shadowed_by_non_doc_file_gates_direct() {
    let mut seed = seeded_vault_files();
    seed.push(("foo.md", "---\ntype: note\n---\nThe real doc\n"));
    seed.push(("foo", "an extensionless non-doc file shadowing the stem\n"));
    let daemon = spawn_ready_daemon_with_log(&[]);

    let args = &["foo", "--yes"];

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_delete(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        args,
    );
    // Direct semantics sanity: the RESOLVED doc was deleted; the shadow remains.
    assert_eq!(
        d_code,
        0,
        "direct stem delete must apply; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );
    assert!(
        !direct_vault.path().join("foo.md").exists(),
        "direct must delete the RESOLVED foo.md"
    );
    assert!(
        direct_vault.path().join("foo").exists(),
        "direct must leave the extensionless shadow file untouched"
    );

    let (r_out, r_err, r_code) = run_delete(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        args,
    );
    assert_eq!(
        redact(&r_out),
        redact(&d_out),
        "shadowed-stem stdout must match direct"
    );
    assert_eq!(
        redact(&r_err),
        redact(&d_err),
        "shadowed-stem stderr must match direct"
    );
    assert_eq!(r_code, d_code, "shadowed-stem exit must match direct");
    assert_eq!(
        dir_snapshot(direct_vault.path()),
        dir_snapshot(routed_vault.path()),
        "shadowed-stem post-state vault must be byte-identical",
    );

    // The load-bearing gate assertion: the shape must NEVER reach the daemon.
    let served = count_served(&daemon.stderr_path, "vault.delete");
    assert_eq!(
        served, 0,
        "a stem shadowed by a non-doc file must gate Direct; the daemon served {served} vault.delete call(s)"
    );
}

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

/// Contention: a routed apply under a held lock must exit 2 and delete NOTHING.
/// Same documented prose delta as `move` (routed reconstructs the fuller
/// `CacheError` Display; Direct prints the shorter hardcoded line).
#[test]
fn routed_apply_under_held_lock_is_exit2_and_writes_nothing() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    // A clean orphan delete apply routes and needs no flags.
    let apply_args = &["orphan.md", "--yes"];

    let (_o, warm_err, warm_code) = run_delete(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        warm_code,
        0,
        "warm-up routed apply should succeed; stderr: {}",
        String::from_utf8_lossy(&warm_err)
    );
    write_vault(vault.path(), &seed); // recreate orphan.md

    let lock_path = find_file_named(&daemon.state_home, ".mutation.lock")
        .expect("warm-up apply must have created the mutation lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "test setup: could not hold the mutation lock");

    let (_out, err, code) = run_delete(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        code,
        2,
        "a routed delete under a held lock must exit 2; stderr: {}",
        String::from_utf8_lossy(&err)
    );
    assert!(
        vault.path().join("orphan.md").exists(),
        "a contended routed delete must remove nothing"
    );
    let err_s = String::from_utf8_lossy(&err);
    assert!(
        err_s.contains("vault mutation lock could not be acquired within timeout")
            && err_s.contains("another norn mutation is in progress against this vault"),
        "routed lock-timeout prose delta; got: {err_s}"
    );

    let served = count_served(&daemon.stderr_path, "vault.delete");
    assert!(
        served >= 2,
        "daemon served warm-up + contended, got {served}"
    );

    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    drop(lock_file);
}

/// `--config` / `--no-cache-refresh` force Direct: served stays 0.
#[test]
fn forced_direct_flags_never_route() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt.yaml");
    std::fs::write(&alt_config, "validate:\n  rules: []\n").unwrap();
    let alt_config_str = alt_config.to_str().unwrap();

    let config_args = vec!["orphan.md", "--yes", "--config", alt_config_str];
    let no_refresh_args = vec!["orphan.md", "--yes", "--no-cache-refresh"];
    let shapes: Vec<(&str, &[&str])> = vec![
        ("--config apply", &config_args),
        ("--no-cache-refresh apply", &no_refresh_args),
    ];

    for (name, args) in shapes {
        let direct_vault = fresh_vault();
        let routed_vault = fresh_vault();
        write_vault(direct_vault.path(), &seed);
        write_vault(routed_vault.path(), &seed);

        let direct_cache = TempDir::new().unwrap();
        let direct_state = TempDir::new().unwrap();
        let (d_out, d_err, d_code) = run_delete(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            args,
        );
        let (r_out, r_err, r_code) = run_delete(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            args,
        );
        assert_eq!(redact(&r_out), redact(&d_out), "[{name}] stdout must match");
        assert_eq!(redact(&r_err), redact(&d_err), "[{name}] stderr must match");
        assert_eq!(r_code, d_code, "[{name}] exit must match");
        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] post-state vault must be byte-identical",
        );
        let served = count_served(&daemon.stderr_path, "vault.delete");
        assert_eq!(
            served, 0,
            "[{name}] forced-Direct must never route; served {served}"
        );
    }
}
