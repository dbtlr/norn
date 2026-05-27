//! Integration tests for `vault get`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-get-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\n---\n# B\n[[a]]\n[[missing]]\n",
    )
    .unwrap();
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
fn get_single_target_json() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["path"], "a.md");
}

#[test]
fn get_wikilink_target() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "[[a]]", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v[0]["path"], "a.md");
}

#[test]
fn get_multiple_targets_returns_array() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "b.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
}

#[test]
fn get_col_narrows_output() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--col", "incoming_links", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let record = &v[0];
    assert!(record.get("incoming_links").is_some());
    assert!(record.get("headings").is_none());
}

#[test]
fn get_body_flag_includes_content() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--body", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(v[0]["body"].as_str().unwrap().contains("A"));
}

#[test]
fn get_unknown_col_warns_on_stderr() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "get",
            "a.md",
            "--col",
            "nonexistent_field",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    // Non-fatal: still succeeds. Warning on stderr.
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent_field") || stderr.contains("unknown"),
        "expected stderr warning for unknown col; got: {}",
        stderr
    );
}

#[test]
fn get_missing_target_partial_failure_exit() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "nonexistent", "--format", "json"])
        .output()
        .unwrap();
    // Non-zero exit because one target failed; stdout still has the one
    // that succeeded.
    assert!(!out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nonexistent"));
}
