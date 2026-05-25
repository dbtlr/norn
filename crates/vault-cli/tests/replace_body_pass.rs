//! Integration tests for Pass 1d — `replace_body` wired into `vault repair apply`.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn vault_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_vault"))
}

/// Build a minimal vault on disk with a `.vault/config.yaml` and one note.
fn setup_vault(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("vault-cli-replace-body-")
        .tempdir()
        .unwrap();
    fs::create_dir_all(tmp.path().join(".vault")).unwrap();
    fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
    let note_path = tmp.path().join("note.md");
    fs::write(&note_path, content).unwrap();
    (tmp, note_path)
}

/// Compute the blake3 hex digest of a file's bytes (matches vault-graph's hash
/// function so we can build a plan with a valid `document_hash`).
fn blake3_of_file(path: &std::path::Path) -> String {
    let bytes = fs::read(path).unwrap();
    blake3::hash(&bytes).to_hex().to_string()
}

fn plan_json(vault_root: &str, note_rel: &str, document_hash: &str, new_body: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 8,
        "vault_root": vault_root,
        "source_filters": {
            "code": [], "severity": [], "field": [], "rule": [],
            "path": [], "target": [], "reason": [], "skip_reason": []
        },
        "summary": {
            "findings": 1,
            "planned_changes": 1,
            "skipped": {"by_reason": {}, "total": 0}
        },
        "changes": [{
            "change_id": "replace-body-test",
            "path": note_rel,
            "document_hash": document_hash,
            "finding_code": "operator-mutation",
            "finding_rule": null,
            "repair_rule": "vault-set",
            "operation": "replace_body",
            "field": null,
            "expected_old_value": null,
            "new_value": new_body,
            "destination": null,
            "link_risk": null,
            "warnings": [],
            "force": false
        }],
        "skipped_findings": [],
        "footnotes": []
    }))
    .unwrap()
}

#[test]
fn pass_1d_applies_replace_body_via_orchestrator() {
    let initial = "---\ntitle: Foo\n---\nold body\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = plan_json(
        tmp.path().to_str().unwrap(),
        "note.md",
        &hash,
        "brand new body\n",
    );

    // vault repair apply applies directly when piped (non-TTY).
    let mut cmd = Command::new(vault_bin())
        .args(["--cwd", tmp.path().to_str().unwrap(), "repair", "apply"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .spawn()
        .unwrap();

    cmd.stdin
        .as_mut()
        .unwrap()
        .write_all(plan.as_bytes())
        .unwrap();

    let output = cmd.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "expected success; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&note_path).unwrap();
    assert_eq!(
        final_content, "---\ntitle: Foo\n---\nbrand new body\n",
        "body should be replaced while frontmatter is preserved"
    );
}

#[test]
fn pass_1d_dry_run_does_not_mutate_file() {
    let initial = "---\ntitle: Bar\n---\noriginal body\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = plan_json(
        tmp.path().to_str().unwrap(),
        "note.md",
        &hash,
        "replaced body\n",
    );

    let mut cmd = Command::new(vault_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
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
        .write_all(plan.as_bytes())
        .unwrap();

    let output = cmd.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "dry-run should succeed; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let after = fs::read_to_string(&note_path).unwrap();
    assert_eq!(after, initial, "dry-run must not mutate the file");
}

#[test]
fn pass_1d_rejects_stale_hash() {
    let (tmp, _note_path) = setup_vault("---\ntitle: Baz\n---\nbody\n");
    let plan = plan_json(
        tmp.path().to_str().unwrap(),
        "note.md",
        "definitely-wrong-hash",
        "new body\n",
    );

    let mut cmd = Command::new(vault_bin())
        .args(["--cwd", tmp.path().to_str().unwrap(), "repair", "apply"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .spawn()
        .unwrap();

    cmd.stdin
        .as_mut()
        .unwrap()
        .write_all(plan.as_bytes())
        .unwrap();

    let output = cmd.wait_with_output().unwrap();
    assert!(
        !output.status.success(),
        "stale hash should cause a non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stale") || stderr.contains("hash"),
        "expected stale-hash error in stderr, got: {stderr}"
    );
}
