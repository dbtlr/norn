//! Integration tests for `norn cache prune`.

use std::process::Command;
use tempfile::TempDir;

/// A run environment with isolated cache + state trees and two vaults:
/// one that stays live, one deleted after its cache entry is minted.
struct Env {
    _xdg: TempDir,
    cache_home: std::path::PathBuf,
    state_home: std::path::PathBuf,
    live_vault: TempDir,
}

fn norn(env: &Env, cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_norn"));
    c.args(args)
        .arg("--cwd")
        .arg(cwd)
        .env("XDG_CACHE_HOME", &env.cache_home)
        .env("XDG_STATE_HOME", &env.state_home);
    c.output().expect("norn should run")
}

fn setup() -> Env {
    let xdg = TempDir::new().unwrap();
    let cache_home = xdg.path().join("cache");
    let state_home = xdg.path().join("state");
    let live_vault = TempDir::new().unwrap();
    std::fs::write(
        live_vault.path().join("a.md"),
        "---\ntype: note\n---\nbody\n",
    )
    .unwrap();
    let env = Env {
        cache_home,
        state_home,
        live_vault,
        _xdg: xdg,
    };
    // Mint a cache entry for a doomed vault, then delete the vault.
    let doomed = TempDir::new().unwrap();
    std::fs::write(doomed.path().join("d.md"), "---\ntype: note\n---\nbody\n").unwrap();
    let out = norn(&env, doomed.path(), &["cache", "index"]);
    assert!(
        out.status.success(),
        "seed index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Dropping `doomed` deletes the vault root; its cache entry is now dead.
    drop(doomed);
    // Pre-create a fresh throttle marker so the lazy sweep (which fires on
    // any invocation when the marker is stale/absent) never interferes with
    // explicit-prune tests. Explicit `cache prune` ignores the marker; only
    // the lazy sweep honors it. Lazy-sweep tests call `clear_marker`
    // deliberately to let the sweep fire.
    let tree = env.cache_home.join("norn");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join(".last-prune"), b"").unwrap();
    env
}

/// For lazy-sweep tests: remove the marker so the next invocation sweeps.
fn clear_marker(env: &Env) {
    let _ = std::fs::remove_file(env.cache_home.join("norn").join(".last-prune"));
}

fn tree_entries(home: &std::path::Path) -> usize {
    let tree = home.join("norn");
    match std::fs::read_dir(tree) {
        Ok(rd) => rd.flatten().filter(|e| e.file_name().len() == 64).count(),
        Err(_) => 0,
    }
}

#[test]
fn dry_run_reports_and_preserves_then_real_run_evicts() {
    let env = setup();
    assert_eq!(tree_entries(&env.cache_home), 1);

    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--dry-run"],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("would"), "dry-run phrasing: {text}");
    assert!(text.contains("dead root"), "reason shown: {text}");
    assert_eq!(tree_entries(&env.cache_home), 1, "dry-run must not delete");

    let out = norn(&env, env.live_vault.path(), &["cache", "prune"]);
    assert!(out.status.success());
    assert_eq!(tree_entries(&env.cache_home), 0, "dead entry evicted");

    // Nothing-to-prune is still exit 0.
    let out = norn(&env, env.live_vault.path(), &["cache", "prune"]);
    assert!(out.status.success());
}

#[test]
fn current_vault_entry_is_exempt() {
    let env = setup();
    // Build the live vault's own cache entry, then prune with retention 0d —
    // everything is aged at Duration precision, but the current vault's
    // entry must survive.
    let out = norn(&env, env.live_vault.path(), &["cache", "index"]);
    assert!(out.status.success());
    assert_eq!(tree_entries(&env.cache_home), 2);
    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--retention", "0d"],
    );
    assert!(out.status.success());
    assert_eq!(
        tree_entries(&env.cache_home),
        1,
        "only the dead/aged non-current entries go"
    );
}

#[test]
fn json_format_emits_contract_shape() {
    let env = setup();
    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--format", "json"],
    );
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["dry_run"], false);
    assert_eq!(v["cache"]["evicted"].as_array().expect("array").len(), 1);
    let entry = &v["cache"]["evicted"][0];
    assert_eq!(entry["reason"], "dead-root");
    assert!(entry["root"].is_string());
    assert!(entry["bytes"].as_u64().unwrap() > 0);
    assert!(v["total_bytes_freed"].as_u64().unwrap() > 0);
    assert!(v["state"]["scanned"].is_u64());
}

#[test]
fn bad_retention_flag_is_invocation_error() {
    let env = setup();
    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--retention", "fortnight"],
    );
    assert_eq!(out.status.code(), Some(2), "clap-level invocation error");
}

#[test]
fn lazy_sweep_runs_on_any_command_and_touches_marker() {
    let env = setup();
    clear_marker(&env); // setup() pre-creates it; this test needs the sweep to fire
    assert_eq!(tree_entries(&env.cache_home), 1);
    // Any invocation triggers the throttled sweep (no marker → sweep runs).
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert_eq!(
        tree_entries(&env.cache_home),
        1,
        "current vault's fresh entry survives; dead one gone, new one minted"
    );
    assert!(
        env.cache_home.join("norn").join(".last-prune").exists(),
        "marker touched"
    );
    // The surviving entry is the live vault's own (exemption + dead-root eviction).
    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--dry-run"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("cache  0"), "nothing left to prune: {text}");
}

#[test]
fn fresh_marker_suppresses_lazy_sweep() {
    let env = setup(); // setup() pre-creates a fresh marker: lazy sweep must skip
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert_eq!(
        tree_entries(&env.cache_home),
        2,
        "dead entry survives: sweep throttled"
    );
}

#[test]
fn stale_marker_fires_lazy_sweep() {
    // setup() pre-creates a fresh marker; backdate it to cross the 24 h throttle boundary.
    let env = setup();
    let marker = env.cache_home.join("norn").join(".last-prune");
    assert!(marker.exists(), "setup() must have written the marker");
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3_600);
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(stale))
        .expect("backdating marker");

    // `cache status` also mints the live vault's own entry; after the lazy
    // sweep fires the dead entry is evicted, leaving exactly 1 (live vault's).
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert_eq!(
        tree_entries(&env.cache_home),
        1,
        "stale marker must trigger sweep: dead entry gone, live vault entry kept"
    );

    // Belt-and-suspenders: a subsequent dry-run must report nothing to prune.
    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--dry-run"],
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("cache  0"), "nothing left to prune: {text}");
}

#[test]
fn dry_run_with_stale_marker_does_not_sweep() {
    let env = setup();
    clear_marker(&env); // absent marker: the tail-hook lazy sweep would fire if not suppressed
    let out = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--dry-run"],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("would be evicted"),
        "dry-run report shown: {text}"
    );
    assert_eq!(
        tree_entries(&env.cache_home),
        1,
        "dry-run must not delete even with a stale marker"
    );
    assert!(
        !env.cache_home.join("norn").join(".last-prune").exists(),
        "dry-run must not touch the marker"
    );
}

#[test]
fn prune_manual_config_disables_lazy_sweep() {
    let env = setup();
    clear_marker(&env); // let the lazy path reach the config decision
    let cfg_dir = env.live_vault.path().join(".norn");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.yaml"),
        "version: 1\ncache:\n  prune: manual\n",
    )
    .unwrap();
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert_eq!(
        tree_entries(&env.cache_home),
        2,
        "manual mode: dead entry survives"
    );
    assert!(
        env.cache_home.join("norn").join(".last-prune").exists(),
        "marker still touched after manual-skip decision"
    );
    // Explicit prune still works in manual mode.
    let out = norn(&env, env.live_vault.path(), &["cache", "prune"]);
    assert!(out.status.success());
    assert_eq!(tree_entries(&env.cache_home), 1);
}

#[test]
fn config_retention_flows_into_explicit_prune() {
    let env = setup();
    // Two live entries: the seeded-dead one is irrelevant here; build the
    // live vault's entry, then add a second live vault whose entry we age out
    // via a 0d retention configured in the PRUNING vault's config.
    let out = norn(&env, env.live_vault.path(), &["cache", "index"]);
    assert!(out.status.success());

    let second_vault = TempDir::new().unwrap();
    std::fs::write(
        second_vault.path().join("s.md"),
        "---\ntype: note\n---\nbody\n",
    )
    .unwrap();
    let out = norn(&env, second_vault.path(), &["cache", "index"]);
    assert!(out.status.success());
    // keep second_vault alive (do not drop) so its entry is LIVE — only the
    // 0d retention can evict it.

    // 3 entries: dead + live_vault + second_vault.
    assert_eq!(tree_entries(&env.cache_home), 3);

    let cfg_dir = env.live_vault.path().join(".norn");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.yaml"),
        "version: 1\ncache:\n  retention: 0d\n",
    )
    .unwrap();
    // No --retention flag: the configured 0d must age out every non-exempt
    // entry (the seeded dead one would go anyway; the point is the config
    // value reaching the sweep, observable on the live vault's own entry
    // being exempt while everything else ages out).
    let out = norn(&env, env.live_vault.path(), &["cache", "prune"]);
    assert!(out.status.success());
    assert_eq!(
        tree_entries(&env.cache_home),
        1,
        "0d config retention evicts all non-exempt entries"
    );
    let text_check = norn(
        &env,
        env.live_vault.path(),
        &["cache", "prune", "--dry-run"],
    );
    let text = String::from_utf8_lossy(&text_check.stdout);
    assert!(text.contains("cache  0"), "nothing left: {text}");

    // Keep second_vault in scope until after assertions so it remains live during prune.
    drop(second_vault);
}
