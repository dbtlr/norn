//! Integration tests for the vault mutation lock.
//!
//! Tests hold the `.mutation.lock` file directly using fs2 to simulate
//! a concurrent norn mutation, then verify that the command under test
//! exits 2 with the expected contention message.

use fs2::FileExt;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// The per-test `XDG_STATE_HOME` tree, rooted in the test tempdir so the
/// binary and the lock-holding test code agree on the lock location and
/// never touch the developer's real state tree.
fn state_home(tmp: &TempDir) -> std::path::PathBuf {
    tmp.path().join(".xdg-state")
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

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn norn_cmd(tmp: &TempDir) -> Command {
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", state_home(tmp));
    c
}

fn synth_vault(tmp: &TempDir) -> std::path::PathBuf {
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n[[a]]\n").unwrap();
    root
}

/// Acquire the mutation lock for a vault at `vault_root`, inside the
/// per-test `state_base` tree (the same tree `norn_cmd` hands the binary).
/// Returns the held file — drop it to release.
fn hold_mutation_lock(vault_root: &std::path::Path, state_base: &std::path::Path) -> std::fs::File {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(vault_root).unwrap();
    let canonical_str = canonical.to_str().unwrap();
    let mut hasher = Sha256::new();
    hasher.update(canonical_str.as_bytes());
    let hash = hex_lower(hasher.finalize().as_ref());
    let lock_dir = state_base.join("norn").join(&hash);
    std::fs::create_dir_all(&lock_dir).unwrap();
    let lock_path = lock_dir.join(".mutation.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    file.try_lock_exclusive()
        .expect("hold_mutation_lock: could not acquire (already held?)");
    file
}

fn minimal_plan_json(vault_root: &std::path::Path) -> String {
    format!(
        r#"{{"schema_version":2,"vault_root":"{}","operations":[{{"kind":"move_document","fields":{{"src":"a.md","dst":"renamed.md"}}}}]}}"#,
        vault_root.to_str().unwrap()
    )
}

// ─── apply ──────────────────────────────────────────────────────────────────

#[test]
fn apply_file_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);
    let plan_json = minimal_plan_json(&vault);
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, &plan_json).unwrap();

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply", "--yes"])
        .arg(&plan_path)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "expected contention message in stderr; got: {stderr}"
    );
    assert!(
        !vault.join("renamed.md").exists(),
        "vault must not have been mutated"
    );
}

#[test]
fn apply_stdin_blocked_saves_pending_and_prints_retry() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);
    let plan_json = minimal_plan_json(&vault);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let mut child = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply", "-", "--yes"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(plan_json.as_bytes())
        .unwrap();

    let out = child.wait_with_output().unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "contention message missing; stderr: {stderr}"
    );
    assert!(
        stderr.contains("retry with: norn apply "),
        "retry message missing; stderr: {stderr}"
    );

    // A pending file must exist.
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(&vault).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_str().unwrap().as_bytes());
    let hash = hex_lower(hasher.finalize().as_ref());
    let pending_dir = state_home(&tmp).join("norn").join(&hash).join("pending");
    let entries: Vec<_> = std::fs::read_dir(&pending_dir)
        .unwrap_or_else(|_| panic!("pending dir not found at {}", pending_dir.display()))
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".plan.json"))
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly 1 pending plan file");
}

// ─── dry-run and readers must not be blocked ──────────────────────────────────

#[test]
fn apply_dry_run_not_blocked_by_held_lock() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);
    let plan_json = minimal_plan_json(&vault);
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, &plan_json).unwrap();

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply", "--dry-run"])
        .arg(&plan_path)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "dry-run should not be blocked; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn validate_not_blocked_by_held_mutation_lock() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["validate"])
        .output()
        .unwrap();

    // validate exits 0 (no findings) or 1 (findings); never 2 (lock error)
    assert!(
        out.status.success() || out.status.code() == Some(1),
        "validate (reader) must not be blocked; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("another norn mutation"),
        "validate must never show the contention message"
    );
}

// ─── rewrite-wikilink ─────────────────────────────────────────────────────────

#[test]
fn rewrite_wikilink_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["rewrite-wikilink", "a", "alpha", "--yes"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "contention message missing; stderr: {stderr}"
    );
}

// ─── move ─────────────────────────────────────────────────────────────────────

#[test]
fn move_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["move", "a.md", "alpha.md", "--yes"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "stderr: {stderr}"
    );
    assert!(vault.join("a.md").exists(), "a.md must not have moved");
}

#[test]
fn move_dry_run_not_blocked_by_held_lock() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["move", "a.md", "alpha.md", "--dry-run"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "dry-run must not be blocked; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ─── set ──────────────────────────────────────────────────────────────────────

#[test]
fn set_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["set", "a.md", "--field", "title=Test", "--yes"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "stderr: {stderr}"
    );
}

// ─── new ──────────────────────────────────────────────────────────────────────

#[test]
fn new_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["new", "new-doc.md", "--yes"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "stderr: {stderr}"
    );
    assert!(
        !vault.join("new-doc.md").exists(),
        "new-doc.md must not have been created"
    );
}

// ─── delete ───────────────────────────────────────────────────────────────────

#[test]
fn delete_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);

    let _held = hold_mutation_lock(&vault, &state_home(&tmp));

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["delete", "a.md", "--allow-broken-links", "--yes"])
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2), "expected exit 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("another norn mutation is in progress"),
        "stderr: {stderr}"
    );
    assert!(
        vault.join("a.md").exists(),
        "a.md must not have been deleted"
    );
}
