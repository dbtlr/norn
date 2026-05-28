//! Integration test for `norn migrate <plan.yaml> --dry-run --format json`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-migrate-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n[[a]]\n").unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

#[test]
fn migrate_dry_run_returns_apply_report() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 1
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: renamed.md
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["operations"][0]["kind"], "move_document");
    assert!(
        !vault.join("renamed.md").exists(),
        "dry-run must not mutate"
    );
}

#[test]
fn migrate_rejects_wrong_schema_version() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 99
vault_root: {}
operations: []
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan_path)
        .args(["--dry-run"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(2),
        "schema version mismatch is pre-flight refusal (exit 2)"
    );
}

#[test]
fn migrate_reads_plan_from_stdin() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan_json = serde_json::json!({
        "schema_version": 1,
        "vault_root": vault.to_str().unwrap(),
        "operations": [{
            "kind": "move_document",
            "fields": {
                "src": "a.md",
                "dst": "renamed.md"
            }
        }]
    });

    use std::io::Write;
    let mut child = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate", "-", "--dry-run", "--format", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(plan_json.to_string().as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["operations"][0]["kind"], "move_document");
    assert!(
        !vault.join("renamed.md").exists(),
        "dry-run must not mutate"
    );
}

#[test]
fn migrate_stdin_with_input_format_yaml_works() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan_yaml = format!(
        r#"schema_version: 1
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: renamed.md
"#,
        vault.to_str().unwrap()
    );

    use std::io::Write;
    let mut child = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args([
            "migrate",
            "-",
            "--input-format",
            "yaml",
            "--dry-run",
            "--format",
            "json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(plan_yaml.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
}
