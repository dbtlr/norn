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
    // Pre-create a fresh throttle marker BEFORE any norn invocation. The tail GC
    // trigger now spawns a DETACHED sweep child (NRN-287); if the marker were
    // absent during the seeding `cache index` below, that child would run
    // asynchronously AND — because `doomed` is deleted before it runs, so its own
    // exemption resolves to None — evict the very dead entry we are seeding,
    // racing the test. A fresh marker suppresses the trigger during setup.
    // Explicit `cache prune` ignores the marker; only the lazy trigger honors it.
    // Lazy-trigger tests call `clear_marker` deliberately to let the sweep fire.
    let tree = env.cache_home.join("norn");
    std::fs::create_dir_all(&tree).unwrap();
    std::fs::write(tree.join(".last-prune"), b"").unwrap();
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

/// The lazy GC trigger now spawns a DETACHED sweep child (NRN-287), so eviction
/// happens out-of-process and asynchronously. Poll (up to ~10s) for the entry
/// count to reach `want` before asserting — the marker touch is synchronous, but
/// the eviction it triggers is not.
fn wait_for_entries(home: &std::path::Path, want: usize) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let n = tree_entries(home);
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
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
    // Any invocation triggers the throttled GC: it touches the marker
    // synchronously and spawns a DETACHED sweep child (NRN-287) that evicts
    // asynchronously.
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert!(
        env.cache_home.join("norn").join(".last-prune").exists(),
        "marker touched synchronously at spawn time"
    );
    // The detached sweep evicts the dead entry; the current vault's fresh entry
    // (minted by `cache status`) is exempt and survives. Poll for the async run.
    assert_eq!(
        wait_for_entries(&env.cache_home, 1),
        1,
        "current vault's fresh entry survives; dead one gone, new one minted"
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

/// The hidden `cache sweep` child (NRN-287) runs the cross-vault GC directly and
/// synchronously (it IS the foreground process here): it evicts a dead-root
/// entry and writes NOTHING to stdout.
#[test]
fn cache_sweep_child_evicts_and_is_silent() {
    let env = setup();
    clear_marker(&env); // irrelevant to the child, but keep the tree pristine
    assert_eq!(tree_entries(&env.cache_home), 1, "one dead entry seeded");
    let out = norn(&env, env.live_vault.path(), &["cache", "sweep"]);
    assert!(
        out.status.success(),
        "sweep child exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "the detached sweep child must be silent on stdout, got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    // The live vault's own entry is minted by nothing here, so only the dead
    // entry existed; it is evicted synchronously by the foreground child.
    assert_eq!(tree_entries(&env.cache_home), 0, "dead entry evicted");
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

    // `cache status` also mints the live vault's own entry; after the detached
    // sweep fires the dead entry is evicted, leaving exactly 1 (live vault's).
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert_eq!(
        wait_for_entries(&env.cache_home, 1),
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
    clear_marker(&env); // let the trigger path fire so the marker assertion is meaningful
    let cfg_dir = env.live_vault.path().join(".norn");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.yaml"),
        "version: 1\ncache:\n  prune: manual\n",
    )
    .unwrap();
    assert_eq!(tree_entries(&env.cache_home), 1, "one dead entry seeded");

    // The manual-disable decision lives in the sweep CHILD: it acquires the lock,
    // reads the config, and exits WITHOUT sweeping. Invoke the hidden `cache
    // sweep` SYNCHRONOUSLY (it IS the foreground process here) so the negative
    // assertion is deterministic — no sleep-and-hope. In manual mode it must not
    // evict, so the dead entry survives (same shape as
    // `cache_sweep_child_evicts_and_is_silent`, inverted by config).
    let out = norn(&env, env.live_vault.path(), &["cache", "sweep"]);
    assert!(
        out.status.success(),
        "sweep child exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        tree_entries(&env.cache_home),
        1,
        "manual mode: the sweep child evicts nothing, dead entry survives"
    );

    // Trigger-side invariant (still meaningful): any command touches the marker at
    // spawn time regardless of the child's later config decision — the stampede
    // guard advances the throttle even when the sweep is disabled.
    let out = norn(&env, env.live_vault.path(), &["cache", "status"]);
    assert!(out.status.success());
    assert!(
        env.cache_home.join("norn").join(".last-prune").exists(),
        "marker still touched at spawn time even in manual mode"
    );

    // Explicit prune still works in manual mode (config only disables the lazy
    // trigger). `cache status` above minted the live vault's exempt entry; the
    // dead one goes, the live one stays.
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
