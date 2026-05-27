use std::process::Command;

#[test]
fn repair_plan_rejects_format_jsonl_with_migration_message() {
    let out = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args(["repair", "plan", "--format", "jsonl"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid value 'jsonl'"),
        "stderr did not mention invalid value: {stderr}"
    );
    assert!(
        stderr.contains("--format json") || stderr.contains("use --format json"),
        "stderr did not suggest migration: {stderr}"
    );
}

#[test]
fn repair_plan_rejects_format_table_with_migration_message() {
    let out = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args(["repair", "plan", "--format", "table"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid value 'table'"));
    assert!(stderr.contains("--format report") || stderr.contains("use --format report"));
}

#[test]
fn repair_plan_accepts_report_json_paths() {
    for fmt in ["report", "json", "paths"] {
        let out = Command::new(env!("CARGO_BIN_EXE_vault"))
            .args(["repair", "plan", "--format", fmt, "--help"])
            .output()
            .unwrap();
        // --help short-circuits successfully; tests that the value parses, not that the command runs
        assert!(
            out.status.success(),
            "{fmt} rejected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
