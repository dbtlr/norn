//! Phase 10 process-level integration tests for `vault new`.
//!
//! Tasks 10.1 (scaffolding + happy-path), 10.2 (--force and -p coverage),
//! 10.3 (schema-aware refusal paths), 10.4 (config-load failures),
//! 10.5 (post-create validate hook).

use std::fs;
use std::process::Command;
use tempfile::Builder;

fn vault_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vault")
}

/// Create a minimal tempdir vault with the given YAML in `.vault/config.yaml`.
fn build_vault(config_yaml: &str) -> tempfile::TempDir {
    let dir = Builder::new()
        .prefix("vault-new-process-")
        .tempdir()
        .unwrap();
    let vault_config_dir = dir.path().join(".vault");
    fs::create_dir_all(&vault_config_dir).unwrap();
    fs::write(vault_config_dir.join("config.yaml"), config_yaml).unwrap();
    dir
}

/// Build a `vault` Command with `--cwd` pointing at the vault tempdir.
fn vault_cmd(vault: &tempfile::TempDir) -> Command {
    let mut c = Command::new(vault_bin());
    c.arg("--cwd").arg(vault.path());
    c
}

// ── Task 10.1: scaffolding + happy-path ──────────────────────────────────────

#[test]
fn process_level_happy_path_dry_run_json() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: task-rule
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      required_frontmatter: [type, status, workspace]
      frontmatter_defaults:
        type: task
        status: backlog
        workspace: "[[{{path.workspace}}]]"
"#,
    );

    let output = vault_cmd(&vault)
        .args([
            "new",
            "Workspaces/foo/tasks/bar.md",
            "--dry-run",
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

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);

    assert_eq!(envelope["operation"], "new");
    assert_eq!(envelope["path"], "Workspaces/foo/tasks/bar.md");
    assert_eq!(envelope["applied"], false);

    let fc = envelope["frontmatter_created"].as_array().unwrap();
    let by_field: std::collections::HashMap<_, _> = fc
        .iter()
        .map(|f| (f["field"].as_str().unwrap().to_string(), f["value"].clone()))
        .collect();
    assert_eq!(by_field.get("type").unwrap(), &serde_json::json!("task"));
    assert_eq!(
        by_field.get("workspace").unwrap(),
        &serde_json::json!("[[foo]]")
    );
}

#[test]
fn process_level_apply_writes_file_with_yes() {
    let vault = build_vault("validate: {}\n");

    let output = vault_cmd(&vault)
        .args(["new", "foo.md", "--yes", "--field", "type=note"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let written = fs::read_to_string(vault.path().join("foo.md")).unwrap();
    assert!(written.starts_with("---\n"), "got:\n{written}");
    assert!(written.contains("type: note"), "got:\n{written}");
}

// ── Task 10.2: --force and -p coverage ───────────────────────────────────────

#[test]
fn process_level_refuses_existing_path_without_force() {
    let vault = build_vault("validate: {}\n");
    fs::write(vault.path().join("exists.md"), "old content").unwrap();

    let output = vault_cmd(&vault)
        .args(["new", "exists.md", "--yes", "--field", "type=note"])
        .output()
        .unwrap();

    // Exit code 2 per spec (pre-flight refusal).
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn process_level_force_overwrites_existing_path() {
    let vault = build_vault("validate: {}\n");
    fs::write(vault.path().join("exists.md"), "old content").unwrap();

    let output = vault_cmd(&vault)
        .args([
            "new",
            "exists.md",
            "--yes",
            "--force",
            "--field",
            "type=note",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let written = fs::read_to_string(vault.path().join("exists.md")).unwrap();
    assert!(!written.contains("old content"), "got:\n{written}");
    assert!(written.contains("type: note"), "got:\n{written}");
}

#[test]
fn process_level_refuses_missing_parent_without_parents_flag() {
    let vault = build_vault("validate: {}\n");

    let output = vault_cmd(&vault)
        .args(["new", "deep/nested/foo.md", "--yes", "--field", "type=note"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn process_level_parents_flag_creates_intermediate_dirs() {
    let vault = build_vault("validate: {}\n");

    let output = vault_cmd(&vault)
        .args([
            "new",
            "deep/nested/foo.md",
            "-p",
            "--yes",
            "--field",
            "type=note",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(vault.path().join("deep/nested/foo.md").exists());
}

#[test]
fn process_level_force_and_parents_combined() {
    let vault = build_vault("validate: {}\n");

    let output = vault_cmd(&vault)
        .args([
            "new",
            "fresh/dir/foo.md",
            "-p",
            "--force",
            "--yes",
            "--field",
            "type=note",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(vault.path().join("fresh/dir/foo.md").exists());
}

// ── Task 10.3: schema-aware refusal paths ────────────────────────────────────

#[test]
fn process_level_invalid_field_format_refuses() {
    let vault = build_vault("validate: {}\n");

    // --field key=value is required; passing "no_equals" should refuse.
    let output = vault_cmd(&vault)
        .args(["new", "foo.md", "--yes", "--field", "no_equals_sign"])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn process_level_invalid_field_json_refuses() {
    let vault = build_vault("validate: {}\n");

    let output = vault_cmd(&vault)
        .args([
            "new",
            "foo.md",
            "--yes",
            "--field-json",
            "tags={not valid json",
        ])
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn process_level_unresolved_wikilink_warns_does_not_refuse() {
    // Wikilink resolution failure is a warning, NOT a refusal.
    // Add a real doc so the index is populated; missing-stem won't be found.
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      field_types:
        workspace: wikilink
"#,
    );
    // Seed the vault with one real doc so the cache/index is non-trivially built.
    fs::write(
        vault.path().join("existing.md"),
        "---\ntype: note\n---\n# Existing\n",
    )
    .unwrap();

    let output = vault_cmd(&vault)
        .args([
            "new",
            "foo.md",
            "--yes",
            "--format",
            "json",
            "--field",
            "workspace=missing-stem",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success (warn only), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    let warnings = envelope["warnings"].as_array().unwrap();
    let kinds: Vec<&str> = warnings
        .iter()
        .map(|w| w["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"unresolved-wikilink"),
        "expected unresolved-wikilink warning, got: {kinds:?}"
    );
}

// ── Task 10.4: config-load failures ──────────────────────────────────────────

#[test]
fn process_level_config_load_rejects_unknown_path_var() {
    // Bad config: rule references {{path.bogus}} but bogus isn't declared in match.path.
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: r
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        title: "{{path.bogus}}"
"#,
    );

    let output = vault_cmd(&vault)
        .args(["new", "foo.md", "--dry-run"])
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
        stderr.contains("bogus") || stderr.contains("not declared") || stderr.contains("path"),
        "stderr: {stderr}"
    );
}

#[test]
fn process_level_config_load_rejects_unknown_transform() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        title: "{{title | bogus_transform}}"
"#,
    );

    let output = vault_cmd(&vault)
        .args(["new", "foo.md", "--dry-run"])
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
        stderr.contains("transform") || stderr.contains("bogus_transform"),
        "stderr: {stderr}"
    );
}

#[test]
fn process_level_config_load_rejects_conflicting_defaults() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: a
      match:
        path: "**/*.md"
      frontmatter_defaults:
        status: backlog
    - name: b
      match:
        path: "**/*.md"
      frontmatter_defaults:
        status: in_progress
"#,
    );

    let output = vault_cmd(&vault)
        .args(["new", "foo.md", "--dry-run"])
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
        stderr.contains("conflict") || stderr.contains("conflicting"),
        "stderr: {stderr}"
    );
}

// ── Task 10.5: post-create validate hook ─────────────────────────────────────

#[test]
fn process_level_post_create_validate_surfaces_missing_required_warning() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      required_frontmatter: [type, description]
      frontmatter_defaults:
        type: note
"#,
    );

    let output = vault_cmd(&vault)
        .args(["new", "foo.md", "--yes", "--format", "json"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    let warnings = envelope["warnings"].as_array().unwrap();
    let kinds: Vec<&str> = warnings
        .iter()
        .map(|w| w["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"missing-required-field"),
        "expected missing-required-field warning, got: {kinds:?}"
    );

    // File was actually written despite the warning.
    assert!(vault.path().join("foo.md").exists());
}

#[test]
fn process_level_validates_clean_when_all_fields_provided() {
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      required_frontmatter: [type, description]
      frontmatter_defaults:
        type: note
"#,
    );

    let output = vault_cmd(&vault)
        .args([
            "new",
            "foo.md",
            "--yes",
            "--format",
            "json",
            "--field",
            "description=Hello",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    let warnings = envelope["warnings"].as_array().unwrap();
    // No missing-required warning, because both required fields are present.
    let kinds: Vec<&str> = warnings
        .iter()
        .map(|w| w["kind"].as_str().unwrap())
        .collect();
    assert!(
        !kinds.contains(&"missing-required-field"),
        "got unexpected missing-required-field: {kinds:?}"
    );
}
