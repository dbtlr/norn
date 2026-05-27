use std::fs;
use std::process::Command;

fn fixture_vault() -> tempfile::TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-apply-orthog-")
        .tempdir()
        .unwrap();
    fs::create_dir_all(tmp.path().join(".vault")).unwrap();
    fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
    tmp
}

fn empty_plan_json(vault_root: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 9,
        "vault_root": vault_root,
        "source_filters": {
            "code": [], "severity": [], "field": [], "rule": [],
            "path": [], "target": [], "reason": [], "skip_reason": []
        },
        "summary": {
            "findings": 0, "planned_changes": 0,
            "skipped": {"by_reason": {}, "total": 0}
        },
        "changes": [],
        "skipped_findings": [],
        "footnotes": []
    }))
    .unwrap()
}

fn write_plan_file(vault: &tempfile::TempDir) -> std::path::PathBuf {
    let plan_path = vault.path().join("plan.json");
    fs::write(&plan_path, empty_plan_json(vault.path().to_str().unwrap())).unwrap();
    plan_path
}

#[test]
fn apply_out_alone_writes_file_and_keeps_stdout_silent() {
    let vault = fixture_vault();
    let plan_path = write_plan_file(&vault);
    let out_path = vault.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            plan_path.to_str().unwrap(),
            "--dry-run",
            "--out",
            out_path.to_str().unwrap(),
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "",
        "stdout must be silent when --out alone"
    );
    assert!(out_path.exists(), "file must be written");
    let body = fs::read_to_string(&out_path).unwrap();
    assert!(
        body.trim_start().starts_with('{'),
        "file must contain JSON, got: {body}"
    );
}

#[test]
fn apply_out_plus_format_report_writes_file_and_emits_report_to_stdout() {
    let vault = fixture_vault();
    let plan_path = write_plan_file(&vault);
    let out_path = vault.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            plan_path.to_str().unwrap(),
            "--dry-run",
            "--out",
            out_path.to_str().unwrap(),
            "--format",
            "report",
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Report shape: empty plan emits "0 changes from plan · nothing to do"
    assert!(
        stdout.contains("nothing to do") || stdout.contains("changes from plan"),
        "stdout must contain report content, got: {stdout}"
    );
    assert!(out_path.exists(), "file must be written");
}

#[test]
fn apply_out_plus_format_json_writes_json_to_both_streams() {
    let vault = fixture_vault();
    let plan_path = write_plan_file(&vault);
    let out_path = vault.path().join("report.json");
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            plan_path.to_str().unwrap(),
            "--dry-run",
            "--out",
            out_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim_start().starts_with('{'),
        "stdout must contain JSON, got: {stdout}"
    );
    assert!(out_path.exists(), "file must be written");
}

#[test]
fn apply_no_out_no_format_uses_tty_vs_piped_default_when_piped() {
    // When stdout is piped (test environment), default is JSON.
    let vault = fixture_vault();
    let plan_path = write_plan_file(&vault);
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            plan_path.to_str().unwrap(),
            "--dry-run",
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim_start().starts_with('{'),
        "piped default should be JSON, got: {stdout}"
    );
}

#[test]
fn apply_format_report_explicit_emits_report() {
    let vault = fixture_vault();
    let plan_path = write_plan_file(&vault);
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            plan_path.to_str().unwrap(),
            "--dry-run",
            "--format",
            "report",
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("nothing to do"),
        "report format on empty plan should say 'nothing to do', got: {stdout}"
    );
}

#[test]
fn apply_format_paths_empty_on_empty_plan() {
    let vault = fixture_vault();
    let plan_path = write_plan_file(&vault);
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "repair",
            "apply",
            plan_path.to_str().unwrap(),
            "--dry-run",
            "--format",
            "paths",
        ])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "exit failure: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout, b"",
        "empty plan → zero bytes on stdout, got {:?}",
        output.stdout
    );
}
