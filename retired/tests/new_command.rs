//! Phase 10 process-level integration tests for `norn new`.
//!
//! Tasks 10.1 (scaffolding + happy-path), 10.2 (--force and -p coverage),
//! 10.3 (schema-aware refusal paths), 10.4 (config-load failures),
//! 10.5 (post-create validate hook).

use std::fs;
use std::process::Command;
use tempfile::Builder;

fn norn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_norn")
}

/// Create a minimal tempdir vault with the given YAML in `.norn/config.yaml`.
fn build_vault(config_yaml: &str) -> tempfile::TempDir {
    let dir = Builder::new()
        .prefix("norn-new-process-")
        .tempdir()
        .unwrap();
    let vault_config_dir = dir.path().join(".norn");
    fs::create_dir_all(&vault_config_dir).unwrap();
    fs::write(vault_config_dir.join("config.yaml"), config_yaml).unwrap();
    dir
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

/// Build a `norn` Command with `--cwd` pointing at the vault tempdir, and
/// `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to hidden subdirs of it so the
/// binary never reads or sweeps the developer's real cache/state trees.
fn norn_cmd(vault: &tempfile::TempDir) -> Command {
    prewrite_prune_marker(&vault.path().join(".xdg-cache"));
    let mut c = Command::new(norn_bin());
    c.arg("--cwd").arg(vault.path());
    c.env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"));
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

    let output = norn_cmd(&vault)
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
fn process_level_list_selector_rule_gates_defaults() {
    // An any-of selector (NRN-71) must gate frontmatter_defaults the same
    // way it gates validation: `--field type=task` puts the doc inside the
    // any-of set, so the rule's status default applies.
    let vault = build_vault(
        r#"
validate:
  rules:
    - name: node-base
      match:
        frontmatter:
          type: [task, phase]
      required_frontmatter: [status]
      frontmatter_defaults:
        status: backlog
"#,
    );

    let output = norn_cmd(&vault)
        .args([
            "new",
            "t1.md",
            "--field",
            "type=task",
            "--dry-run",
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
    let fc = envelope["frontmatter_created"].as_array().unwrap();
    assert!(
        fc.iter()
            .any(|f| f["field"] == "status" && f["value"] == "backlog"),
        "status default should apply via the any-of selector: {stdout}"
    );

    // Outside the any-of set: the default must NOT apply.
    let output = norn_cmd(&vault)
        .args([
            "new",
            "n1.md",
            "--field",
            "type=note",
            "--dry-run",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(&stdout).expect(&stdout);
    let fc = envelope["frontmatter_created"].as_array().unwrap();
    assert!(
        !fc.iter().any(|f| f["field"] == "status"),
        "note is outside the any-of set; no status default expected: {stdout}"
    );
}

#[test]
fn process_level_apply_writes_file_with_yes() {
    let vault = build_vault("validate: {}\n");

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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
    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

#[test]
fn process_level_ambiguous_wikilink_warns_with_candidates() {
    // A wikilink whose stem resolves to MULTIPLE docs is a distinct warning kind
    // (`ambiguous-wikilink`) from the no-candidates case — and carries the
    // candidate paths so the operator can disambiguate. Warn only, never refuse.
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
    // Two docs sharing the stem `shared` in different directories.
    fs::create_dir_all(vault.path().join("a")).unwrap();
    fs::create_dir_all(vault.path().join("b")).unwrap();
    fs::write(
        vault.path().join("a/shared.md"),
        "---\ntype: note\n---\n# A\n",
    )
    .unwrap();
    fs::write(
        vault.path().join("b/shared.md"),
        "---\ntype: note\n---\n# B\n",
    )
    .unwrap();

    let output = norn_cmd(&vault)
        .args([
            "new",
            "foo.md",
            "--yes",
            "--format",
            "json",
            "--field",
            "workspace=shared",
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
    let ambiguous = warnings
        .iter()
        .find(|w| w["kind"].as_str() == Some("ambiguous-wikilink"))
        .unwrap_or_else(|| panic!("expected ambiguous-wikilink warning, got: {warnings:?}"));
    let candidates: Vec<&str> = ambiguous["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert!(
        candidates.contains(&"a/shared.md") && candidates.contains(&"b/shared.md"),
        "expected both candidate paths, got: {candidates:?}"
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

    let output = norn_cmd(&vault)
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

// ── NRN-230 (PR A): resolve/synth refusal prose is BYTE-IDENTICAL ───────────
//
// The CLI `norn new` refusal surface has no JSON error envelope (deliberate —
// see docs/errors.md's MCP-only coded-refusal contract); it prints
// `error: {Display}` to stderr and exits 2. Coding these refusals for the MCP
// surface (NRN-230) must NOT change one byte of this CLI-side prose. These
// pin the exact stderr text for a representative newly-typed refusal from
// each family (`NewResolveError` / F3, `SynthError` / F4).

#[test]
fn process_level_path_and_rule_conflict_prints_exact_prose() {
    let vault = build_vault("validate: {}\n");

    let output = norn_cmd(&vault)
        .args(["new", "foo.md", "--as", "whatever", "--yes"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: pass either a path or --as, not both\n"
    );
}

#[test]
fn process_level_unknown_rule_prints_exact_prose() {
    let vault = build_vault("validate: {}\n");

    let output = norn_cmd(&vault)
        .args(["new", "--as", "bogus-rule", "--yes"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: unknown rule `bogus-rule`\n"
    );
}

#[test]
fn process_level_no_inbox_configured_prints_exact_prose() {
    let vault = build_vault("validate: {}\n");

    let output = norn_cmd(&vault)
        .args(["new", "--title", "Some Title", "--yes"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: no path, no --as, and no inbox configured\n"
    );
}

#[test]
fn process_level_invalid_field_format_prints_exact_prose() {
    let vault = build_vault("validate: {}\n");

    let output = norn_cmd(&vault)
        .args(["new", "foo.md", "--yes", "--field", "no_equals_sign"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "error: invalid --field format (expected key=value): no_equals_sign\n"
    );
}

#[test]
fn process_level_body_scaffold_render_failure_prints_exact_prose() {
    // NRN-230: typing the body-scaffold render refusal (BodyScaffoldRenderError,
    // coded `template-render-failed` on MCP) must keep the CLI's stderr prose
    // byte-identical, in both formats — the CLI new path has no JSON error
    // envelope (deliberate).
    let vault = build_vault(
        "validate:\n  rules:\n    - name: scaffolded\n      target: \"fixed.md\"\n      body: \"hello {{bogus}}\"\n",
    );

    for format_args in [&[][..], &["--format", "json"][..]] {
        let output = norn_cmd(&vault)
            .args(["new", "--as", "scaffolded", "--yes"])
            .args(format_args)
            .output()
            .unwrap();

        assert_eq!(
            output.status.code(),
            Some(2),
            "format args: {format_args:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "error: body scaffold render error: unknown variable `bogus`\n",
            "format args: {format_args:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "the CLI new refusal path emits nothing on stdout (format args: {format_args:?})"
        );
    }
}
