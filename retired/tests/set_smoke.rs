//! Phase 6 process-level integration tests for `vault set`.
//! Tasks 6.1 (happy path), 6.2 (refusals), 6.3 (combined ops + body change).

use std::fs;
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

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn norn_cmd(tmp: &tempfile::TempDir) -> Command {
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

fn fixture_tempdir() -> tempfile::TempDir {
    let tmp = Builder::new().prefix("norn-set-").tempdir().unwrap();
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

    let output = norn_cmd(&tmp)
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
    let output = norn_cmd(&tmp)
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
fn set_forgot_doc_field_shaped_target_hints() {
    // F4: `norn set status=done` (DOC forgotten) binds `status=done` as DOC and
    // fails to resolve. The not-found error should hint that the token looks
    // like a field assignment.
    let tmp = fixture_tempdir();
    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "status=done",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "field-shaped DOC that fails to resolve should refuse: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("looks like a field assignment") && stderr.contains("norn set <doc>"),
        "expected forgot-doc hint, got: {stderr}"
    );
}

#[test]
fn set_refuses_cross_class_conflict() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\ntags:\n- a\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
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

    let output = norn_cmd(&tmp)
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

/// NRN-221: a `--format json` schema refusal emits the structured
/// `{ code, message, path? }` error envelope on STDOUT with the SPECIFIC
/// kebab code — not the generic `internal-error` fallback, and not bare
/// prose on stderr. Matches the move/delete arms' NRN-150 contract.
#[test]
fn set_format_json_schema_refusal_emits_coded_error_envelope() {
    let tmp = Builder::new().prefix("norn-set-").tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(
        tmp.path().join(".norn/config.yaml"),
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - done\n",
    )
    .unwrap();
    let doc = tmp.path().join("task.md");
    fs::write(&doc, "---\ntype: task\nstatus: backlog\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "task.md",
            "--field",
            "status=bogus",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "a schema refusal exits 2; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be a JSON error envelope");
    assert_eq!(
        envelope["code"], "value-not-allowed",
        "the envelope carries the specific schema-refusal code, not internal-error; got: {envelope}"
    );
    assert!(
        envelope["message"]
            .as_str()
            .is_some_and(|m| m.contains("is not allowed for 'status'")),
        "envelope message preserves the prose; got: {envelope}"
    );
    // Disk untouched: the refusal fires before any write.
    let content = fs::read_to_string(&doc).unwrap();
    assert!(
        content.contains("status: backlog") && !content.contains("bogus"),
        "a refused set must write nothing: {content}"
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

    let mut child = norn_cmd(&tmp)
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
fn set_quote_requiring_key_collection_applies_and_round_trips() {
    // NRN-142: a `"#foo"` key that requires quoting must re-emit quoted on a
    // collection `set` (was pre-v0.44 corruption / v0.44 refusal). End-to-end:
    // the write succeeds and `get` reads the new array back under `#foo`.
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\n\"#foo\": [a, b]\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "--field-json",
            "#foo=[\"z\"]",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "set of quote-requiring key must succeed: stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The written file must still parse and read the key back byte-exactly.
    let final_content = fs::read_to_string(&doc).unwrap();
    let yaml = final_content
        .strip_prefix("---\n")
        .and_then(|r| r.split("\n---\n").next())
        .unwrap();
    let parsed: serde_json::Value =
        serde_yaml::from_str(yaml).expect("written frontmatter must parse");
    assert_eq!(parsed["#foo"], serde_json::json!(["z"]), "{final_content}");

    let get = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "get",
            "note.md",
            "--col",
            "#foo",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        get.status.success(),
        "get failed: {}",
        String::from_utf8_lossy(&get.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&get.stdout).expect("get json");
    let hay = json.to_string();
    assert!(
        hay.contains("\"z\""),
        "get output should carry the value: {hay}"
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

    let mut child = norn_cmd(&tmp)
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

    let output = norn_cmd(&tmp)
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

    let output = norn_cmd(&tmp)
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

    let output = norn_cmd(&tmp)
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

    let output = norn_cmd(&tmp)
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

    let output = norn_cmd(&tmp)
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

// === NRN-208: trailing KEY=VALUE positionals (ADR 0010 mutate sugar) ===

#[test]
fn set_positional_kv_desugars_to_field_write() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "status=active",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "set with positional k=v failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("status: active"),
        "positional k=v should write the field: {final_content}"
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let changes = json["frontmatter_changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["field"], "status");
    assert_eq!(changes[0]["new"], "active");
}

#[test]
fn set_positional_kv_accepts_colon_separator_forgiveness() {
    // Batch A's split_field_value forgiveness: `:` works as a separator too.
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "status:active",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "positional with colon separator failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("status: active"),
        "colon-separated positional should write the field: {final_content}"
    );
}

#[test]
fn set_positional_without_separator_hard_errors() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "nosep",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "separator-less positional should hard-error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("expected key=value") && stderr.contains("nosep"),
        "error should name the offending token: {stderr}"
    );
    // No mutation on refusal.
    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(final_content.contains("status: draft"), "{final_content}");
}

#[test]
fn set_bad_positional_fails_fast_without_lock_or_cache() {
    // F5: a pure argv error (separator-less positional) must be rejected BEFORE
    // the mutation lock is acquired and the cache is loaded — no side effects.
    // The cache DB lives under XDG_CACHE_HOME; if the fast path fired, that tree
    // is never created.
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    // This test asserts the cache home stays absent (proving the cache was never
    // loaded and the mutation lock never acquired), so it must NOT go through
    // `norn_cmd`, whose throttle-marker pre-write (NRN-287) would create
    // `.xdg-cache/norn/`. A bad-argv error is rejected before dispatch, so this
    // path never spawns a GC sweep child that would need suppressing anyway.
    let mut cmd = Command::new(norn_bin());
    cmd.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    let output = cmd
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "badtoken",
            "--yes",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "separator-less positional should fail fast: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("expected key=value") && stderr.contains("badtoken"),
        "error should name the offending token: {stderr}"
    );
    // No cache tree built => cache load never ran => the lock was never acquired.
    assert!(
        !tmp.path().join(".xdg-cache").exists(),
        "bad positional must not load the cache (no lock acquired)"
    );
    // File untouched.
    assert!(fs::read_to_string(&doc).unwrap().contains("status: draft"));
}

#[test]
fn set_positional_and_field_flag_both_accumulate() {
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("note.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "note.md",
            "status=active",
            "--field",
            "priority=high",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "mixing positional + --field failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("status: active") && final_content.contains("priority: high"),
        "both the positional and the --field should be written: {final_content}"
    );
}

#[test]
fn set_first_positional_is_doc_even_when_kv_shaped() {
    // Edge: a doc literally named `a=b.md` — the FIRST positional is always DOC,
    // the separator requirement applies only to positionals AFTER the first.
    let tmp = fixture_tempdir();
    let doc = tmp.path().join("a=b.md");
    fs::write(&doc, "---\nstatus: draft\n---\nbody\n").unwrap();

    let output = norn_cmd(&tmp)
        .args([
            "--cwd",
            tmp.path().to_str().unwrap(),
            "set",
            "a=b.md",
            "status=active",
            "--yes",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "kv-shaped DOC name should resolve as the first positional: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_content = fs::read_to_string(&doc).unwrap();
    assert!(
        final_content.contains("status: active"),
        "the second positional writes the field, DOC is `a=b.md`: {final_content}"
    );
}
