//! Integration tests for `vault move`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-move-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
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
fn move_dry_run_prints_preview_and_exits_clean() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("vault move b.md → renamed.md"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("1 backlink to rewrite across 1 file"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be moved"
    );
    assert!(
        !tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should not exist"
    );
}

#[test]
fn move_yes_applies_and_rewrites_backlinks() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("✓ moved b.md → renamed.md"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been moved"
    );
    assert!(
        tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should exist"
    );
    let a_content = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(
        a_content.contains("[[renamed]]"),
        "a.md should now reference renamed: {a_content}"
    );
}

#[test]
fn move_format_json_emits_envelope() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("output must parse as JSON");
    assert_eq!(v["operation"], "move");
    assert_eq!(v["source"], "b.md");
    assert_eq!(v["destination"], "renamed.md");
    assert_eq!(v["applied"], false);
    assert_eq!(v["link_rewrites"]["total"], 1);
    // --format json without --yes is implicitly non-interactive; file must not move
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be moved"
    );
}

#[test]
fn move_destination_exists_refused() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn move_yes_format_json_emits_single_json_object() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--yes", "--format", "json"])
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
    assert_eq!(v["operation"], "move");
    // applied = true: the mutation was performed
    assert_eq!(v["applied"], true);
    // File must actually have moved
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been moved"
    );
    assert!(
        tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should exist"
    );
}

#[test]
fn move_destination_exists_with_force_succeeds() {
    let tmp = synth();
    // Add a third file so the cascade has something to rewrite (c.md links to a.md).
    std::fs::write(
        tmp.path().join("vault/c.md"),
        "---\ntype: note\n---\n# C\n[[a]]\n",
    )
    .unwrap();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--force", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a.md should be gone, b.md should exist (overwritten with a.md content)
    assert!(
        !tmp.path().join("vault/a.md").exists(),
        "a.md should have been moved"
    );
    assert!(tmp.path().join("vault/b.md").exists(), "b.md should exist");
}

#[cfg(target_os = "macos")]
#[test]
fn move_case_only_difference_refuses_same_path() {
    let tmp = synth();
    let out = Command::new(vault_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "A.md"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "expected pre-flight refusal on case-only-different destination"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 (pre-flight refusal): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("same canonical path") || stderr.contains("same path"),
        "stderr should mention same-path refusal: {stderr}"
    );
}
