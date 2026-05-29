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

fn synth_vault(tmp: &TempDir) -> std::path::PathBuf {
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n[[a]]\n").unwrap();
    root
}

/// Acquire the mutation lock for a vault at `vault_root`.
/// Returns the held file — drop it to release.
fn hold_mutation_lock(vault_root: &std::path::Path) -> std::fs::File {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(vault_root).unwrap();
    let canonical_str = canonical.to_str().unwrap();
    let mut hasher = Sha256::new();
    hasher.update(canonical_str.as_bytes());
    let hash = hex_lower(hasher.finalize().as_ref());
    let state_base = std::env::var("XDG_STATE_HOME")
        .unwrap_or_else(|_| format!("{}/.local/state", std::env::var("HOME").unwrap()));
    let lock_dir = std::path::PathBuf::from(state_base)
        .join("norn")
        .join(&hash);
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
        r#"{{"schema_version":1,"vault_root":"{}","operations":[{{"kind":"move_document","fields":{{"src":"a.md","dst":"renamed.md"}}}}]}}"#,
        vault_root.to_str().unwrap()
    )
}

// ─── migrate ──────────────────────────────────────────────────────────────────

#[test]
fn migrate_file_blocked_by_held_lock_exits_2() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);
    let plan_json = minimal_plan_json(&vault);
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, &plan_json).unwrap();

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate", "--yes"])
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
fn migrate_stdin_blocked_saves_pending_and_prints_retry() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);
    let plan_json = minimal_plan_json(&vault);

    let _held = hold_mutation_lock(&vault);

    let mut child = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate", "-", "--yes"])
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
        stderr.contains("retry with: norn migrate "),
        "retry message missing; stderr: {stderr}"
    );

    // A pending file must exist.
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(&vault).unwrap();
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_str().unwrap().as_bytes());
    let hash = hex_lower(hasher.finalize().as_ref());
    let state_base = std::env::var("XDG_STATE_HOME")
        .unwrap_or_else(|_| format!("{}/.local/state", std::env::var("HOME").unwrap()));
    let pending_dir = std::path::PathBuf::from(state_base)
        .join("norn")
        .join(&hash)
        .join("pending");
    let entries: Vec<_> = std::fs::read_dir(&pending_dir)
        .unwrap_or_else(|_| panic!("pending dir not found at {}", pending_dir.display()))
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".plan.json"))
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly 1 pending plan file");
}

// ─── dry-run and readers must not be blocked ──────────────────────────────────

#[test]
fn migrate_dry_run_not_blocked_by_held_lock() {
    let tmp = TempDir::new().unwrap();
    let vault = synth_vault(&tmp);
    let plan_json = minimal_plan_json(&vault);
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, &plan_json).unwrap();

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate", "--dry-run"])
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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

    let _held = hold_mutation_lock(&vault);

    let out = Command::new(norn_bin())
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
