//! End-to-end proof that `norn repair --plan` routes through the warm daemon
//! byte-identically to Direct (NRN-231). Mirrors `serve_find_get_routing.rs` /
//! `serve_delete_routing.rs`.
//!
//! `repair --plan` is READ-ONLY (it never writes into the vault, only
//! optionally to an out-of-vault `--out` file), so — unlike `delete` — direct
//! and routed runs can safely share ONE seeded vault directory: no risk of one
//! run's mutation contaminating the other's baseline.
//!
//! Two fields still need redaction, exactly as `serve_delete_routing.rs`
//! redacts `trace_id`/`vault_root`/`plan_hash`:
//! - `MigrationPlan.generated_at` is a real `chrono::Utc::now()` timestamp
//!   stamped fresh by each process invocation.
//! - `MigrationPlan.vault_root` (and the report format's "Repair plan
//!   against <path>" title line) differ because the daemon canonicalizes the
//!   vault root (resolving macOS's `/var` → `/private/var` symlink) while
//!   Direct uses the `--cwd` path as given — same on-disk vault, different
//!   text.

#![cfg(unix)]

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::{count_served, norn_bin, spawn_ready_daemon_with_log, write_vault};

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

/// Seed: `target-note.md` (fixable-link target), `source.md` (near-miss
/// wikilink → a High-confidence `rewrite_link` op, code `link-target-missing`),
/// `orphan.md` (a wikilink with no close candidate → skipped with reason
/// `link-decision-needed`), and a clean `note.md` witness.
fn seeded_vault_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "target-note.md",
            "---\ntype: note\ntitle: Target Note\n---\n\nI am the target.\n",
        ),
        (
            "source.md",
            "---\ntype: note\ntitle: Source\n---\n\nSee [[target-not]] for details.\n",
        ),
        (
            "orphan.md",
            "---\ntype: note\ntitle: Orphan\n---\n\nSee [[completely-unrelated-nonexistent-xyz]] too.\n",
        ),
        ("note.md", "---\ntype: note\n---\nWitness body\n"),
    ]
}

fn fresh_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-repair-route-vault-")
        .tempdir()
        .expect("vault tempdir");
    write_vault(tmp.path(), &seeded_vault_files());
    tmp
}

fn run_repair(cache: &Path, state: &Path, vault: &Path, args: &[&str]) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new(norn_bin())
        .env("XDG_CACHE_HOME", cache)
        .env("XDG_STATE_HOME", state)
        .env("NORN_SERVICE_HANDSHAKE_TIMEOUT_MS", "5000")
        .stdin(Stdio::null())
        .arg("--cwd")
        .arg(vault)
        .arg("repair")
        .args(args)
        .output()
        .expect("run norn repair");
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

/// Normalize away the two fields that legitimately differ between the direct
/// and routed runs of an otherwise-deterministic plan: `generated_at` (a real
/// timestamp) and `vault_root` (canonicalized daemon-side, raw `--cwd`
/// direct-side) — in both its JSON key and the report format's plaintext
/// "Repair plan against <path>" title line.
fn redact(bytes: &[u8]) -> String {
    let input = String::from_utf8_lossy(bytes);
    let mut result = String::with_capacity(input.len());
    for segment in input.split_inclusive('\n') {
        let (line, nl) = match segment.strip_suffix('\n') {
            Some(l) => (l, "\n"),
            None => (segment, ""),
        };
        let mut s = if line.starts_with("Repair plan against ") {
            "Repair plan against <redacted>".to_string()
        } else {
            line.to_string()
        };
        for key in ["vault_root", "generated_at"] {
            s = redact_json_string_field(&s, key);
        }
        result.push_str(&s);
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

/// Each matrix shape is a plain `fn` (not a closure) that maps a per-run
/// `--out` target path to the full flag list — shapes that don't pass `--out`
/// simply ignore the argument.
type ArgsBuilder = fn(&Path) -> Vec<String>;

fn shape_report(_out: &Path) -> Vec<String> {
    vec!["--plan".into(), "--format".into(), "report".into()]
}
fn shape_json(_out: &Path) -> Vec<String> {
    vec!["--plan".into(), "--format".into(), "json".into()]
}
fn shape_paths(_out: &Path) -> Vec<String> {
    vec!["--plan".into(), "--format".into(), "paths".into()]
}
fn shape_default_piped(_out: &Path) -> Vec<String> {
    vec!["--plan".into()]
}
fn shape_code_filter(_out: &Path) -> Vec<String> {
    vec![
        "--plan".into(),
        "--code".into(),
        "link-target-missing".into(),
        "--format".into(),
        "json".into(),
    ]
}
fn shape_path_filter(_out: &Path) -> Vec<String> {
    vec![
        "--plan".into(),
        "--path".into(),
        "source.md".into(),
        "--format".into(),
        "json".into(),
    ]
}
fn shape_confidence_high(_out: &Path) -> Vec<String> {
    vec![
        "--plan".into(),
        "--confidence".into(),
        "high".into(),
        "--format".into(),
        "json".into(),
    ]
}
fn shape_skip_reason(_out: &Path) -> Vec<String> {
    vec![
        "--plan".into(),
        "--skip-reason".into(),
        "link-decision-needed".into(),
        "--format".into(),
        "json".into(),
    ]
}
fn shape_out_silent(out: &Path) -> Vec<String> {
    vec![
        "--plan".into(),
        "--out".into(),
        out.to_str().unwrap().into(),
    ]
}
fn shape_out_with_json(out: &Path) -> Vec<String> {
    vec![
        "--plan".into(),
        "--out".into(),
        out.to_str().unwrap().into(),
        "--format".into(),
        "json".into(),
    ]
}

/// `(name, args_builder, expected_exit, routes, writes_out)`. The
/// `args_builder` receives a per-run `--out` target path (unused by shapes
/// that don't pass `--out`).
fn shape_matrix() -> Vec<(&'static str, ArgsBuilder, i32, bool, bool)> {
    vec![
        ("format report", shape_report, 0, true, false),
        ("format json", shape_json, 0, true, false),
        ("format paths", shape_paths, 0, true, false),
        ("default (piped=json)", shape_default_piped, 0, true, false),
        ("triage --code", shape_code_filter, 0, true, false),
        ("triage --path", shape_path_filter, 0, true, false),
        ("--confidence high", shape_confidence_high, 0, true, false),
        ("--skip-reason", shape_skip_reason, 0, true, false),
        ("--out (silent stdout)", shape_out_silent, 0, true, true),
        ("--out --format json", shape_out_with_json, 0, true, true),
    ]
}

#[test]
fn routed_repair_plan_is_byte_identical_to_direct() {
    let vault = fresh_vault();

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let mut expected_served = 0usize;
    for (name, build_args, expected_exit, routes, writes_out) in shape_matrix() {
        let direct_out_dir = TempDir::new().unwrap();
        let routed_out_dir = TempDir::new().unwrap();
        let direct_out_path = direct_out_dir.path().join("plan.json");
        let routed_out_path = routed_out_dir.path().join("plan.json");

        let direct_args = build_args(&direct_out_path);
        let routed_args = build_args(&routed_out_path);
        let direct_args_ref: Vec<&str> = direct_args.iter().map(String::as_str).collect();
        let routed_args_ref: Vec<&str> = routed_args.iter().map(String::as_str).collect();

        let (d_out, d_err, d_code) = run_repair(
            direct_cache.path(),
            direct_state.path(),
            vault.path(),
            &direct_args_ref,
        );
        assert_eq!(
            d_code,
            expected_exit,
            "[{name}] direct exit sanity; stderr: {}",
            String::from_utf8_lossy(&d_err)
        );

        let (r_out, r_err, r_code) = run_repair(
            &daemon.cache_home,
            &daemon.state_home,
            vault.path(),
            &routed_args_ref,
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

        if writes_out {
            let d_file = std::fs::read(&direct_out_path)
                .unwrap_or_else(|e| panic!("[{name}] read direct --out file: {e}"));
            let r_file = std::fs::read(&routed_out_path)
                .unwrap_or_else(|e| panic!("[{name}] read routed --out file: {e}"));
            assert_eq!(
                redact(&r_file),
                redact(&d_file),
                "[{name}] routed --out file must be byte-identical to direct"
            );
        }

        if routes {
            expected_served += 1;
        }
        let served = count_served(&daemon.stderr_path, "vault.repair");
        assert_eq!(
            served,
            expected_served,
            "[{name}] served count must be {expected_served}, got {served}\n{}",
            std::fs::read_to_string(&daemon.stderr_path).unwrap_or_default(),
        );
    }
}

/// Bare `norn repair` (summary mode, no `--plan`) has no wire analogue and
/// must never route — the served counter stays at 0.
#[test]
fn bare_repair_summary_does_not_route() {
    let vault = fresh_vault();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) =
        run_repair(direct_cache.path(), direct_state.path(), vault.path(), &[]);
    assert_eq!(
        d_code,
        0,
        "direct bare summary exit sanity; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );

    let (r_out, r_err, r_code) =
        run_repair(&daemon.cache_home, &daemon.state_home, vault.path(), &[]);
    assert_eq!(r_out, d_out, "bare summary stdout must match direct");
    assert_eq!(r_err, d_err, "bare summary stderr must match direct");
    assert_eq!(r_code, d_code, "bare summary exit must match direct");

    let served = count_served(&daemon.stderr_path, "vault.repair");
    assert_eq!(
        served, 0,
        "bare `repair` (no --plan) must never route; served {served}"
    );
}

/// `--config` / `--no-cache-refresh` force Direct: served stays 0.
#[test]
fn forced_direct_flags_never_route() {
    let vault = fresh_vault();
    let daemon = spawn_ready_daemon_with_log(&[]);

    let alt_config_dir = TempDir::new().unwrap();
    let alt_config = alt_config_dir.path().join("alt.yaml");
    std::fs::write(&alt_config, "validate:\n  rules: []\n").unwrap();
    let alt_config_str = alt_config.to_str().unwrap();

    let config_args = vec!["--plan", "--format", "json", "--config", alt_config_str];
    let no_refresh_args = vec!["--plan", "--format", "json", "--no-cache-refresh"];
    let shapes: Vec<(&str, &[&str])> = vec![
        ("--config", &config_args),
        ("--no-cache-refresh", &no_refresh_args),
    ];

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();

    for (name, args) in shapes {
        let (d_out, d_err, d_code) =
            run_repair(direct_cache.path(), direct_state.path(), vault.path(), args);
        let (r_out, r_err, r_code) =
            run_repair(&daemon.cache_home, &daemon.state_home, vault.path(), args);
        assert_eq!(redact(&r_out), redact(&d_out), "[{name}] stdout must match");
        assert_eq!(redact(&r_err), redact(&d_err), "[{name}] stderr must match");
        assert_eq!(r_code, d_code, "[{name}] exit must match");
        let served = count_served(&daemon.stderr_path, "vault.repair");
        assert_eq!(
            served, 0,
            "[{name}] forced-Direct must never route; served {served}"
        );
    }
}

/// No daemon socket present: `repair --plan` still succeeds by falling back to
/// Direct.
#[test]
fn no_daemon_runs_direct() {
    let vault = fresh_vault();
    let cache = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let (_out, err, code) = run_repair(
        cache.path(),
        state.path(),
        vault.path(),
        &["--plan", "--format", "json"],
    );
    assert_eq!(
        code,
        0,
        "repair --plan should exit 0 with no daemon, got {code}; stderr: {}",
        String::from_utf8_lossy(&err)
    );
}

/// NRN-291 sweep parity: a ROUTED `repair --plan` performs NO local
/// cache-maintenance side effect — the daemon owns its warm cache — so the CLI
/// client must skip the tail `lazy_sweep`, exactly as the pre-NRN-291
/// early-return bypassed it (on main a routed read `return`ed before the tail).
/// A DIRECT run (no daemon reachable) still sweeps. The observable proxy is the
/// prune marker `<XDG_CACHE_HOME>/norn/.last-prune` (src/cache/prune.rs
/// `PRUNE_MARKER`): `lazy_sweep` creates/touches it when the marker is absent or
/// stale, so skipping the sweep leaves an absent marker absent. `serve` never
/// touches the marker (it is dispatched before the cache-opening tail), so the
/// daemon side cannot mask the routed client's skip.
#[test]
fn routed_repair_plan_does_not_touch_prune_marker() {
    // Mirrors src/cache/prune.rs `PRUNE_MARKER` (crate-private, so hardcoded here).
    fn prune_marker(cache_home: &Path) -> PathBuf {
        cache_home.join("norn").join(".last-prune")
    }

    let vault = fresh_vault();
    let daemon = spawn_ready_daemon_with_log(&[]);

    // DIRECT control: a fresh cache home with NO daemon reachable. The marker is
    // absent, so the direct run's tail sweep must CREATE it — proving both that
    // `lazy_sweep` fires on the direct path and that the marker is the right
    // observable for "did the sweep run?".
    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    assert!(
        !prune_marker(direct_cache.path()).exists(),
        "precondition: direct cache prune marker must be absent before the run"
    );
    let (_d_out, d_err, d_code) = run_repair(
        direct_cache.path(),
        direct_state.path(),
        vault.path(),
        &["--plan", "--format", "json"],
    );
    assert_eq!(
        d_code,
        0,
        "direct exit sanity; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );
    assert!(
        prune_marker(direct_cache.path()).exists(),
        "a DIRECT `repair --plan` must run the tail sweep, creating the prune marker \
         (matching main's direct path)"
    );

    // ROUTED: served by the daemon. The client must SKIP the tail sweep, so the
    // marker under the daemon's cache home stays absent.
    assert!(
        !prune_marker(&daemon.cache_home).exists(),
        "precondition: daemon cache prune marker must be absent before the routed run"
    );
    let (_r_out, r_err, r_code) = run_repair(
        &daemon.cache_home,
        &daemon.state_home,
        vault.path(),
        &["--plan", "--format", "json"],
    );
    assert_eq!(
        r_code,
        0,
        "routed exit sanity; stderr: {}",
        String::from_utf8_lossy(&r_err)
    );
    let served = count_served(&daemon.stderr_path, "vault.repair");
    assert_eq!(
        served, 1,
        "the run must have been served by the daemon, got {served}"
    );
    assert!(
        !prune_marker(&daemon.cache_home).exists(),
        "a ROUTED `repair --plan` must NOT run the tail sweep — the prune marker must stay absent"
    );
}

/// Exit-code isomorphism (mirrors `find::route`'s end-to-end test): a vault
/// with an error-severity diagnostic must exit 1 on both direct and routed
/// `repair --plan` — the `has_diagnostic_errors` wire bit reproduces
/// `crate::exit_code_for(&index)`.
#[test]
fn exit_code_matches_direct_on_diagnostic_error_vault() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-repair-route-diag-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("good.md"),
        "---\ntype: note\ntitle: Good\n---\nbody\n",
    )
    .unwrap();
    // Invalid UTF-8 with a .md extension trips read_to_string, surfaced as a
    // Severity::Error diagnostic (code "read-failed") — same fixture shape as
    // `find::route`'s exit-code isomorphism test.
    std::fs::write(
        tmp.path().join("bad-utf8.md"),
        b"\xff\xfe\xfd\xfc invalid utf-8 here",
    )
    .unwrap();

    let direct_cache = TempDir::new().unwrap();
    let direct_state = TempDir::new().unwrap();
    let (d_out, d_err, d_code) = run_repair(
        direct_cache.path(),
        direct_state.path(),
        tmp.path(),
        &["--plan", "--format", "json"],
    );
    assert_eq!(
        d_code,
        1,
        "direct repair --plan must exit 1 on a diagnostic-error vault; stderr: {}",
        String::from_utf8_lossy(&d_err)
    );

    let daemon = spawn_ready_daemon_with_log(&[]);
    let (r_out, r_err, r_code) = run_repair(
        &daemon.cache_home,
        &daemon.state_home,
        tmp.path(),
        &["--plan", "--format", "json"],
    );
    assert_eq!(r_code, d_code, "routed exit must match direct (both 1)");
    assert_eq!(
        redact(&r_out),
        redact(&d_out),
        "routed stdout must match direct"
    );
    assert_eq!(
        redact(&r_err),
        redact(&d_err),
        "routed stderr must match direct"
    );

    let served = count_served(&daemon.stderr_path, "vault.repair");
    assert_eq!(
        served, 1,
        "the diagnostic-error run must have been served by the daemon, got {served}"
    );
}
