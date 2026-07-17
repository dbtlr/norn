//! NRN-99 (H1): optional expected-hash CAS on `norn edit`.
//!
//! `--expected-hash <HASH>` is an OPT-IN precondition: when supplied, the edit
//! refuses (exit 2, pre-flight) unless the document's current full-content
//! blake3 hex equals HASH. Absent, the edit behaves exactly as today
//! (read-modify-write), preserving the one-shot ergonomic for the common case.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::Builder;

fn norn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn")
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    std::fs::create_dir_all(&tree).expect("NRN-287 sweep isolation: pre-write throttle-marker dir");
    std::fs::write(tree.join(".last-prune"), b"")
        .expect("NRN-287 sweep isolation: pre-write throttle marker");
}

fn norn_cmd(tmp: &tempfile::TempDir) -> Command {
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

fn fixture() -> tempfile::TempDir {
    let tmp = Builder::new().prefix("norn-edit-cas-").tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
    tmp
}

/// blake3 hex of a file's full bytes — the canonical document hash norn uses
/// (matches `set::synth`'s `document_hash` and the applier's CAS check).
fn blake3_of_file(path: &std::path::Path) -> String {
    blake3::hash(&fs::read(path).unwrap()).to_hex().to_string()
}

fn run_edit(
    tmp: &tempfile::TempDir,
    doc: &str,
    edits: &str,
    extra: &[&str],
) -> std::process::Output {
    let mut args = vec!["--cwd", tmp.path().to_str().unwrap(), "edit", doc];
    args.extend_from_slice(extra);
    let mut child = norn_cmd(tmp)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(edits.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

const EDITS: &str = r#"[{"op":"str_replace","old":"world","new":"norn"}]"#;

#[test]
fn edit_with_correct_expected_hash_applies() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\nhello world\n").unwrap();
    let hash = blake3_of_file(&doc);

    let out = run_edit(&tmp, "note.md", EDITS, &["--expected-hash", &hash, "--yes"]);
    assert!(
        out.status.success(),
        "correct expected-hash must apply; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = fs::read_to_string(&doc).unwrap();
    assert!(after.contains("hello norn"), "edit applied; got:\n{after}");
}

#[test]
fn edit_with_uppercase_expected_hash_applies() {
    // blake3 hex is emitted lowercase, but hex is case-insensitive — a correct
    // hash copied in uppercase must be accepted, not read as drift.
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\nhello world\n").unwrap();
    let hash = blake3_of_file(&doc).to_uppercase();

    let out = run_edit(&tmp, "note.md", EDITS, &["--expected-hash", &hash, "--yes"]);
    assert!(
        out.status.success(),
        "uppercase-but-correct expected-hash must apply; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::read_to_string(&doc).unwrap().contains("hello norn"));
}

#[test]
fn edit_with_stale_expected_hash_refuses_and_does_not_write() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    let initial = "---\ntype: note\n---\nhello world\n";
    fs::write(&doc, initial).unwrap();

    let out = run_edit(
        &tmp,
        "note.md",
        EDITS,
        &["--expected-hash", "deadbeefstalehash", "--yes"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "stale expected-hash is a pre-flight refusal (exit 2); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("hash") || stderr.to_lowercase().contains("drift"),
        "expected a drift/hash error on stderr, got: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&doc).unwrap(),
        initial,
        "refused edit must not write"
    );
}

#[test]
fn edit_without_expected_hash_behaves_as_today() {
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\nhello world\n").unwrap();

    // No --expected-hash: read-modify-write, applies unconditionally.
    let out = run_edit(&tmp, "note.md", EDITS, &["--yes"]);
    assert!(
        out.status.success(),
        "no-flag edit must apply as today; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = fs::read_to_string(&doc).unwrap();
    assert!(after.contains("hello norn"), "edit applied; got:\n{after}");
}

#[test]
fn edit_dry_run_with_stale_expected_hash_still_refuses() {
    // The precondition is a preflight guard — it refuses before the transform,
    // so even a dry-run surfaces the drift rather than previewing a phantom edit.
    let tmp = fixture();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntype: note\n---\nhello world\n").unwrap();

    let out = run_edit(
        &tmp,
        "note.md",
        EDITS,
        &["--expected-hash", "deadbeefstalehash", "--dry-run"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "stale expected-hash must refuse even on dry-run; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
