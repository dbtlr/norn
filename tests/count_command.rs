//! Integration tests for `vault count`.

use std::process::Command;
use tempfile::TempDir;

fn synth_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-count-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\nstatus: active\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.md"),
        "---\ntype: log\nstatus: backlog\n---\nbody\n",
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
fn count_total_only_emits_total() {
    let tmp = synth_vault();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["total"], 3);
    assert!(v.get("by").is_none());
}

#[test]
fn count_by_field_groups() {
    let tmp = synth_vault();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "status", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["by"], "status");
    assert_eq!(v["total"], 3);
    assert_eq!(v["groups"]["active"], 1);
    assert_eq!(v["groups"]["backlog"], 2);
}

#[test]
fn count_filter_then_by_narrows() {
    let tmp = synth_vault();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "count",
            "--eq",
            "type:note",
            "--by",
            "status",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["total"], 2);
    assert_eq!(v["groups"]["active"], 1);
    assert_eq!(v["groups"]["backlog"], 1);
}
