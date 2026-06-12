//! Integration tests for `vault delete`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-delete-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    // Minimal vault config required by build_index.
    std::fs::create_dir(root.join(".norn")).unwrap();
    std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
    std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
    std::fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n").unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn norn_cmd(tmp: &tempfile::TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

#[test]
fn delete_leaf_dry_run_no_op() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
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
        stdout.contains("norn delete d.md"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn delete_leaf_yes_removes_file() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    // The output must parse as a single JSON object (ApplyReport), not two concatenated.
    let trimmed = String::from_utf8_lossy(&out.stdout);
    let trimmed = trimmed.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("output must be a single JSON object: {e}\ngot: {trimmed}"));
    // ApplyReport shape: schema_version, dry_run, applied count, operations[].
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["dry_run"], false);
    // applied count = 1: the delete_document op was executed.
    assert_eq!(v["applied"], 1);
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("d.md"),
        "summary should mention d.md: {:?}",
        v["operations"][0]["summary"]
    );
    // File must actually have been deleted
    assert!(
        !tmp.path().join("vault/d.md").exists(),
        "d.md should have been deleted"
    );
}

#[test]
fn delete_dry_run_format_json_emits_envelope() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("--dry-run --format json must emit a JSON envelope: {e}\ngot: {trimmed}")
    });
    // ApplyReport shape.
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("d.md"),
        "summary should mention d.md: {:?}",
        v["operations"][0]["summary"]
    );
    // Dry-run must not mutate the filesystem.
    assert!(
        tmp.path().join("vault/d.md").exists(),
        "d.md should not be deleted on dry-run"
    );
}

// ---------------------------------------------------------------------------
// T4 — delete --rewrite-to cascade counts in JSON output
// ---------------------------------------------------------------------------

#[test]
fn delete_rewrite_to_cascade_counts_in_json() {
    // Vault seeded by synth(): a.md has [[b]], b.md exists, c.md exists.
    // Delete b.md --rewrite-to c.md → the backlink in a.md should be
    // redirected to c.md; cascade.applied == 1, cascade.files == 1.
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "delete",
            "b.md",
            "--yes",
            "--rewrite-to",
            "c.md",
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));

    let ops = v["operations"].as_array().expect("operations array");
    let del_op = ops
        .iter()
        .find(|o| o["kind"] == "delete_document")
        .unwrap_or_else(|| panic!("delete_document op not found in: {ops:?}"));

    let cascade = &del_op["cascade"];
    assert!(
        !cascade.is_null(),
        "cascade must be present on delete_document op with --rewrite-to"
    );
    // a.md has 1 backlink to b.md that was redirected
    assert_eq!(
        cascade["applied"], 1,
        "1 backlink redirect applied; cascade: {cascade}"
    );
    assert_eq!(cascade["files"], 1, "1 file contained the backlink");

    // Verify filesystem + content mutations
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(a.contains("[[c]]"), "a.md should now reference c: {a}");
}

#[test]
fn delete_format_json_emits_envelope() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
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
    // ApplyReport shape: --format json without --yes is implicitly dry-run.
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("b.md"),
        "summary should mention b.md: {:?}",
        v["operations"][0]["summary"]
    );
    // --format json without --yes is implicitly non-interactive; file must not be deleted.
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be deleted when using --format json without --yes"
    );
}
