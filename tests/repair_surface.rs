//! Surface guards for the restructured `norn repair` command (Plan Tasks 18+19):
//!   * `norn repair --plan` emits a MigrationPlan (schema_version 2).
//!   * bare `norn repair` prints a read-only findings summary.
//!   * `norn repair plan` (old subcommand) is gone → non-zero exit.
//!   * `norn repair apply` (old subcommand) is gone → non-zero exit.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Non-hidden temp dir so the walker does not skip it.
fn vault_root(prefix: &str) -> PathBuf {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("temp dir should be created");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// A fixture with one High-confidence closest-match proposal (`[[Norn Brand]]`
/// → `norn-brand.md`) and one unresolvable broken link (→ skipped).
fn build_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-repair-surface-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    fs::write(
        root.join("norn-brand.md"),
        "---\ntitle: Norn Brand\n---\n# Norn Brand\n\nTarget.\n",
    )
    .expect("norn-brand.md should write");

    fs::write(
        root.join("source.md"),
        "---\ntitle: Source\n---\n# Source\n\nSee [[Norn Brand]] and [[totally-unknown-xyzzy-99999]].\n",
    )
    .expect("source.md should write");

    (root, config_path)
}

fn run(root: &Path, config_path: &Path, extra: &[&str]) -> std::process::Output {
    let cache_dir = tempfile::Builder::new()
        .prefix("norn-cache-")
        .tempdir()
        .expect("cache temp dir should be created");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"));
    cmd.args([
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
    ]);
    cmd.args(extra);
    prewrite_prune_marker(cache_dir.path());
    cmd.env("XDG_CACHE_HOME", cache_dir.path())
        .env("XDG_STATE_HOME", cache_dir.path().join("state"))
        .env("NO_COLOR", "1");
    cmd.output().expect("vault command should execute")
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

#[test]
fn repair_plan_json_emits_migration_plan() {
    let (root, config_path) = build_fixture();
    let out = run(&root, &config_path, &["--plan", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let plan: serde_json::Value =
        serde_json::from_str(&stdout).expect("--plan --format json should emit valid JSON");

    assert_eq!(
        plan["schema_version"], 2,
        "repair --plan must emit MigrationPlan schema_version 2"
    );
    assert_eq!(
        plan["generator"], "norn-repair",
        "MigrationPlan generator must be norn-repair"
    );
    assert!(
        plan["operations"].is_array(),
        "MigrationPlan must carry an operations array; got: {plan}"
    );
    // The fixture's [[Norn Brand]] link produces exactly one rewrite_link op.
    let ops = plan["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1, "expected one operation; got: {ops:?}");
    assert_eq!(ops[0]["kind"], "rewrite_link");
    assert!(
        ops[0]["fields"].is_object(),
        "operation must nest its data under `fields`"
    );
    assert!(
        ops[0]["fields"].get("operation").is_none(),
        "operation key must be promoted to `kind`, not duplicated in fields"
    );
    // The unresolvable link is reported in `skipped`.
    assert!(
        plan["skipped"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "expected at least one skipped finding; got: {plan}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn bare_repair_prints_findings_summary() {
    let (root, config_path) = build_fixture();
    let out = run(&root, &config_path, &[]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("findings"),
        "bare repair should print a findings summary; got: {stdout}"
    );
    // Summary is NOT a MigrationPlan — it must not be a JSON envelope.
    assert!(
        !stdout.trim_start().starts_with('{'),
        "bare repair summary must not emit a JSON plan; got: {stdout}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn old_repair_plan_subcommand_is_gone() {
    let xdg = tempfile::tempdir().expect("temp xdg dir should be created");
    prewrite_prune_marker(&xdg.path().join("cache"));
    let out = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["repair", "plan", "--format", "json"])
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", xdg.path().join("cache"))
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "`repair plan` subcommand must be removed (replaced by --plan flag)"
    );
}

#[test]
fn old_repair_apply_subcommand_is_gone() {
    let xdg = tempfile::tempdir().expect("temp xdg dir should be created");
    prewrite_prune_marker(&xdg.path().join("cache"));
    let out = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["repair", "apply"])
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", xdg.path().join("cache"))
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "`repair apply` subcommand must be removed (use `norn apply` instead)"
    );
}
