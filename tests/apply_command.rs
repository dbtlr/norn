//! Integration test for `norn apply <plan.yaml> --dry-run --format json`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-apply-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n[[a]]\n").unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
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

#[test]
fn apply_dry_run_returns_apply_report() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 2
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: renamed.md
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["operations"][0]["kind"], "move_document");
    assert!(
        !vault.join("renamed.md").exists(),
        "dry-run must not mutate"
    );
}

/// NRN-212: `--format json` is output-shape-only, matching every other
/// mutation command (set/delete/new/edit/move) — it must NOT be treated as
/// consent to apply. Without `--yes` and outside a TTY, `apply --format
/// json` must behave as an implicit dry-run: nothing written, report says
/// `dry_run: true`. Regression: the old code forced `is_apply`/`dry_run =
/// false` whenever `--format json` was present, so this call used to WRITE
/// the file with no `--yes` anywhere on the command line.
#[test]
fn apply_json_without_yes_is_implicit_dry_run_and_writes_nothing() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 2
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: renamed.md
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();

    let before = std::fs::read(vault.join("a.md")).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["dry_run"], true,
        "no --yes → implicit dry-run even under --format json: {report}"
    );
    assert!(
        vault.join("a.md").exists(),
        "a.md must NOT have been moved away"
    );
    assert!(
        !vault.join("renamed.md").exists(),
        "renamed.md must not exist — --format json alone must not apply"
    );
    let after = std::fs::read(vault.join("a.md")).unwrap();
    assert_eq!(before, after, "a.md must be byte-unchanged");
}

/// NRN-212 regression guard: `--format json --yes` must still apply.
#[test]
fn apply_json_with_yes_applies() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 2
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: renamed.md
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--format", "json", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["dry_run"], false);
    assert!(
        vault.join("renamed.md").exists(),
        "renamed.md must exist after --format json --yes"
    );
    assert!(
        !vault.join("a.md").exists(),
        "a.md must have been moved away"
    );
}

#[test]
fn apply_rejects_wrong_schema_version() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 99
vault_root: {}
operations: []
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--dry-run"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        out.status.code(),
        Some(2),
        "schema version mismatch is pre-flight refusal (exit 2)"
    );
}

/// NRN-150: a precondition refusal under `--format json` emits the structured
/// `{ code, message, path? }` error envelope on STDOUT (not just prose on stderr),
/// with the stable kebab code an agent branches on. Here a bogus `document_hash`
/// forces a `stale-document-hash` CAS refusal; exit is the preflight code 2.
#[test]
fn apply_json_failure_emits_structured_envelope_on_stdout() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().unwrap(),
        "operations": [{
            "kind": "add_frontmatter",
            "fields": {
                "path": "a.md",
                "field": "status",
                "new_value": "done",
                // Non-empty wrong hash: hydration only fills EMPTY hashes, so this
                // survives to the CAS check and refuses with stale-document-hash.
                "document_hash": "0000000000000000000000000000000000000000000000000000000000000000"
            }
        }]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, serde_json::to_string_pretty(&plan).unwrap()).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--format", "json"])
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(2),
        "CAS refusal is a preflight refusal (exit 2); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be a JSON envelope, got {stdout:?}: {e}"));
    assert_eq!(
        envelope["code"], "stale-document-hash",
        "envelope carries the stable kebab code: {envelope}"
    );
    assert_eq!(envelope["path"], "a.md", "envelope carries the path");
    assert!(
        envelope["message"].as_str().is_some(),
        "envelope carries a human message: {envelope}"
    );
    // The vault must be byte-identical after a precondition refusal.
    let a = std::fs::read_to_string(vault.join("a.md")).unwrap();
    assert_eq!(a, "---\ntype: note\n---\n# A\n");
}

#[test]
fn apply_reads_plan_from_stdin() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan_json = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().unwrap(),
        "operations": [{
            "kind": "move_document",
            "fields": {
                "src": "a.md",
                "dst": "renamed.md"
            }
        }]
    });

    use std::io::Write;
    let mut child = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply", "-", "--dry-run", "--format", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(plan_json.to_string().as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["operations"][0]["kind"], "move_document");
    assert!(
        !vault.join("renamed.md").exists(),
        "dry-run must not mutate"
    );
}

/// NRN-101 (review F3): two `{{seq}}` creates in ONE plan must preview distinct
/// ids on dry-run — earlier plan-local allocations are folded in, so the preview
/// matches what apply would actually write (MMR-1, MMR-2), not a duplicated id.
#[test]
fn apply_dry_run_multiple_seq_creates_predict_distinct_ids() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let mk = |p: &str| {
        serde_json::json!({
            "kind": "create_document",
            "fields": {
                "path": p,
                "new_value": { "frontmatter": {"type": "task"}, "body": "# t\n" }
            }
        })
    };
    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().unwrap(),
        "operations": [mk("tasks/MMR-{{seq}}.md"), mk("tasks/MMR-{{seq}}.md")]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, plan.to_string()).unwrap();
    std::fs::create_dir_all(vault.join("tasks")).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let summaries: Vec<String> = report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["summary"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        summaries.iter().any(|s| s.contains("MMR-1.md")),
        "expected a create of MMR-1.md; got {summaries:?}"
    );
    assert!(
        summaries.iter().any(|s| s.contains("MMR-2.md")),
        "expected the second create to predict MMR-2.md (not a duplicate MMR-1); got {summaries:?}"
    );
    assert!(
        !vault.join("tasks/MMR-1.md").exists(),
        "dry-run must not write"
    );
}

/// Ported from the deleted `repair_apply_out_orthogonality` suite: `--out`
/// writes the JSON ApplyReport to a file and keeps stdout silent.
#[test]
fn apply_out_alone_writes_file_and_keeps_stdout_silent() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan = format!(
        r#"schema_version: 2
vault_root: {}
operations: []
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, plan).unwrap();
    let out_path = tmp.path().join("report.json");

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--dry-run", "--out"])
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "",
        "stdout must be silent when --out is set"
    );
    assert!(out_path.exists(), "report file must be written");
    let body = std::fs::read_to_string(&out_path).unwrap();
    assert!(
        body.trim_start().starts_with('{'),
        "report file must contain JSON, got: {body}"
    );
}

/// Ported from the deleted `repair_apply_stdin` suite: malformed stdin is a
/// pre-flight refusal (non-zero exit) with a parse error on stderr.
#[test]
fn apply_malformed_stdin_exits_non_zero() {
    let tmp = synth();
    let vault = tmp.path().join("vault");

    use std::io::Write;
    let mut child = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply", "-", "--dry-run", "--format", "json"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"not json")
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success(), "should fail on malformed stdin");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("parse"),
        "expected a parse error on stderr, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// T5 — per-op cascade attribution across multiple move_document ops
// ---------------------------------------------------------------------------

#[test]
fn apply_per_op_cascade_attribution_multi_move() {
    // Two docs (p.md and q.md), each with a DISTINCT set of backlinkers.
    // p.md is referenced by 1 file; q.md is referenced by 2 files.
    // After a dry-run apply, each move_document op must carry its OWN
    // cascade.planned — not a shared aggregate.
    let tmp = tempfile::Builder::new()
        .prefix("norn-apply-cascade-t5-")
        .tempdir()
        .unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir(&vault).unwrap();

    // Docs being moved
    std::fs::write(vault.join("p.md"), "---\ntype: note\n---\n# P\n").unwrap();
    std::fs::write(vault.join("q.md"), "---\ntype: note\n---\n# Q\n").unwrap();
    // 1 backlinker for p
    std::fs::write(
        vault.join("p_link1.md"),
        "---\ntype: note\n---\n# P-link-1\n[[p]]\n",
    )
    .unwrap();
    // 2 backlinkers for q
    std::fs::write(
        vault.join("q_link1.md"),
        "---\ntype: note\n---\n# Q-link-1\n[[q]]\n",
    )
    .unwrap();
    std::fs::write(
        vault.join("q_link2.md"),
        "---\ntype: note\n---\n# Q-link-2\n[[q]]\n",
    )
    .unwrap();

    let plan = format!(
        r#"schema_version: 2
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: p.md
      dst: p_renamed.md
  - kind: move_document
    fields:
      src: q.md
      dst: q_renamed.md
"#,
        vault.to_str().unwrap()
    );
    let plan_path = tmp.path().join("plan.yaml");
    std::fs::write(&plan_path, &plan).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));

    assert_eq!(report["dry_run"], true, "must be dry-run");

    let ops = report["operations"].as_array().expect("operations array");
    let move_ops: Vec<&serde_json::Value> = ops
        .iter()
        .filter(|o| o["kind"] == "move_document")
        .collect();
    assert_eq!(
        move_ops.len(),
        2,
        "expected 2 move_document ops; got: {move_ops:?}"
    );

    // Find the op for p.md (summary contains "p.md")
    let p_op = move_ops
        .iter()
        .find(|o| o["summary"].as_str().unwrap_or("").contains("p.md"))
        .unwrap_or_else(|| panic!("op for p.md not found; ops: {move_ops:?}"));
    // Find the op for q.md
    let q_op = move_ops
        .iter()
        .find(|o| o["summary"].as_str().unwrap_or("").contains("q.md"))
        .unwrap_or_else(|| panic!("op for q.md not found; ops: {move_ops:?}"));

    let p_cascade = &p_op["cascade"];
    let q_cascade = &q_op["cascade"];

    assert!(
        !p_cascade.is_null(),
        "p.md move must carry a cascade summary"
    );
    assert!(
        !q_cascade.is_null(),
        "q.md move must carry a cascade summary"
    );

    // Per-op attribution: p has 1 backlinker, q has 2 — must NOT be aggregated
    assert_eq!(
        p_cascade["planned"], 1,
        "p.md op: planned must be 1 (only p_link1.md references it); cascade: {p_cascade}"
    );
    assert_eq!(
        q_cascade["planned"], 2,
        "q.md op: planned must be 2 (q_link1.md + q_link2.md reference it); cascade: {q_cascade}"
    );

    // Dry-run must not mutate
    assert!(
        !vault.join("p_renamed.md").exists(),
        "dry-run must not move p"
    );
    assert!(
        !vault.join("q_renamed.md").exists(),
        "dry-run must not move q"
    );
}

#[test]
fn apply_stdin_with_input_format_yaml_works() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let plan_yaml = format!(
        r#"schema_version: 2
vault_root: {}
operations:
  - kind: move_document
    fields:
      src: a.md
      dst: renamed.md
"#,
        vault.to_str().unwrap()
    );

    use std::io::Write;
    let mut child = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args([
            "apply",
            "-",
            "--input-format",
            "yaml",
            "--dry-run",
            "--format",
            "json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(plan_yaml.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
}

// ---------------------------------------------------------------------------
// NRN-100 (H2) — a `create_document` and a `replace_body` op compose into ONE
// MigrationPlan and apply as a single batch. Both op kinds route through the
// same pass-through arm of `expand()` (src/planner/intent/mod.rs), so the
// applier treats them as ordinary planned changes; the guarantee under test is
// that a create of a NEW doc and a CAS-checked body replacement of an EXISTING
// doc land together from one plan.
// ---------------------------------------------------------------------------

/// blake3 hex digest of a file's bytes — matches vault-graph's hash function so
/// a `replace_body` op can carry a valid `document_hash` (CAS precondition).
fn blake3_of_file(path: &std::path::Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap())
        .to_hex()
        .to_string()
}

#[test]
fn apply_composes_create_document_and_replace_body() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    // a.md exists ("---\ntype: note\n---\n# A\n"); c.md does not yet.
    let a_path = vault.join("a.md");
    let a_hash = blake3_of_file(&a_path);

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().unwrap(),
        "operations": [
            {
                "kind": "create_document",
                "fields": {
                    "path": "c.md",
                    "new_value": { "frontmatter": {"type": "note"}, "body": "# C\n" }
                }
            },
            {
                "kind": "replace_body",
                "fields": {
                    "path": "a.md",
                    "document_hash": a_hash,
                    "new_value": "# A (rewritten)\n"
                }
            }
        ]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, plan.to_string()).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The new doc was created with the given frontmatter + body.
    let c = std::fs::read_to_string(vault.join("c.md")).unwrap();
    assert!(c.contains("type: note"), "c.md frontmatter; got:\n{c}");
    assert!(c.contains("# C"), "c.md body; got:\n{c}");
    // The existing doc's body was replaced while its frontmatter was preserved.
    let a = std::fs::read_to_string(&a_path).unwrap();
    assert!(
        a.contains("type: note"),
        "a.md frontmatter preserved; got:\n{a}"
    );
    assert!(
        a.contains("# A (rewritten)") && !a.contains("# A\n"),
        "a.md body replaced; got:\n{a}"
    );
}

#[test]
fn apply_composed_create_and_replace_body_dry_run_does_not_mutate() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let a_path = vault.join("a.md");
    let a_before = std::fs::read_to_string(&a_path).unwrap();
    let a_hash = blake3_of_file(&a_path);

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().unwrap(),
        "operations": [
            {
                "kind": "create_document",
                "fields": {
                    "path": "c.md",
                    "new_value": { "frontmatter": {"type": "note"}, "body": "# C\n" }
                }
            },
            {
                "kind": "replace_body",
                "fields": {
                    "path": "a.md",
                    "document_hash": a_hash,
                    "new_value": "# A (rewritten)\n"
                }
            }
        ]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, plan.to_string()).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    let kinds: Vec<&str> = report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["kind"].as_str().unwrap_or_default())
        .collect();
    assert!(
        kinds.contains(&"create_document") && kinds.contains(&"replace_body"),
        "both ops present in the report; got {kinds:?}"
    );
    assert!(!vault.join("c.md").exists(), "dry-run must not create c.md");
    assert_eq!(
        std::fs::read_to_string(&a_path).unwrap(),
        a_before,
        "dry-run must not mutate a.md"
    );
}

/// The CAS precondition still guards a `replace_body` even when it rides in a
/// plan alongside a `create_document`: a stale hash aborts the whole apply and
/// the sibling create must NOT have landed (compute-all-validate-then-write).
#[test]
fn apply_composed_stale_replace_body_hash_aborts_and_create_does_not_land() {
    let tmp = synth();
    let vault = tmp.path().join("vault");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": vault.to_str().unwrap(),
        "operations": [
            {
                "kind": "create_document",
                "fields": {
                    "path": "c.md",
                    "new_value": { "frontmatter": {"type": "note"}, "body": "# C\n" }
                }
            },
            {
                "kind": "replace_body",
                "fields": {
                    "path": "a.md",
                    "document_hash": "definitely-wrong-hash",
                    "new_value": "# A (rewritten)\n"
                }
            }
        ]
    });
    let plan_path = tmp.path().join("plan.json");
    std::fs::write(&plan_path, plan.to_string()).unwrap();

    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["apply"])
        .arg(&plan_path)
        .args(["--yes"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "stale hash must abort the apply; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    // Pin the failure CAUSE to the CAS check, not an unrelated error — otherwise
    // this test could pass for the wrong reason (e.g. a schema/parse refusal).
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stale") || stderr.contains("hash"),
        "expected a stale-hash error on stderr, got: {stderr}"
    );
    assert!(
        !vault.join("c.md").exists(),
        "sibling create must not land when the composed plan aborts on a stale hash"
    );
}
