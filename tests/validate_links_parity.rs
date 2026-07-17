//! Regression tests locking in validate behavior for the link-* finding family.
//!
//! ## Root cause (Phase 4 investigation, 2026-05-22)
//!
//! Investigated parity between `validate --code 'link-*'` (formerly
//! `--code link-unresolved,link-ambiguous`, retired in Phase 1) and
//! the (deleted in v0.30) `vault links unresolved`.
//!
//! The key behavioral difference was **path-filter divergence**:
//! - `vault validate` respects `validate.ignore` patterns in `.norn/config.yaml`.
//! - `vault links unresolved` walked all indexed documents regardless of config.
//!
//! ## Behavior contracts locked in by these tests
//!
//! 1. `validate` respects `validate.ignore` — an ignored path produces zero findings.
//! 2. `validate` emits per-occurrence (no dedup): two occurrences of `[[missing]]`
//!    in the same document produce two rows, not one.

use std::fs;
use std::process::Command;
use tempfile::TempDir;

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Wraps a vault invocation with per-test `XDG_CACHE_HOME` and
/// `XDG_STATE_HOME` trees so the binary never reads or sweeps the
/// developer's real cache/state trees.
fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    prewrite_prune_marker(dir.path());
    dir
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    std::fs::create_dir_all(&tree).expect("NRN-287 sweep isolation: pre-write throttle-marker dir");
    std::fs::write(tree.join(".last-prune"), b"")
        .expect("NRN-287 sweep isolation: pre-write throttle marker");
}

/// A vault with:
/// - `active/a.md` — two occurrences of `[[missing]]` in an active (non-ignored) path
/// - `Archive/old.md` — one occurrence of `[[missing]]` in a path matched by `validate.ignore`
///
/// `.norn/config.yaml` sets `validate.ignore: ["Archive/**"]`.
fn synth_vault_with_ignore() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-parity-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    let active = root.join("active");
    let archive = root.join("Archive");
    let vault_dir = root.join(".norn");

    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&archive).unwrap();
    fs::create_dir_all(&vault_dir).unwrap();

    // Two unresolved [[missing]] occurrences in active/a.md
    fs::write(
        active.join("a.md"),
        "---\ntype: note\n---\nFirst [[missing]] mention.\nSecond [[missing]] mention.\n",
    )
    .unwrap();

    // One unresolved [[missing]] occurrence in Archive/old.md (should be ignored by validate)
    fs::write(
        archive.join("old.md"),
        "---\ntype: note\n---\nArchived [[missing]] link.\n",
    )
    .unwrap();

    // Config with validate.ignore covering Archive/**
    fs::write(
        vault_dir.join("config.yaml"),
        "validate:\n  ignore:\n    - \"Archive/**\"\n  rules: []\nrepair:\n  rules: []\n",
    )
    .unwrap();

    tmp
}

/// `validate --code link-target-missing` respects `validate.ignore`:
/// only the 2 occurrences in `active/a.md` are emitted; `Archive/old.md` is skipped.
#[test]
fn validate_respects_validate_ignore_for_link_target_missing() {
    let tmp = synth_vault_with_ignore();
    let mut cmd = Command::new(norn_bin());
    cmd.args(["--cwd"]).arg(tmp.path().join("vault")).args([
        "validate",
        "--code",
        "link-target-missing",
        "--format",
        "jsonl",
    ]);
    let _cache = isolate_cache(&mut cmd);
    let out = cmd.output().unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect();

    // Only active/a.md contributes — Archive/old.md is ignored
    assert_eq!(
        rows.len(),
        2,
        "expected 2 findings (active/a.md only, Archive skipped); got {}.\nstdout:\n{}",
        rows.len(),
        stdout
    );
    for row in &rows {
        assert_eq!(
            row["path"].as_str().unwrap(),
            "active/a.md",
            "all findings should be from active/a.md"
        );
    }
}

/// `validate` emits per-occurrence, not per unique (path, target).
/// Two occurrences of `[[missing]]` in the same doc produce 2 rows, not 1.
#[test]
fn validate_emits_per_occurrence_not_per_unique_pair() {
    // Reuse the same synth vault; active/a.md has 2 occurrences of [[missing]]
    let tmp = synth_vault_with_ignore();

    // Validate: 2 occurrences from active/a.md
    let mut cmd = Command::new(norn_bin());
    cmd.args(["--cwd"]).arg(tmp.path().join("vault")).args([
        "validate",
        "--code",
        "link-target-missing",
        "--format",
        "jsonl",
    ]);
    let _cache = isolate_cache(&mut cmd);
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let validate_rows: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        validate_rows.len(),
        2,
        "validate should emit 2 rows for 2 occurrences; got {}",
        validate_rows.len()
    );
}
