//! Integration tests for `norn new` rule-targeted creation (NRN-51 Task 5).
//!
//! Tests the three-mode path resolution:
//!   1. `--as RULE` → derives path from the rule's `target` template.
//!   2. No path, no `--as`, inbox configured → lands in inbox/<slug>.md.
//!   3. Refusal paths (unknown rule, missing var).

use std::fs;
use std::process::Command;
use tempfile::Builder;

fn norn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn")
}

/// Create a minimal tempdir vault with the given YAML in `.norn/config.yaml`.
fn build_vault(config_yaml: &str) -> tempfile::TempDir {
    let dir = Builder::new()
        .prefix("norn-new-rule-target-")
        .tempdir()
        .unwrap();
    let vault_config_dir = dir.path().join(".norn");
    fs::create_dir_all(&vault_config_dir).unwrap();
    fs::write(vault_config_dir.join("config.yaml"), config_yaml).unwrap();
    dir
}

/// Build a `norn` Command with `--cwd` pointing at the vault tempdir, and
/// cache/state dirs isolated so we never read the developer's real trees.
fn norn_cmd(vault: &tempfile::TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.arg("--cwd").arg(vault.path());
    c.env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"));
    c
}

// ── Scenario 1: --as targets a named rule, derives path from target template ──

#[test]
fn new_targets_named_rule_and_derives_path() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: task
      target: "tasks/{{title|slugify}}.md"
      frontmatter_defaults:
        type: task
"#,
    );

    let output = norn_cmd(&vault)
        .args([
            "new",
            "--as",
            "task",
            "--title",
            "Fix Audit Reader",
            "--yes",
            "-p",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // File should exist at the derived path.
    let expected_path = vault.path().join("tasks/fix-audit-reader.md");
    assert!(
        expected_path.exists(),
        "expected file at tasks/fix-audit-reader.md, vault dir: {:?}",
        fs::read_dir(vault.path()).unwrap().collect::<Vec<_>>()
    );

    // The frontmatter default from the rule should be applied.
    let written = fs::read_to_string(&expected_path).unwrap();
    assert!(
        written.contains("type: task"),
        "expected type: task in frontmatter, got:\n{written}"
    );

    // JSON envelope should confirm the path and applied=true.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    assert_eq!(envelope["applied"], serde_json::json!(true));
    assert_eq!(
        envelope["path"],
        serde_json::json!("tasks/fix-audit-reader.md")
    );
}

// ── Scenario 2: No path, no --as → inbox fallback ─────────────────────────────

#[test]
fn new_without_route_lands_in_inbox() {
    let vault = build_vault(
        r#"
inbox:
  path: Inbox
validate: {}
"#,
    );

    // The Inbox directory must exist (no -p given; inbox fallback doesn't auto-create).
    fs::create_dir_all(vault.path().join("Inbox")).unwrap();

    let output = norn_cmd(&vault)
        .args([
            "new",
            "--title",
            "Random Thought",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected_path = vault.path().join("Inbox/random-thought.md");
    assert!(
        expected_path.exists(),
        "expected file at Inbox/random-thought.md"
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    assert_eq!(envelope["applied"], serde_json::json!(true));
    assert_eq!(
        envelope["path"],
        serde_json::json!("Inbox/random-thought.md")
    );
}

// ── Scenario 3: Unknown rule → exit 2 ────────────────────────────────────────

#[test]
fn new_unknown_rule_refuses_exit_2() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: task
      target: "tasks/{{title|slugify}}.md"
"#,
    );

    let output = norn_cmd(&vault)
        .args(["new", "--as", "nope", "--title", "X"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("nope"),
        "expected unknown rule name in stderr, got: {stderr}"
    );
}

// ── Scenario 4: Missing required var → exit 2 ────────────────────────────────

#[test]
fn new_missing_var_refuses_exit_2() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: workspace-task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
      frontmatter_defaults:
        type: task
"#,
    );

    // Run WITHOUT --var workspace=... so generate_path should fail on MissingVar.
    let output = norn_cmd(&vault)
        .args(["new", "--as", "workspace-task", "--title", "My Task"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("workspace"),
        "expected missing var name `workspace` in stderr, got: {stderr}"
    );
}

// ── Bonus: --var supplies path-vars that resolve in target template ────────────

#[test]
fn new_with_var_resolves_template_correctly() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: workspace-task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
      frontmatter_defaults:
        type: task
        workspace: "{{var.workspace}}"
"#,
    );

    // Pre-create the directory since parents aren't enabled.
    fs::create_dir_all(vault.path().join("Workspaces/norn/tasks")).unwrap();

    let output = norn_cmd(&vault)
        .args([
            "new",
            "--as",
            "workspace-task",
            "--title",
            "Fix Audit Reader",
            "--var",
            "workspace=norn",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected_path = vault
        .path()
        .join("Workspaces/norn/tasks/fix-audit-reader.md");
    assert!(expected_path.exists(), "expected file at derived path");

    let written = fs::read_to_string(&expected_path).unwrap();
    assert!(written.contains("type: task"), "got:\n{written}");

    // Verify path_vars from --var flow into frontmatter substitution.
    assert!(
        written.contains("workspace: norn"),
        "expected workspace: norn from var substitution, got:\n{written}"
    );
}

// ── NRN-51: inbox-mode frontmatter substitution uses --var values ─────────────
//
// Mode C (inbox fallback) must thread `--var` into `build_plan`'s
// `extra_path_vars` so that frontmatter defaults referencing `{{var.X}}`
// resolve to the supplied value rather than an empty string.

#[test]
fn inbox_mode_var_resolves_in_frontmatter_defaults() {
    let vault = build_vault(
        r#"
inbox:
  path: Inbox
validate:
  rules:
    - name: inbox-watcher
      match:
        path: "Inbox/**/*.md"
      frontmatter_defaults:
        type: inbox
        project: "{{var.project}}"
"#,
    );

    // The Inbox directory must exist (no -p given).
    fs::create_dir_all(vault.path().join("Inbox")).unwrap();

    let output = norn_cmd(&vault)
        .args([
            "new",
            "--title",
            "My Inbox Note",
            "--var",
            "project=norn",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let expected_path = vault.path().join("Inbox/my-inbox-note.md");
    assert!(
        expected_path.exists(),
        "expected file at Inbox/my-inbox-note.md"
    );

    let written = fs::read_to_string(&expected_path).unwrap();

    // The key assertion: `project` must contain the resolved value `norn`,
    // not an empty string (which would appear as `project: ''` or `project: ""`).
    assert!(
        written.contains("project: norn"),
        "expected 'project: norn' in frontmatter (var must resolve in inbox mode), got:\n{written}"
    );
}

// ── Error: both path and --as given → exit 2 ─────────────────────────────────

#[test]
fn new_both_path_and_as_refuses_exit_2() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: task
      target: "tasks/{{title|slugify}}.md"
"#,
    );

    let output = norn_cmd(&vault)
        .args(["new", "explicit/path.md", "--as", "task", "--title", "X"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 when both path and --as given, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
