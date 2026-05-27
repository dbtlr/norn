//! Phase 6 process-level integration tests for `vault set`.
//! Tasks 6.1 (happy path), 6.2 (refusals), 6.3 (combined ops + body change).

use std::fs;
use std::process::{Command, Stdio};
use tempfile::Builder;

fn norn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn")
}

fn fixture_tempdir() -> tempfile::TempDir {
    let tmp = Builder::new().prefix("vault-cli-set-").tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
    tmp
}

// === Task 6.1: happy path ===

#[test]
fn set_field_writes_frontmatter_change_in_tempdir() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--field",
            "status=active",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .expect("failed to run vault");

    assert!(
        output.status.success(),
        "vault set failed: stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let result = fs::read_to_string(&doc).unwrap();
    assert!(
        result.contains("status: active"),
        "file should contain new status: {result}"
    );

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output should be JSON");
    assert_eq!(json["operation"], "set");
    assert_eq!(json["applied"], true);
    let changes = json["frontmatter_changes"]
        .as_array()
        .expect("changes is array");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["field"], "status");
    assert_eq!(changes[0]["new"], "active");
}

// === Task 6.2: refusal paths ===

#[test]
fn set_refuses_when_doc_not_found() {
    let tmp = fixture_tempdir();
    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "nonexistent.md",
            "--field",
            "x=y",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn set_refuses_cross_class_conflict() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntags:\n- a\n---\nbody\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--field",
            "tags=foo",
            "--push",
            "tags=bar",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tags"),
        "stderr should name the conflicting key: {stderr}"
    );
}

#[test]
fn set_refuses_field_json_with_malformed_json() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--field-json",
            "data={not valid",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// === Task 6.3: combined ops + body change ===

#[test]
fn set_applies_combined_field_push_remove_and_body_atomically() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(
        &doc,
        "---\nstatus: draft\naliases:\n  - old\npriority: high\n---\nold body\n",
    )
    .unwrap();

    let mut child = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--field",
            "status=active",
            "--push",
            "aliases=new",
            "--remove",
            "priority",
            "--body-from-stdin",
            "--yes",
            "--format",
            "json",
        ])
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
        .write_all(b"new body\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "vault set failed: stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("status: active"),
        "status should be active: {final_content}"
    );
    assert!(
        final_content.contains("- new"),
        "new alias should be present: {final_content}"
    );
    assert!(
        final_content.contains("- old"),
        "old alias should still be present: {final_content}"
    );
    assert!(
        !final_content.contains("priority"),
        "priority should be removed: {final_content}"
    );
    assert!(
        final_content.ends_with("new body\n"),
        "body should be replaced: {final_content}"
    );
}

#[test]
fn set_body_from_stdin_matching_existing_body_is_noop_write() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    let original = "---\nstatus: draft\n---\nsame body\n";
    fs::write(&doc, original).unwrap();
    let mtime_before = fs::metadata(&doc).unwrap().modified().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));

    let mut child = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--body-from-stdin",
            "--yes",
            "--format",
            "json",
        ])
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
        .write_all(b"same body\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mtime_after = fs::metadata(&doc).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "no-op write should not touch the file mtime"
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["body_changed"], false);
}

#[test]
fn set_push_accumulates_on_existing_block_array() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\naliases:\n  - existing\n---\nbody\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--push",
            "aliases=new-a",
            "--push",
            "aliases=new-b",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("- existing"),
        "original item preserved: {final_content}"
    );
    assert!(
        final_content.contains("- new-a"),
        "new-a pushed: {final_content}"
    );
    assert!(
        final_content.contains("- new-b"),
        "new-b pushed: {final_content}"
    );
}

#[test]
fn set_push_accumulates_on_existing_flow_array() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\naliases: [existing]\n---\nbody\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--push",
            "aliases=new-a",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("existing"),
        "original item preserved: {final_content}"
    );
    assert!(
        final_content.contains("new-a"),
        "new-a pushed: {final_content}"
    );
}

#[test]
fn set_push_creates_new_array_when_field_absent() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--push",
            "aliases=first",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("aliases:"),
        "aliases key inserted: {final_content}"
    );
    assert!(
        final_content.contains("- first"),
        "first item present: {final_content}"
    );
}

#[test]
fn set_remove_drops_key() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\npriority: high\n---\nbody\n").unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--remove",
            "priority",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        !final_content.contains("priority"),
        "priority should be removed: {final_content}"
    );
    assert!(
        final_content.contains("status: draft"),
        "status should be preserved: {final_content}"
    );
}

#[test]
fn set_dry_run_does_not_mutate_file() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    let original = "---\nstatus: draft\n---\nbody\n";
    fs::write(&doc, original).unwrap();

    let output = Command::new(norn_bin())
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--field",
            "status=active",
            "--dry-run",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let final_content = fs::read_to_string(&doc).unwrap();
    assert_eq!(final_content, original, "dry-run should not mutate file");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["applied"], false);
}
