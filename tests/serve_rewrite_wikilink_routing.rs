//! End-to-end proof that `norn rewrite-wikilink` routes through the warm daemon
//! (NRN-229 PR B) — byte-identical to Direct across the routable matrix, a
//! multi-doc body+frontmatter rewrite, and the `--out` file surface (both sides
//! write the JSON report to a file; stdout stays silent). Copies the
//! `serve_move_routing` template.
//!
//! `rewrite-wikilink` is the cleanest cascade command to route: BOTH surfaces
//! apply the raw `{old, new}`, so there is no stem-resolution gate and every
//! input routes — including an unresolvable OLD (`target-not-found` refusal).

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, dir_snapshot, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn fresh_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-rw-route-vault-")
        .tempdir()
        .expect("vault tempdir")
}

/// Seed: `old-target.md` / `new-target.md`, plus `a.md` (body links + a
/// frontmatter wikilink) and `b.md` (a body link) — a multi-doc, body+frontmatter
/// cascade — and a `note.md` witness.
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("old-target.md", "---\ntype: note\n---\nold\n"),
        ("new-target.md", "---\ntype: note\n---\nnew\n"),
        (
            "a.md",
            "---\nrel: \"[[old-target]]\"\n---\nBody [[old-target]] and [[old-target|disp]].\n",
        ),
        ("b.md", "---\ntype: note\n---\nAlso [[old-target]] here.\n"),
        ("note.md", "---\ntype: note\n---\nWitness\n"),
    ]
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

fn run_rw(cache: &Path, state: &Path, vault: &Path, args: &[&str]) -> (Vec<u8>, Vec<u8>, i32) {
    prewrite_prune_marker(cache);
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("rewrite-wikilink")
        .args(args)
        .output()
        .expect("run norn rewrite-wikilink");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

fn redact(bytes: &[u8]) -> String {
    redact_str(&String::from_utf8_lossy(bytes))
}

fn redact_str(input: &str) -> String {
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

fn shape_matrix() -> Vec<(&'static str, Vec<&'static str>, i32)> {
    vec![
        (
            "records apply",
            vec!["old-target", "new-target", "--yes"],
            0,
        ),
        (
            "json apply",
            vec!["old-target", "new-target", "--yes", "--format", "json"],
            0,
        ),
        (
            "records dry-run",
            vec!["old-target", "new-target", "--dry-run"],
            0,
        ),
        (
            "json dry-run",
            vec!["old-target", "new-target", "--dry-run", "--format", "json"],
            0,
        ),
        (
            "json preview (no --yes)",
            vec!["old-target", "new-target", "--format", "json"],
            0,
        ),
        (
            "records preview (non-tty)",
            vec!["old-target", "new-target"],
            0,
        ),
        // coded refusal (target-not-found): OLD resolves to no document.
        (
            "records refusal (target-not-found)",
            vec!["no-such-target", "new-target", "--yes"],
            2,
        ),
        (
            "json refusal (target-not-found)",
            vec!["no-such-target", "new-target", "--yes", "--format", "json"],
            2,
        ),
    ]
}

#[test]
fn routed_rewrite_wikilink_is_byte_identical_to_direct() {
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
        let (d_out, d_err, d_code) = run_rw(
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

        let (r_out, r_err, r_code) = run_rw(
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

        expected_served += 1;
        let served = count_served(&daemon.stderr_path, "vault.rewrite_wikilink");
        assert_eq!(
            served,
            expected_served,
            "[{name}] served count must be {expected_served}, got {served}\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

/// `--out <path>` writes the JSON report to a file (stdout silent) on BOTH the
/// routed and direct paths — the file bytes are byte-identical (trace/path
/// fields redacted), the vault post-state matches, and the daemon served it.
#[test]
fn routed_rewrite_out_writes_identical_file() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let direct_vault = fresh_vault();
    let routed_vault = fresh_vault();
    write_vault(direct_vault.path(), &seed);
    write_vault(routed_vault.path(), &seed);

    let out_dir = TempDir::new().unwrap();
    let direct_out = out_dir.path().join("direct.json");
    let routed_out = out_dir.path().join("routed.json");

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_rw(
        direct_cache.path(),
        direct_state.path(),
        direct_vault.path(),
        &[
            "old-target",
            "new-target",
            "--yes",
            "--out",
            direct_out.to_str().unwrap(),
        ],
    );
    let (r_out, r_err, r_code) = run_rw(
        &daemon.cache_home,
        &daemon.state_home,
        routed_vault.path(),
        &[
            "old-target",
            "new-target",
            "--yes",
            "--out",
            routed_out.to_str().unwrap(),
        ],
    );

    // stdout is silent, exit 0, stderr identical on both sides.
    assert!(
        d_out.is_empty(),
        "direct --out must silence stdout; got {:?}",
        String::from_utf8_lossy(&d_out)
    );
    assert!(
        r_out.is_empty(),
        "routed --out must silence stdout; got {:?}",
        String::from_utf8_lossy(&r_out)
    );
    assert_eq!(
        d_code,
        0,
        "direct --out apply exits 0; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );
    assert_eq!(
        r_code,
        0,
        "routed --out apply exits 0; stderr: {}",
        String::from_utf8_lossy(&r_err)
    );
    assert_eq!(
        redact(&r_err),
        redact(&d_err),
        "routed --out stderr must match direct"
    );

    // The report FILES are byte-identical (trace/path fields redacted).
    let d_file = std::fs::read_to_string(&direct_out).expect("direct --out file");
    let r_file = std::fs::read_to_string(&routed_out).expect("routed --out file");
    assert_eq!(
        redact_str(&r_file),
        redact_str(&d_file),
        "routed --out report file must match direct byte-for-byte"
    );

    assert_eq!(
        dir_snapshot(direct_vault.path()),
        dir_snapshot(routed_vault.path()),
        "--out post-state vault must be byte-identical",
    );

    let served = count_served(&daemon.stderr_path, "vault.rewrite_wikilink");
    assert_eq!(
        served, 1,
        "the daemon must have served exactly the routed --out apply"
    );
}

#[test]
fn no_daemon_runs_direct() {
    let seed = seeded_vault_files();
    for (name, args, expected_exit) in shape_matrix() {
        let vault = fresh_vault();
        write_vault(vault.path(), &seed);
        let cache = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let (_o, err, code) = run_rw(cache.path(), state.path(), vault.path(), &args);
        assert_eq!(
            code,
            expected_exit,
            "[{name}] rewrite-wikilink should exit {expected_exit} with no daemon, got {code}; stderr: {}",
            String::from_utf8_lossy(&err)
        );
    }
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

/// Contention: a routed apply under a held lock exits 2 and writes NOTHING (same
/// documented prose delta as `move`/`delete`).
#[test]
fn routed_apply_under_held_lock_is_exit2_and_writes_nothing() {
    let seed = seeded_vault_files();
    let daemon = spawn_ready_daemon_with_log(&[("NORN_MUTATION_LOCK_TIMEOUT_MS", "150")]);
    let vault = fresh_vault();
    write_vault(vault.path(), &seed);
    let apply_args = &["old-target", "new-target", "--yes"];

    let (_o, warm_err, warm_code) = run_rw(
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
    write_vault(vault.path(), &seed); // restore old-target links

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

    let (_out, err, code) = run_rw(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        apply_args,
    );
    assert_eq!(
        code,
        2,
        "a routed rewrite under a held lock must exit 2; stderr: {}",
        String::from_utf8_lossy(&err)
    );
    // Nothing rewritten: a.md still holds the old-target links.
    let a = std::fs::read_to_string(vault.path().join("a.md")).unwrap();
    assert!(
        a.contains("[[old-target]]") && !a.contains("new-target"),
        "a contended routed rewrite must change nothing:\n{a}"
    );
    let err_s = String::from_utf8_lossy(&err);
    assert!(
        err_s.contains("vault mutation lock could not be acquired within timeout")
            && err_s.contains("another norn mutation is in progress against this vault"),
        "routed lock-timeout prose delta; got: {err_s}"
    );

    let served = count_served(&daemon.stderr_path, "vault.rewrite_wikilink");
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

    let config_args = vec![
        "old-target",
        "new-target",
        "--yes",
        "--config",
        alt_config_str,
    ];
    let no_refresh_args = vec!["old-target", "new-target", "--yes", "--no-cache-refresh"];
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
        let (d_out, d_err, d_code) = run_rw(
            direct_cache.path(),
            direct_state.path(),
            direct_vault.path(),
            args,
        );
        let (r_out, r_err, r_code) = run_rw(
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
        let served = count_served(&daemon.stderr_path, "vault.rewrite_wikilink");
        assert_eq!(
            served, 0,
            "[{name}] forced-Direct must never route; served {served}"
        );
    }
}
