//! Integration tests for section/body edit ops (`append_to_section`, etc.)
//! applied as first-class MigrationPlan actions (NRN-98 / H1).
//!
//! Unlike `norn edit` — which pre-transforms the body at synth time and stamps a
//! whole-body `replace_body` op — these ops resolve **at apply time**: the
//! applier reads the current body under whole-doc CAS and applies the edit, so a
//! section edit composes into an atomic multi-op plan.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn norn_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_norn"))
}

fn setup_vault(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::Builder::new()
        .prefix("norn-edit-ops-")
        .tempdir()
        .unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
    let note_path = tmp.path().join("note.md");
    fs::write(&note_path, content).unwrap();
    (tmp, note_path)
}

fn blake3_of_file(path: &std::path::Path) -> String {
    blake3::hash(&fs::read(path).unwrap()).to_hex().to_string()
}

/// Build a MigrationPlan whose operations are edit ops. Each `(kind, fields)`
/// entry becomes one operation; `fields` carries the edit anchor (heading/content
/// or old/new) plus `path`/`document_hash`.
fn edit_plan(vault_root: &str, ops: Vec<serde_json::Value>) -> String {
    serde_json::to_string(&serde_json::json!({
        "schema_version": 1,
        "vault_root": vault_root,
        "operations": ops,
    }))
    .unwrap()
}

fn run_migrate(vault_root: &str, plan: &str, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&str> = vec!["--cwd", vault_root, "migrate", "-"];
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
fn append_to_section_applies_at_apply_time() {
    let initial = "---\ntitle: Foo\n---\n## Notes\nline one\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = edit_plan(
        tmp.path().to_str().unwrap(),
        vec![serde_json::json!({
            "kind": "append_to_section",
            "fields": {
                "path": "note.md",
                "document_hash": hash,
                "heading": "Notes",
                "content": "line two"
            }
        })],
    );

    let output = run_migrate(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
    assert!(
        output.status.success(),
        "expected success; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let final_content = fs::read_to_string(&note_path).unwrap();
    assert!(
        final_content.contains("line one") && final_content.contains("line two"),
        "Notes section should carry both lines; got:\n{final_content}"
    );
    assert!(
        final_content.starts_with("---\ntitle: Foo\n---\n"),
        "frontmatter must be preserved; got:\n{final_content}"
    );
}

#[test]
fn str_replace_applies_at_apply_time() {
    // A content-anchored op (unique-match-or-refuse), distinct from the
    // heading-anchored section ops.
    let initial = "---\nt: 1\n---\nhello world\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = edit_plan(
        tmp.path().to_str().unwrap(),
        vec![serde_json::json!({
            "kind": "str_replace",
            "fields": { "path": "note.md", "document_hash": hash, "old": "world", "new": "norn" }
        })],
    );
    let output = run_migrate(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&note_path).unwrap(),
        "---\nt: 1\n---\nhello norn\n"
    );
}

#[test]
fn edit_op_with_stale_hash_aborts_without_writing() {
    let initial = "---\ntitle: Foo\n---\n## Notes\nline one\n";
    let (tmp, note_path) = setup_vault(initial);
    let plan = edit_plan(
        tmp.path().to_str().unwrap(),
        vec![serde_json::json!({
            "kind": "append_to_section",
            "fields": {
                "path": "note.md",
                "document_hash": "0000000000000000000000000000000000000000000000000000000000000000",
                "heading": "Notes",
                "content": "line two"
            }
        })],
    );

    let output = run_migrate(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
    assert!(!output.status.success(), "stale hash must fail the apply");
    assert_eq!(
        fs::read_to_string(&note_path).unwrap(),
        initial,
        "a drifted target must not be written"
    );
}

#[test]
fn dry_run_does_not_mutate() {
    let initial = "---\ntitle: Foo\n---\n## Notes\nline one\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = edit_plan(
        tmp.path().to_str().unwrap(),
        vec![serde_json::json!({
            "kind": "append_to_section",
            "fields": { "path": "note.md", "document_hash": hash, "heading": "Notes", "content": "x" }
        })],
    );
    let output = run_migrate(tmp.path().to_str().unwrap(), &plan, &["--dry-run"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&note_path).unwrap(),
        initial,
        "dry-run must not mutate"
    );
}

#[test]
fn multiple_same_path_edits_apply_in_order() {
    // Two ops on one doc in one plan: append, then replace the whole section.
    // They apply sequentially against the evolving body (share the plan-time hash).
    let initial = "---\nt: 1\n---\n## A\nalpha\n\n## B\nbeta\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = edit_plan(
        tmp.path().to_str().unwrap(),
        vec![
            serde_json::json!({
                "kind": "append_to_section",
                "fields": { "path": "note.md", "document_hash": hash, "heading": "A", "content": "alpha2" }
            }),
            serde_json::json!({
                "kind": "replace_section",
                "fields": { "path": "note.md", "document_hash": hash, "heading": "B", "content": "BETA" }
            }),
        ],
    );
    let output = run_migrate(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = fs::read_to_string(&note_path).unwrap();
    assert!(
        out.contains("alpha") && out.contains("alpha2"),
        "A section appended; got:\n{out}"
    );
    assert!(
        out.contains("BETA") && !out.contains("beta"),
        "B section replaced; got:\n{out}"
    );
}

#[test]
fn edit_composes_with_frontmatter_change_in_one_plan() {
    // The headline H1 use: a frontmatter set + a section append in ONE atomic
    // plan (the Mimir "set lifecycle + append to ## History" pattern). Both
    // reference the original doc hash; the frontmatter pass writes first, the
    // edit pass reads the updated file and appends.
    let initial = "---\nstatus: todo\n---\n## History\n- created\n";
    let (tmp, note_path) = setup_vault(initial);
    let hash = blake3_of_file(&note_path);
    let plan = edit_plan(
        tmp.path().to_str().unwrap(),
        vec![
            serde_json::json!({
                "kind": "set_frontmatter",
                "fields": { "path": "note.md", "document_hash": hash, "field": "status", "expected_old_value": "todo", "new_value": "done" }
            }),
            serde_json::json!({
                "kind": "append_to_section",
                "fields": { "path": "note.md", "document_hash": hash, "heading": "History", "content": "- done" }
            }),
        ],
    );
    let output = run_migrate(tmp.path().to_str().unwrap(), &plan, &["--yes"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = fs::read_to_string(&note_path).unwrap();
    assert!(out.contains("status: done"), "frontmatter set; got:\n{out}");
    assert!(
        out.contains("- created") && out.contains("- done"),
        "History appended; got:\n{out}"
    );
}
