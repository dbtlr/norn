//! Integration tests for `norn find` link-relationship predicates
//! (`--links-to`, `--unresolved-links`) and their shared appearance on
//! `norn count`.

use std::process::Command;
use tempfile::TempDir;

/// Vault shape:
///   hub.md           — link target (no outgoing links)
///   linker.md        — links [[hub]] (resolves)
///   task-linker.md   — type:task, links [[hub]] (resolves)
///   broken.md        — links [[ghost]] (unresolved)
///   clean.md         — no links
fn synth_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-find-links-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("hub.md"), "---\ntype: note\n---\nhub\n").unwrap();
    std::fs::write(
        root.join("linker.md"),
        "---\ntype: note\n---\nsee [[hub]]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("task-linker.md"),
        "---\ntype: task\n---\nsee [[hub]]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("broken.md"),
        "---\ntype: note\n---\nsee [[ghost]]\n",
    )
    .unwrap();
    std::fs::write(root.join("clean.md"), "---\ntype: note\n---\nno links\n").unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Runs the binary with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn run(tmp: &TempDir, args: &[&str]) -> std::process::Output {
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    Command::new(norn_bin())
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"))
        .arg("--cwd")
        .arg(tmp.path().join("vault"))
        .args(args)
        .output()
        .unwrap()
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

#[test]
fn links_to_returns_resolved_linkers() {
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--links-to", "hub", "--format", "paths"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines.contains(&"linker.md"), "stdout: {stdout}");
    assert!(lines.contains(&"task-linker.md"), "stdout: {stdout}");
    assert!(!lines.contains(&"clean.md"), "stdout: {stdout}");
    assert!(!lines.contains(&"broken.md"), "stdout: {stdout}");
    assert!(!lines.contains(&"hub.md"), "stdout: {stdout}");
}

#[test]
fn links_to_composes_with_frontmatter() {
    let tmp = synth_vault();
    let out = run(
        &tmp,
        &[
            "find",
            "--eq",
            "type:task",
            "--links-to",
            "hub",
            "--format",
            "paths",
        ],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["task-linker.md"], "stdout: {stdout}");
}

#[test]
fn links_to_nonexistent_target_errors() {
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--links-to", "ghost", "--format", "paths"]);
    assert!(!out.status.success());
    // Read-command convention (matches `norn get`): missing target → exit 1.
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no document matched"), "stderr: {stderr}");
}

#[test]
fn links_to_alone_satisfies_predicate_gate() {
    // A bare `find --links-to hub` (no other predicate) must run the query,
    // not print the missing-predicate help page (which exits 2).
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--links-to", "hub", "--format", "paths"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("linker.md"), "stdout: {stdout}");
}

#[test]
fn unresolved_links_returns_dangling_docs() {
    let tmp = synth_vault();
    let out = run(&tmp, &["find", "--unresolved-links", "--format", "paths"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["broken.md"], "stdout: {stdout}");
}

#[test]
fn count_inherits_links_to() {
    // `--links-to` lands on `norn count` for free via the shared FilterArgs.
    let tmp = synth_vault();
    let out = run(&tmp, &["count", "--links-to", "hub", "--format", "json"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["total"], 2);
}
