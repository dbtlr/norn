//! Integration tests for `vault delete`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-delete-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    // Minimal vault config required by build_index.
    std::fs::create_dir(root.join(".vault")).unwrap();
    std::fs::write(root.join(".vault/config.yaml"), "validate: {}\n").unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
    std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
    std::fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n").unwrap();
    tmp
}

fn vault_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("vault{}", std::env::consts::EXE_SUFFIX));
    p
}

#[test]
fn delete_leaf_dry_run_no_op() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        tmp.path().join("vault/d.md").exists(),
        "d.md should not be deleted on dry-run"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("vault delete d.md"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn delete_leaf_yes_removes_file() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/d.md").exists(),
        "d.md should have been deleted"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("✓ deleted d.md"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn delete_with_incoming_links_refused() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--yes"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be deleted when refused"
    );
}

#[test]
fn delete_with_allow_broken_links_succeeds() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--yes", "--allow-broken-links"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );
    // a.md should still have its (now broken) link to b.
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(a.contains("[[b]]"), "a.md link should remain (broken): {a}");
}

#[test]
fn delete_with_rewrite_to_redirects_backlinks() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--yes", "--rewrite-to", "c.md"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(a.contains("[[c]]"), "a.md should now reference c: {a}");
    assert!(
        !a.contains("[[b]]"),
        "a.md should no longer reference b: {a}"
    );
}

#[test]
fn delete_yes_format_json_emits_single_json_object() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--yes", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The output must parse as a single JSON object, not two concatenated.
    let trimmed = String::from_utf8_lossy(&out.stdout);
    let trimmed = trimmed.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("output must be a single JSON object: {e}\ngot: {trimmed}"));
    assert_eq!(v["operation"], "delete");
    // applied = true: the mutation was performed
    assert_eq!(v["applied"], true);
    // File must actually have been deleted
    assert!(
        !tmp.path().join("vault/d.md").exists(),
        "d.md should have been deleted"
    );
}

#[test]
fn delete_format_json_emits_envelope() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--allow-broken-links", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("output must parse as JSON");
    assert_eq!(v["operation"], "delete");
    assert_eq!(v["target"], "b.md");
    assert_eq!(v["applied"], false);
    assert_eq!(v["rewrite_to"], serde_json::Value::Null);
    // --format json without --yes is implicitly non-interactive; file must not be deleted.
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be deleted when using --format json without --yes"
    );
}
