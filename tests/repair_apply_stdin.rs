use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn fixture_vault() -> tempfile::TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-apply-stdin-")
        .tempdir()
        .unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
    tmp
}

fn empty_plan_json(vault_root: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 9,
        "vault_root": vault_root,
        "source_filters": {
            "code": [],
            "severity": [],
            "field": [],
            "rule": [],
            "path": [],
            "target": [],
            "reason": [],
            "skip_reason": []
        },
        "summary": {
            "findings": 0,
            "planned_changes": 0,
            "skipped": {"by_reason": {}, "total": 0}
        },
        "changes": [],
        "skipped_findings": [],
        "footnotes": []
    }))
    .unwrap()
}

#[test]
fn apply_reads_plan_from_stdin_when_no_positional() {
    let vault = fixture_vault();
    let plan_json = empty_plan_json(vault.path().to_str().unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            "--dry-run",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .spawn()
        .unwrap();
    cmd.stdin
        .as_mut()
        .unwrap()
        .write_all(plan_json.as_bytes())
        .unwrap();
    let output = cmd.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?} stdout={:?}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn apply_reads_plan_from_stdin_when_dash_positional() {
    let vault = fixture_vault();
    let plan_json = empty_plan_json(vault.path().to_str().unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            "-",
            "--dry-run",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .spawn()
        .unwrap();
    cmd.stdin
        .as_mut()
        .unwrap()
        .write_all(plan_json.as_bytes())
        .unwrap();
    let output = cmd.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn apply_with_malformed_stdin_exits_non_zero() {
    let vault = fixture_vault();
    // vault path not needed — parse fails before vault-root check
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["--cwd", vault.path().to_str().unwrap(), "repair", "apply"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .spawn()
        .unwrap();
    cmd.stdin.as_mut().unwrap().write_all(b"not json").unwrap();
    let output = cmd.wait_with_output().unwrap();
    assert!(!output.status.success(), "should fail on bad stdin");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not parse plan from stdin") || stderr.contains("parse"),
        "expected stdin parse error, got: {stderr}",
    );
}
