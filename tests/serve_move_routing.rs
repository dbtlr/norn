//! End-to-end proof that `norn move` routes through the warm daemon (NRN-229
//! PR B) — byte-identical to Direct across the routable shape matrix, including a
//! folder move (which exercises the `remove_empty_dirs` cleanup that runs on BOTH
//! surfaces) and a `--parents` apply.
//!
//! Copies the `serve_set_routing` template: each case seeds TWO identical vault
//! copies (one Direct with no socket, one routed against the daemon's cache home)
//! and asserts (1) the trace/path-redacted `(stdout, stderr, exit)` triple is
//! byte-identical, (2) the whole post-state vault is byte-identical, (3) the
//! daemon SERVED `vault.move` exactly the expected number of times (a silent
//! fall-back to Direct would make 1+2 pass vacuously).
//!
//! Beyond `trace_id`, the `move` JSON report embeds an ABSOLUTE `vault_root` and
//! a `plan_hash` (which hashes it); both legitimately differ between the two
//! tempdir copies, so both are redacted on BOTH sides — a genuine layout
//! divergence still surfaces.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-move-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

/// Seed vault: `a.md` (moved / cascade source), `b.md` linking `[[a]]` (a live
/// backlink AND a pre-existing destination for the `destination-exists` refusal),
/// an untouched `note.md` witness, and a `sub/` subtree for the folder move.
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("a.md", "---\ntype: note\n---\nA body\n"),
        ("b.md", "---\ntype: note\n---\nLinks to [[a]] here.\n"),
        ("note.md", "---\ntype: note\n---\nWitness body\n"),
        ("sub/c.md", "---\ntype: note\n---\nInside sub\n"),
    ]
}

fn run_move(cache: &Path, state: &Path, vault: &Path, args: &[&str]) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("move")
        .args(args)
        .output()
        .expect("run norn move");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Redact the non-deterministic / path-bearing fields so a routed run compares to
/// a direct one in a different tempdir: the records `trace:` footer and the JSON
/// `trace_id` / `vault_root` / `plan_hash` values.
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
    // Pretty-printed JSON: `"key": "value"` (a space after the colon).
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

/// `(name, args, expected_exit, routes)` — `routes` is whether the daemon should
/// serve this shape (a stem source or a missing target is gated to Direct).
fn shape_matrix() -> Vec<(&'static str, Vec<&'static str>, i32, bool)> {
    vec![
        (
            "records apply",
            vec!["a.md", "renamed.md", "--yes"],
            0,
            true,
        ),
        (
            "json apply",
            vec!["a.md", "renamed.md", "--yes", "--format", "json"],
            0,
            true,
        ),
        (
            "records dry-run",
            vec!["a.md", "renamed.md", "--dry-run"],
            0,
            true,
        ),
        (
            "json dry-run",
            vec!["a.md", "renamed.md", "--dry-run", "--format", "json"],
            0,
            true,
        ),
        (
            "json preview (no --yes)",
            vec!["a.md", "renamed.md", "--format", "json"],
            0,
            true,
        ),
        (
            "records preview (non-tty)",
            vec!["a.md", "renamed.md"],
            0,
            true,
        ),
        // coded preflight refusal: destination b.md already exists.
        (
            "records refusal (destination-exists)",
            vec!["a.md", "b.md", "--yes"],
            2,
            true,
        ),
        (
            "json refusal (destination-exists)",
            vec!["a.md", "b.md", "--yes", "--format", "json"],
            2,
            true,
        ),
        // folder move apply (records) — exercises remove_empty_dirs on both sides.
        ("folder move apply", vec!["sub", "sub2", "--yes"], 0, true),
        // --parents apply into a not-yet-existing subtree.
        (
            "parents apply",
            vec!["a.md", "deep/nested/x.md", "--parents", "--yes"],
            0,
            true,
        ),
        // unknown-target refusal: a stem/path with no doc on disk → gated Direct.
        (
            "unknown-target refusal (gated Direct)",
            vec!["nonexistent.md", "whatever.md", "--yes"],
            2,
            false,
        ),
    ]
}

#[test]
fn routed_move_is_byte_identical_to_direct() {
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
        let (d_out, d_err, d_code) = run_move(
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

        let (r_out, r_err, r_code) = run_move(
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
        let served = count_served(&daemon.stderr_path, "vault.move");
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
        let (_o, err, code) = run_move(cache.path(), state.path(), vault.path(), &args);
        assert_eq!(
            code,
            expected_exit,
            "[{name}] move should exit {expected_exit} with no daemon, got {code}; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
}

/// Stem-shadowing regression: the vault holds `foo.md` AND an extensionless
/// non-doc file `foo` (README/README.md-style). `norn move foo dst.md --yes`
/// makes a bare exists() check true, but the CLI arm applies the index-RESOLVED
/// `foo.md` while `vault.move` would apply the RAW `foo` — silently moving a
/// DIFFERENT file with exit 0 on both sides. The `.md`-extension gate must keep
/// this shape Direct (served == 0) with full triple + vault-byte identity.
#[test]
fn stem_shadowed_by_non_doc_file_gates_direct() {
    let mut seed = seeded_vault_files();
    seed.push(("foo.md", "---\ntype: note\n---\nThe real doc\n"));
    seed.push(("foo", "an extensionless non-doc file shadowing the stem\n"));
    let daemon = spawn_ready_daemon_with_log(&[]);

    let args = &["foo", "dst.md", "--yes"];

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_move(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        args,
    );
    // Direct semantics sanity: the RESOLVED doc moved; the shadow file did not.
    assert_eq!(
        d_code,
        0,
        "direct stem move must apply; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );
    assert!(
        !direct_vault.path().join("foo.md").exists() && direct_vault.path().join("dst.md").exists(),
        "direct must move the RESOLVED foo.md"
    );
    assert!(
        direct_vault.path().join("foo").exists(),
        "direct must leave the extensionless shadow file untouched"
    );

    let (r_out, r_err, r_code) = run_move(
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
    let served = count_served(&daemon.stderr_path, "vault.move");
    assert_eq!(
        served, 0,
        "a stem shadowed by a non-doc file must gate Direct; the daemon served {served} vault.move call(s)"
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

/// Contention: a routed apply while the vault mutation lock is held externally
/// must be SAFE — exit 2 (the daemon's lock-timeout becomes a coded
/// `mutation-lock-timeout` refusal, reconstructed as an exit-2 refusal) and
/// NOTHING written. Direct and routed diverge only on the stderr PROSE (Direct
/// prints a shorter hardcoded line; routed reconstructs the fuller `CacheError`
/// Display), so this pins exit + nothing-written + served, not byte-identity, and
/// records the exact delta.
#[test]
fn routed_apply_under_held_lock_is_exit2_and_writes_nothing() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    let apply_args = &["a.md", "renamed.md", "--yes"];

    // Warm-up apply materializes the state dir + .mutation.lock, then restore.
    let (_o, warm_err, warm_code) = run_move(
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
    // The warm-up MOVED a.md → renamed.md; restore the source AND remove the
    // rename artifact so a second (contended) apply's write would be detectable.
    std::fs::remove_file(vault.path().join("renamed.md")).ok();
    write_vault(vault.path(), &seed);

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

    let (_out, err, code) = run_move(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        code,
        2,
        "a routed apply under a held lock must exit 2; stderr: {}",
        String::from_utf8_lossy(&err)
    );
    // Nothing written: a.md still present, renamed.md absent.
    assert!(vault.path().join("a.md").exists(), "source must remain");
    assert!(
        !vault.path().join("renamed.md").exists(),
        "no destination may be created under contention"
    );
    // The routed prose is the fuller CacheError Display (documented divergence).
    let err_s = String::from_utf8_lossy(&err);
    assert!(
        err_s.contains("vault mutation lock could not be acquired within timeout")
            && err_s.contains("another norn mutation is in progress against this vault"),
        "routed lock-timeout prose delta; got: {err_s}"
    );

    let served = count_served(&daemon.stderr_path, "vault.move");
    assert!(
        served >= 2,
        "daemon served warm-up + contended, got {served}"
    );

    let _ = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN) };
    drop(lock_file);
}

/// `--config` / `--no-cache-refresh` force Direct even with a live daemon: the
/// shared `routing_forced_direct` guard keeps them off the wire (served stays 0).
#[test]
fn forced_direct_flags_never_route() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt.yaml");
    std::fs::write(&alt_config, "validate:\n  rules: []\n").unwrap();
    let alt_config_str = alt_config.to_str().unwrap();

    let config_args = vec!["a.md", "renamed.md", "--yes", "--config", alt_config_str];
    let no_refresh_args = vec!["a.md", "renamed.md", "--yes", "--no-cache-refresh"];
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
        let (d_out, d_err, d_code) = run_move(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            args,
        );
        let (r_out, r_err, r_code) = run_move(
            &daemon.cache_home,
            &daemon.state_home,
            routed_vault.path(),
            args,
        );
        assert_eq!(
            redact(&r_out),
            redact(&d_out),
            "[{name}] stdout must match direct"
        );
        assert_eq!(
            redact(&r_err),
            redact(&d_err),
            "[{name}] stderr must match direct"
        );
        assert_eq!(r_code, d_code, "[{name}] exit must match direct");
        assert_eq!(
            dir_snapshot(direct_vault.path()),
            dir_snapshot(routed_vault.path()),
            "[{name}] post-state vault must be byte-identical",
        );
        let served = count_served(&daemon.stderr_path, "vault.move");
        assert_eq!(
            served, 0,
            "[{name}] forced-Direct flags must never route; served {served}",
        );
    }
}

/// Cascade-failure identity (the deterministic outcome-matrix case): a live
/// single-file move whose primary op writes, then a cascade backlink rewrite
/// FAILS because its target file lives in a read-only subdirectory the applier
/// cannot write. By design this is exit 0 with a `cascade.failed = 1` report and
/// a dangling-link WARNING on stderr (a failed backlink rewrite is a warning, not
/// an op failure — the move itself applied). Routed and Direct must produce the
/// byte-identical report, the identical warning, and identical post-state vault
/// bytes (the backlink left dangling on both). This pins the routed reproduction
/// of `emit_cascade_failure_warnings` from the wire report.
///
/// NOTE: a genuine `outcome: failed` / exit-1 partial requires the PRIMARY op to
/// fail mid-apply after preflight passed — a race that is not deterministically
/// inducible for a single-op move plan; that exit-1 partial semantics is pinned
/// at the applier level, not via a flaky e2e.
#[test]
fn routed_partial_cascade_failure_matches_direct() {
    // a.md at root moves fine; ro/linker.md links [[a]] but its parent dir is
    // read-only, so the cascade rewrite of linker.md fails after a.md moved.
    let seed: Vec<(&str, &str)> = vec![
        ("a.md", "---\ntype: note\n---\nA body\n"),
        ("ro/linker.md", "---\ntype: note\n---\nLinks [[a]] here.\n"),
    ];
    let daemon = spawn_ready_daemon_with_log(&[]);

    let run_case = |cache: &Path,
                    state: &Path|
     -> (
        String,
        String,
        i32,
        std::collections::BTreeMap<String, Vec<u8>>,
    ) {
        let vault = fresh_vault();
        write_vault(vault.path(), &seed);
        // Make the subdir read-only so the applier cannot rewrite ro/linker.md.
        let ro_dir = vault.path().join("ro");
        let mut perms = std::fs::metadata(&ro_dir).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o555);
        std::fs::set_permissions(&ro_dir, perms).unwrap();

        let (out, err, code) = run_move(
            cache,
            state,
            vault.path(),
            &["a.md", "renamed.md", "--yes", "--format", "json"],
        );

        // Restore write perms so dir_snapshot + TempDir cleanup can read/remove.
        let mut perms = std::fs::metadata(&ro_dir).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&ro_dir, perms).unwrap();
        let snap = dir_snapshot(vault.path());
        (redact(&out), redact(&err), code, snap)
    };

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code, d_snap) = run_case(direct_cache.path(), direct_state.path());
    let (r_out, r_err, r_code, r_snap) = run_case(&daemon.cache_home, &daemon.state_home);

    // Sanity: a cascade backlink write-failure is exit 0 (the move applied; the
    // failed rewrite is a dangling-link warning), and the warning fired.
    assert_eq!(
        d_code, 0,
        "direct cascade-failure move exits 0; stderr: {d_err}"
    );
    assert_eq!(
        r_code, 0,
        "routed cascade-failure move exits 0; stderr: {r_err}"
    );
    assert!(
        d_err.contains("could not be rewritten after retries and now dangle"),
        "direct must warn about the dangling backlink; stderr: {d_err}"
    );

    assert_eq!(r_out, d_out, "routed cascade report must match direct");
    assert_eq!(r_err, d_err, "routed cascade warning must match direct");
    assert_eq!(
        d_snap, r_snap,
        "routed cascade post-state vault must match direct byte-for-byte"
    );

    let served = count_served(&daemon.stderr_path, "vault.move");
    assert!(
        served >= 1,
        "daemon must have served the cascade-failure move"
    );
}
