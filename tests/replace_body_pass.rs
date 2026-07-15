//! Integration tests for `replace_body` applied through `norn apply`.
//!
//! Ported from the retired `norn repair apply` surface (Plan Task 19): the
//! `replace_body` op is applied via the unified MigrationPlan applier.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn norn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_norn"))
}

/// Build a minimal vault on disk with a `.norn/config.yaml` and one note.
fn setup_vault(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("norn-replace-body-")
        .tempdir()
        .unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
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

/// Build a one-op MigrationPlan with a `replace_body` op. The repair op fields
/// (`change_id`, `document_hash`, etc.) nest under `fields`; `operation` is
/// promoted to the op `kind`.
fn plan_json(vault_root: &str, note_rel: &str, document_hash: &str, new_body: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 2,
        "vault_root": vault_root,
        "operations": [{
            "kind": "replace_body",
            "fields": {
                "change_id": "replace-body-test",
                "path": note_rel,
                "document_hash": document_hash,
                "finding_code": "operator-mutation",
                "repair_rule": "vault-set",
                "new_value": new_body
            }
        }]
    }))
    .unwrap()
}

/// Run `norn apply -` feeding `plan` on stdin, with the given extra args.
/// `XDG_CACHE_HOME`/`XDG_STATE_HOME` are isolated to hidden subdirs of the
/// vault root so the binary never reads or sweeps the developer's real
/// cache/state trees.
fn run_apply(vault_root: &str, plan: &str, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&str> = vec!["--cwd", vault_root, "apply", "-"];
    args.extend_from_slice(extra);
    let xdg_root = std::path::Path::new(vault_root);
    let mut cmd = Command::new(norn_bin())
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .env("XDG_CACHE_HOME", xdg_root.join(".xdg-cache"))
        .env("XDG_STATE_HOME", xdg_root.join(".xdg-state"))
        .spawn()
        .unwrap();
    cmd.stdin
        .as_mut()
        .unwrap()
        .write_all(plan.as_bytes())
        .unwrap();
    drop(cmd.stdin.take());
    cmd.wait_with_output().unwrap()
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

    // --yes applies immediately (non-interactive).
    let output = run_apply(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
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

    let output = run_apply(tmp.path().to_str().unwrap(), &plan, &["--dry-run"]);
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

    let output = run_apply(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
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
