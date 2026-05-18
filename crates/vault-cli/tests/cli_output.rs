use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/basic")
}

fn vault(args: &[&str]) -> String {
    vault_success(args).0
}

fn vault_success(args: &[&str]) -> (String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args(args)
        .output()
        .expect("vault command should run");

    assert!(
        output.status.success(),
        "vault command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    (
        String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
        String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
    )
}

fn vault_success_env(args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("vault command should run");

    assert!(
        output.status.success(),
        "vault command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    (
        String::from_utf8(output.stdout).expect("stdout should be UTF-8"),
        String::from_utf8(output.stderr).expect("stderr should be UTF-8"),
    )
}

fn vault_error(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args(args)
        .output()
        .expect("vault command should run");

    assert!(
        !output.status.success(),
        "vault command succeeded unexpectedly\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

fn vault_error_env(args: &[&str], envs: &[(&str, &str)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_vault"));
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command.output().expect("vault command should run");

    assert!(
        !output.status.success(),
        "vault command succeeded unexpectedly\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

fn vault_in_dir(args: &[&str], current_dir: &PathBuf) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .current_dir(current_dir)
        .args(args)
        .output()
        .expect("vault command should run");

    assert!(
        output.status.success(),
        "vault command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("stdout should be UTF-8")
}

fn temp_cache_dir() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let process = std::process::id();
    std::env::temp_dir().join(format!("vault-cli-cache-{process}-{unique}-{counter}"))
}

#[test]
fn vault_version_reports_package_version() {
    let output = vault(&["--version"]);
    assert!(output.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn vault_help_documents_global_cwd() {
    let output = vault(&["--help"]);
    assert!(output.contains("-C, --cwd"));
    assert!(output.contains("--vault"));
    assert!(output.contains("--config"));
    assert!(output.contains("--verbose"));
    assert!(output.contains("docs"));
    assert!(output.contains("files"));
    assert!(output.contains("links"));
    assert!(output.contains("search"));
    assert!(output.contains("registry"));
    assert!(output.contains("repair"));
    assert!(!output.contains("cache"));
}

#[test]
fn graph_umbrella_is_removed() {
    let error = vault_error(&["graph", "--help"]);
    assert!(error.contains("unrecognized subcommand 'graph'"));
}

#[test]
fn grouped_help_lists_new_surfaces() {
    let output = vault(&["docs", "--help"]);
    assert!(output.contains("Parsed Markdown documents"));
    assert!(output.contains("list"));
    assert!(output.contains("inspect"));

    let output = vault(&["links", "--help"]);
    assert!(output.contains("Link facts across the vault"));
    assert!(output.contains("unresolved"));
    assert!(output.contains("backlinks"));

    let error = vault_error(&["cache", "--help"]);
    assert!(error.contains("unrecognized subcommand 'cache'"));

    let output = vault(&["registry", "--help"]);
    assert!(output.contains("Manage named vault roots"));
    assert!(output.contains("add"));
    assert!(output.contains("list"));
    assert!(output.contains("remove"));

    let output = vault(&["repair", "--help"]);
    assert!(output.contains("Plan and apply deterministic vault repairs"));
    assert!(output.contains("plan"));
    assert!(output.contains("links"));

    let output = vault(&["repair", "plan", "--help"]);
    assert!(output.contains("[possible values: json, jsonl, table]"));
    assert!(output.contains("skipped, unsupported, and ambiguous findings"));
    assert!(output.contains("--out"));
    assert!(!output.contains("paths"));

    let output = vault(&["repair", "apply", "--help"]);
    assert!(output.contains("reports skipped fallout as context"));
    assert!(output.contains("stale hashes"));
    assert!(!output.contains("manual-decision"));

    let output = vault(&["search", "--help"]);
    assert!(output.contains("Deterministic document search"));
    assert!(output.contains("--filter"));
    assert!(output.contains("--path"));
    assert!(output.contains("--has"));
    assert!(output.contains("--missing"));
    assert!(output.contains("--text"));
}

#[test]
fn repair_links_reports_link_drift_and_duplicate_stems() {
    let root = fixture_root();
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "repair",
        "links",
        "--format",
        "json",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("link report should be JSON");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["summary"]["unresolved_links"], 5);
    assert_eq!(report["summary"]["ambiguous_links"], 1);
    assert_eq!(report["summary"]["duplicate_stem_risks"], 1);
    assert_eq!(report["ambiguous_links"][0]["target"], "duplicate");
    assert_eq!(
        report["ambiguous_links"][0]["candidates"][0],
        "duplicate.md"
    );
    assert!(report["ambiguous_links"][0]["decision"]
        .as_str()
        .unwrap()
        .starts_with("skipped:"));
    assert_eq!(report["duplicate_stem_risks"][0]["stem"], "duplicate");
    assert!(report["unresolved_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| {
            link["unresolved_reason"] == "anchor-missing"
                && link["anchor"] == "Missing Same Heading"
        }));
    assert!(report["path_style_markdown_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["target"] == "folder/delta.md"));
    assert!(report["unresolved_links"][0]["decision"]
        .as_str()
        .unwrap()
        .starts_with("skipped:"));
}

#[test]
fn repair_links_reports_target_move_and_delete_risk() {
    let root = fixture_root();
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "repair",
        "links",
        "--target",
        "alpha",
        "--format",
        "json",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("link report should be JSON");
    assert_eq!(report["target_risk"]["target_path"], "alpha.md");
    assert_eq!(report["target_risk"]["incoming_link_count"], 6);
    assert!(report["target_risk"]["incoming_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["source_path"] == "beta.md"));
    assert!(report["target_risk"]["delete_risk"]
        .as_str()
        .unwrap()
        .contains("break indexed incoming links"));
    assert!(report["affected_files"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path == "beta.md"));
}

#[test]
fn repair_links_table_is_row_oriented() {
    let root = fixture_root();
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "repair",
        "links",
        "--target",
        "alpha",
        "--format",
        "table",
    ]);

    assert!(output.contains("unresolved_links"));
    assert!(output.contains("category"));
    assert!(output.contains("ambiguous"));
    assert!(output.contains("path-style"));
    assert!(output.contains("target_path"));
    assert!(output.contains("incoming_sources"));
    assert!(output.contains("skipped:"));
    assert!(!output.contains("manual decision required"));
    assert!(!output.contains("\"unresolved_links\""));
}

#[test]
fn repair_plan_generates_configured_frontmatter_change() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\n          - completed\n          - wont_do\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        rule: task-status\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--code",
        "frontmatter-disallowed-value,frontmatter-forbidden-field",
        "--field",
        "status",
    ]);

    let plan = serde_json::from_str::<Value>(&output).expect("repair plan should be JSON");
    assert_eq!(plan["schema_version"], 2);
    assert_eq!(plan["summary"]["findings"], 1);
    assert_eq!(plan["summary"]["planned_changes"], 1);
    assert_eq!(plan["summary"]["skipped_findings"], 0);
    assert_eq!(plan["summary"]["unsupported_findings"], 0);
    assert_eq!(
        plan["source_filters"]["code"],
        serde_json::json!([
            "frontmatter-disallowed-value",
            "frontmatter-forbidden-field"
        ])
    );
    assert_eq!(plan["changes"][0]["path"], "task.md");
    assert!(plan["changes"][0]["document_hash"].as_str().unwrap().len() > 20);
    assert_eq!(plan["changes"][0]["repair_rule"], "map-someday-status");
    assert_eq!(plan["changes"][0]["operation"], "set_frontmatter");
    assert_eq!(plan["changes"][0]["field"], "status");
    assert_eq!(plan["changes"][0]["expected_old_value"], "someday");
    assert_eq!(plan["changes"][0]["new_value"], "backlog");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_plan_out_writes_json_artifact_without_stdout() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");
    let plan_path = root.join("repair.json");

    let (stdout, _stderr) = vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    assert_eq!(stdout, "");
    let plan_text = fs::read_to_string(&plan_path).expect("plan should write");
    let plan = serde_json::from_str::<Value>(&plan_text).expect("repair plan should be JSON");
    assert_eq!(plan["summary"]["planned_changes"], 1);
    assert_eq!(plan["changes"][0]["path"], "task.md");

    let error = vault_error(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--format",
        "table",
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    assert!(error.contains("repair plan --out writes JSON artifacts"));
    assert!(error.contains("omit --out for table output"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn broad_repair_plan_with_skipped_findings_still_applies_changes() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n\n[[missing]]\n",
    )
    .expect("task should write");

    let plan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
    ]);
    let plan_json = serde_json::from_str::<Value>(&plan).expect("repair plan should be JSON");
    assert_eq!(plan_json["summary"]["planned_changes"], 1);
    assert_eq!(plan_json["summary"]["skipped_findings"], 1);
    assert_eq!(plan_json["skipped_findings"][0]["code"], "link-unresolved");
    assert!(plan_json["skipped_findings"][0]["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str().unwrap().contains("rewrite")));

    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "apply",
        plan_path.to_str().unwrap(),
        "--dry-run",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["applied_changes"], 1);
    assert_eq!(report["changed_files"][0], "task.md");
    assert_eq!(report["plan_context"]["skipped_findings"], 1);
    assert_eq!(report["plan_context"]["unsupported_findings"], 1);
    assert_eq!(report["plan_context"]["ambiguous_findings"], 0);

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_plan_table_is_row_oriented() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n\n[[missing]]\n",
    )
    .expect("task should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--format",
        "table",
    ]);

    assert!(output.contains("planned_changes"));
    assert!(output.contains("skipped_findings"));
    assert!(output.contains("task.md"));
    assert!(output.contains("set_frontmatter"));
    assert!(output.contains("link-unresolved"));
    assert!(!output.contains("\"changes\""));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_apply_writes_frontmatter_plan_and_verifies() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n\nBody stays.\n",
    )
    .expect("task should write");

    let plan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
        "--code",
        "frontmatter-disallowed-value",
        "--field",
        "status",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "apply",
        plan_path.to_str().unwrap(),
        "--verify",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["applied_changes"], 1);
    assert_eq!(report["changed_files"][0], "task.md");
    assert_eq!(report["plan_context"]["skipped_findings"], 0);
    assert_eq!(report["plan_context"]["unsupported_findings"], 0);
    assert_eq!(report["plan_context"]["ambiguous_findings"], 0);
    assert_eq!(report["verification"]["remaining_findings"], 0);

    let updated = fs::read_to_string(root.join("task.md")).expect("task should read");
    assert!(updated.contains("status: backlog"));
    assert!(updated.contains("# Task\n\nBody stays.\n"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_apply_dry_run_does_not_write() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");

    let plan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "apply",
        plan_path.to_str().unwrap(),
        "--dry-run",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["changed_files"][0], "task.md");
    let unchanged = fs::read_to_string(root.join("task.md")).expect("task should read");
    assert!(unchanged.contains("status: someday"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_apply_rejects_stale_plan() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: frontmatter-disallowed-value\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");

    let plan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: later\n---\n# Task\n",
    )
    .expect("task should write");

    let error = vault_error(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "apply",
        plan_path.to_str().unwrap(),
    ]);

    assert!(error.contains("stale repair plan"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_config_rejects_ambiguous_actions() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "repair:\n  rules:\n    - name: bad\n      set_frontmatter:\n        field: status\n        value: backlog\n      remove_frontmatter:\n        field: status\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("task.md"), "# Task\n").expect("task should write");

    let error = vault_error(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
    ]);

    assert!(error.contains("invalid config"));
    assert!(error.contains("repair rule bad declares both"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn registry_add_list_target_and_remove_are_isolated_by_xdg_config_home() {
    let config_home = temp_cache_dir();
    let config_home_string = config_home.to_string_lossy().to_string();
    let fixture = fixture_root();
    let fixture_string = fixture.to_string_lossy().to_string();
    let envs = [("XDG_CONFIG_HOME", config_home_string.as_str())];

    vault_success_env(
        &["registry", "add", "basic", fixture_string.as_str()],
        &envs,
    );

    let list = vault_success_env(&["registry", "list", "--format", "json"], &envs).0;
    let entries = serde_json::from_str::<Value>(&list).expect("registry list should be JSON");
    assert_eq!(entries.as_array().unwrap().len(), 1);
    assert_eq!(entries[0]["name"], "basic");
    assert_eq!(entries[0]["path"], fixture_string);

    let docs = vault_success_env(
        &["--vault", "basic", "docs", "list", "--format", "paths"],
        &envs,
    )
    .0;
    assert!(docs.lines().any(|line| line == "alpha.md"));

    vault_success_env(&["registry", "remove", "basic"], &envs);
    let list = vault_success_env(&["registry", "list", "--format", "json"], &envs).0;
    let entries = serde_json::from_str::<Value>(&list).expect("registry list should be JSON");
    assert!(entries.as_array().unwrap().is_empty());

    fs::remove_dir_all(config_home).ok();
}

#[test]
fn vault_targeting_rejects_vault_and_cwd_together() {
    let config_home = temp_cache_dir();
    let config_home_string = config_home.to_string_lossy().to_string();
    let fixture = fixture_root();
    let fixture_string = fixture.to_string_lossy().to_string();
    let envs = [("XDG_CONFIG_HOME", config_home_string.as_str())];

    vault_success_env(
        &["registry", "add", "basic", fixture_string.as_str()],
        &envs,
    );
    let error = vault_error_env(
        &[
            "--vault",
            "basic",
            "-C",
            fixture_string.as_str(),
            "docs",
            "list",
        ],
        &envs,
    );

    assert!(error.contains("--vault and -C/--cwd cannot be used together"));

    fs::remove_dir_all(config_home).ok();
}

#[test]
fn graph_documents_help_documents_frontmatter_filter() {
    let output = vault(&["docs", "list", "--help"]);
    assert!(output.contains("Frontmatter field:value filter"));
    assert!(output.contains("--path"));
    assert!(output.contains("--has"));
    assert!(output.contains("--missing"));
    assert!(output.contains("[possible values: json, jsonl, table, paths]"));
}

#[test]
fn docs_summary_help_documents_count_by() {
    let output = vault(&["docs", "summary", "--help"]);
    assert!(output.contains("--count-by"));
}

#[test]
fn docs_inspect_defaults_to_json() {
    let output = vault(&["docs", "inspect", "--help"]);
    assert!(output.contains("[default: json]"));
}

#[test]
fn omitted_list_format_defaults_to_json_when_stdout_is_piped() {
    let root = fixture_root();
    let output = vault(&["-C", root.to_str().unwrap(), "docs", "list"]);

    let documents = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert!(documents.as_array().unwrap().iter().any(|document| {
        document["path"] == "alpha.md" && document["frontmatter"]["title"] == "Alpha"
    }));
}

#[test]
fn docs_list_supports_table_and_paths_formats() {
    let root = fixture_root();
    let table = vault(&[
        "-C",
        root.to_str().unwrap(),
        "docs",
        "list",
        "--format",
        "table",
    ]);

    assert!(table.contains("path"));
    assert!(table.contains("title"));
    assert!(table.contains("diagnostics"));
    assert!(table.contains("alpha.md"));
    assert!(table.contains("Alpha"));

    let paths = vault(&[
        "-C",
        root.to_str().unwrap(),
        "docs",
        "list",
        "--format",
        "paths",
    ]);

    assert!(paths.lines().any(|line| line == "alpha.md"));
    assert!(paths.lines().any(|line| line == "folder/gamma.md"));
    assert!(!paths.contains('{'));
}

#[test]
fn search_filters_metadata_and_literal_text() {
    let root = fixture_root();
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "search",
        "--filter",
        "status:draft",
        "--text",
        "ambiguous link",
        "--format",
        "json",
    ]);

    let documents = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(documents.as_array().unwrap().len(), 1);
    assert_eq!(documents[0]["path"], "alpha.md");
}

#[test]
fn search_supports_path_presence_table_and_paths_formats() {
    let root = fixture_root();
    let paths = vault(&[
        "-C",
        root.to_str().unwrap(),
        "search",
        "--path",
        "folder/**",
        "--text",
        "Delta Heading",
        "--format",
        "paths",
    ]);

    assert_eq!(paths, "folder/delta.md\n");

    let table = vault(&[
        "-C",
        root.to_str().unwrap(),
        "search",
        "--has",
        "status",
        "--format",
        "table",
    ]);

    assert!(table.contains("path"));
    assert!(table.contains("alpha.md"));
    assert!(table.contains("Alpha"));
}

#[test]
fn graph_links_help_documents_obsidian_semantics() {
    let output = vault(&["links", "list", "--help"]);
    assert!(output.contains("frontmatter/property wikilinks"));
    assert!(output.contains("same-note heading/block references"));
    assert!(output.contains("Markdown image links to local files"));
    assert!(output.contains("source_context.area"));
}

#[test]
fn graph_unresolved_help_documents_reasons() {
    let output = vault(&["links", "unresolved", "--help"]);
    assert!(output.contains("target-missing"));
    assert!(output.contains("anchor-missing"));
    assert!(output.contains("block-ref-missing"));
    assert!(output.contains("ambiguous"));
}

#[test]
fn graph_backlinks_help_documents_file_targets() {
    let output = vault(&["links", "backlinks", "--help"]);
    assert!(output.contains("non-Markdown files"));
    assert!(output.contains("Stem matching only applies to Markdown documents"));
}

#[test]
fn validate_help_documents_read_only_contract() {
    let output = vault(&["validate", "--help"]);
    assert!(output.contains("Validate vault graph facts and configured frontmatter rules"));
    assert!(output.contains("Validate does not mutate files"));
    assert!(output.contains("--summary"));
    assert!(output.contains("--config"));
}

#[test]
fn validate_jsonl_reports_graph_findings_and_diagnostics() {
    let root = fixture_root();
    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let findings = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert!(findings
        .iter()
        .any(|finding| finding["code"] == "link-unresolved"
            && finding["path"] == "alpha.md"
            && finding["link"]["raw"] == "[[missing]]"));
    assert!(findings
        .iter()
        .any(|finding| finding["code"] == "link-ambiguous"
            && finding["path"] == "alpha.md"
            && finding["link"]["raw"] == "[[duplicate]]"
            && finding["link"]["unresolved_reason"] == "ambiguous"));
    assert!(findings
        .iter()
        .any(|finding| finding["code"] == "frontmatter-parse-failed"
            && finding["path"] == "broken-frontmatter.md"
            && finding["diagnostic"]["code"] == "frontmatter-parse-failed"));
}

#[test]
fn validate_reports_required_frontmatter_from_config() {
    let root = temp_cache_dir();
    fs::write(
        root.with_extension("yaml"),
        "validate:\n  required_frontmatter:\n    - title\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("has-title.md"),
        "---\ntitle: Present\n---\n# Present\n",
    )
    .expect("note should write");
    fs::write(root.join("missing-title.md"), "# Missing\n").expect("note should write");

    let config_path = root.with_extension("yaml");
    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["code"], "frontmatter-required-field-missing");
    assert_eq!(findings[0]["path"], "missing-title.md");
    assert_eq!(findings[0]["field"], "title");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_discovers_default_config_from_cwd() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join(".vault")).expect("config dir should be created");
    fs::write(
        root.join(".vault/config.yaml"),
        "validate:\n  required_frontmatter:\n    - title\n",
    )
    .expect("config should write");
    fs::write(root.join("missing-title.md"), "# Missing\n").expect("note should write");

    let output = vault(&["-C", root.to_str().unwrap(), "validate", "--format", "json"]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["field"], "title");

    fs::remove_dir_all(root).ok();
}

#[test]
fn validate_missing_default_config_uses_defaults() {
    let root = temp_cache_dir();
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("note.md"), "---\ntitle: Present\n---\n# Note\n")
        .expect("note should write");

    let output = vault(&["-C", root.to_str().unwrap(), "validate", "--format", "json"]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 0);

    fs::remove_dir_all(root).ok();
}

#[test]
fn commands_default_to_process_current_directory() {
    let root = temp_cache_dir();
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("note.md"), "# Note\n").expect("note should write");

    let output = vault_in_dir(&["docs", "list", "--format", "json"], &root);

    let documents = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(documents.as_array().unwrap().len(), 1);
    assert_eq!(documents[0]["path"], "note.md");

    fs::remove_dir_all(root).ok();
}

#[test]
fn graph_jsonl_tolerates_early_closing_stdout_consumers() {
    let root = temp_cache_dir();
    fs::create_dir_all(&root).expect("temp dir should be created");
    for index in 0..2_000 {
        fs::write(
            root.join(format!("note-{index}.md")),
            format!("---\ntitle: Note {index}\n---\n# Note {index}\n"),
        )
        .expect("note should write");
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args([
            "-C",
            root.to_str().unwrap(),
            "docs",
            "list",
            "--format",
            "jsonl",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("vault command should spawn");

    let stdout = child.stdout.take().expect("stdout should be piped");
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .expect("first JSONL row should be readable");
    assert!(serde_json::from_str::<Value>(&first_line).is_ok());
    drop(reader);

    let output = child.wait_with_output().expect("vault command should exit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "vault command failed after closed pipe\nstderr:\n{stderr}"
    );
    assert!(!stderr.contains("panicked"));
    assert!(!stderr.contains("Broken pipe"));

    fs::remove_dir_all(root).ok();
}

#[test]
fn validate_invalid_discovered_config_fails() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join(".vault")).expect("config dir should be created");
    fs::write(
        root.join(".vault/config.yaml"),
        "validate:\n  rules:\n    - name: bad\n      match:\n        path:\n          - 1\n          - 2\n",
    )
    .expect("config should write");
    fs::write(root.join("note.md"), "# Note\n").expect("note should write");

    let error = vault_error(&["-C", root.to_str().unwrap(), "validate"]);

    assert!(error.contains("invalid config"));
    assert!(error.contains(".vault/config.yaml"));
    assert!(error.contains("validate.rules[0].match.path"));

    fs::remove_dir_all(root).ok();
}

#[test]
fn validate_resolves_explicit_relative_config_against_cwd() {
    let root = temp_cache_dir();
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("vault.yaml"),
        "validate:\n  required_frontmatter:\n    - title\n",
    )
    .expect("config should write");
    fs::write(root.join("missing-title.md"), "# Missing\n").expect("note should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "validate",
        "--config",
        "vault.yaml",
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["field"], "title");

    fs::remove_dir_all(root).ok();
}

#[test]
fn validate_reports_scoped_required_frontmatter_from_config() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  required_frontmatter:\n    - title\n  rules:\n    - name: workspace-notes\n      match:\n        path: \"Workspaces/**/notes/*.md\"\n      required_frontmatter:\n        - type\n        - workspace\n    - name: workspace-tasks\n      match:\n        path: \"Workspaces/**/tasks/*.md\"\n      required_frontmatter:\n        - status\n",
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Workspaces/demo/notes")).expect("note dirs should be created");
    fs::create_dir_all(root.join("Workspaces/demo/tasks")).expect("task dirs should be created");
    fs::write(
        root.join("Workspaces/demo/notes/note.md"),
        "---\ntitle: Note\n---\n# Note\n",
    )
    .expect("note should write");
    fs::write(
        root.join("Workspaces/demo/tasks/task.md"),
        "---\ntitle: Task\n---\n# Task\n",
    )
    .expect("task should write");
    fs::write(root.join("loose.md"), "---\ntitle: Loose\n---\n# Loose\n")
        .expect("loose note should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 3);
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "Workspaces/demo/notes/note.md"
            && finding["field"] == "type"
            && finding["rule"] == "workspace-notes"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "Workspaces/demo/notes/note.md"
            && finding["field"] == "workspace"
            && finding["rule"] == "workspace-notes"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "Workspaces/demo/tasks/task.md"
            && finding["field"] == "status"
            && finding["rule"] == "workspace-tasks"
    }));
    assert!(!findings
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["path"] == "loose.md"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_summary_reports_grouped_counts() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  required_frontmatter:\n    - title\n  rules:\n    - name: note-kind\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: note\n      required_frontmatter:\n        - kind\n    - name: task-status\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: task\n      required_frontmatter:\n        - status\n",
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Notes")).expect("notes dir should be created");
    fs::create_dir_all(root.join("Tasks")).expect("tasks dir should be created");
    fs::write(root.join("missing-title.md"), "# Missing\n").expect("note should write");
    fs::write(
        root.join("Notes/note.md"),
        "---\ntitle: Note\ntype: note\n---\n# Note\n",
    )
    .expect("note should write");
    fs::write(
        root.join("Tasks/task.md"),
        "---\ntitle: Task\ntype: task\n---\n# Task\n",
    )
    .expect("task should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--summary",
        "--format",
        "json",
    ]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["findings"], 3);
    assert_eq!(summary["codes"]["frontmatter-required-field-missing"], 3);
    assert_eq!(summary["severities"]["warning"], 3);
    assert_eq!(summary["rules"]["note-kind"], 1);
    assert_eq!(summary["rules"]["task-status"], 1);
    assert_eq!(summary["fields"]["title"], 1);
    assert_eq!(summary["fields"]["kind"], 1);
    assert_eq!(summary["fields"]["status"], 1);
    assert_eq!(summary["path_prefixes"]["root"], 1);
    assert_eq!(summary["path_prefixes"]["Notes"], 1);
    assert_eq!(summary["path_prefixes"]["Tasks"], 1);

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn omitted_validate_summary_format_defaults_to_json_when_stdout_is_piped() {
    let root = fixture_root();
    let output = vault(&["-C", root.to_str().unwrap(), "validate", "--summary"]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["findings"], 7);
    assert_eq!(summary["codes"]["link-unresolved"], 5);
}

#[test]
fn validate_summary_supports_table_format() {
    let root = fixture_root();
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "validate",
        "--summary",
        "--format",
        "table",
    ]);

    assert!(output.contains("metric"));
    assert!(output.contains("findings"));
    assert!(output.contains("codes"));
    assert!(output.contains("link-unresolved"));
    assert!(output.contains("path_prefixes"));
}

#[test]
fn validate_filters_raw_findings_for_triage() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: typed-note\n      match:\n        path: \"**/*.md\"\n      field_types:\n        created: datetime\n    - name: note-description\n      match:\n        path: \"Notes/**\"\n      required_frontmatter:\n        - description\n",
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Notes")).expect("notes dir should be created");
    fs::write(
        root.join("Notes/bad-created.md"),
        "---\ncreated: 2026-05-17\n---\n# Bad\n",
    )
    .expect("note should write");
    fs::write(
        root.join("Notes/missing-description.md"),
        "---\ncreated: 2026-05-17T10:00\n---\n# Missing\n",
    )
    .expect("note should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--code",
        "frontmatter-invalid-type",
        "--field",
        "created",
        "--rule",
        "typed-note",
        "--path",
        "Notes/**",
        "--severity",
        "warning",
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["path"], "Notes/bad-created.md");
    assert_eq!(findings[0]["code"], "frontmatter-invalid-type");
    assert_eq!(findings[0]["field"], "created");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_filters_summary_before_grouping() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: typed-note\n      match:\n        path: \"**/*.md\"\n      field_types:\n        created: datetime\n        modified: datetime\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("one.md"),
        "---\ncreated: nope\nmodified: also-nope\n---\n# One\n",
    )
    .expect("note should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--code",
        "frontmatter-invalid-type",
        "--field",
        "created",
        "--summary",
        "--format",
        "json",
    ]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["findings"], 1);
    assert_eq!(summary["fields"]["created"], 1);
    assert_eq!(summary["invalid_types"]["created"]["datetime"], 1);
    assert!(summary["fields"].get("modified").is_none());

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_filters_link_findings_by_target_and_reason() {
    let root = fixture_root();
    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--code",
        "link-unresolved,link-ambiguous",
        "--target",
        "duplicate",
        "--reason",
        "ambiguous",
        "--format",
        "jsonl",
    ]);

    let findings = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["code"], "link-ambiguous");
    assert_eq!(findings[0]["link"]["target"], "duplicate");
    assert_eq!(findings[0]["link"]["unresolved_reason"], "ambiguous");
}

#[test]
fn validate_summary_reports_disallowed_value_counts() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status-values\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\n          - completed\n          - wont_do\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task-one.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");
    fs::write(
        root.join("task-two.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");
    fs::write(
        root.join("task-three.md"),
        "---\ntype: task\nstatus: later\n---\n# Task\n",
    )
    .expect("task should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--summary",
        "--format",
        "json",
    ]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["findings"], 3);
    assert_eq!(summary["fields"]["status"], 3);
    assert_eq!(summary["disallowed_values"]["status"]["someday"], 2);
    assert_eq!(summary["disallowed_values"]["status"]["later"], 1);

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_summary_reports_invalid_type_counts() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: typed-note\n      match:\n        path: \"**/*.md\"\n      field_types:\n        created: datetime\n        date: date\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("note.md"),
        "---\ncreated: not-a-date\ndate: also-not\n---\n# Note\n",
    )
    .expect("note should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--summary",
        "--format",
        "json",
    ]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["invalid_types"]["created"]["datetime"], 1);
    assert_eq!(summary["invalid_types"]["date"]["date"], 1);

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_reports_allowed_value_findings() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status-values\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: task\n      required_frontmatter:\n        - status\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\n          - completed\n          - wont_do\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: someday\n---\n# Task\n",
    )
    .expect("task should write");
    fs::write(
        root.join("valid-task.md"),
        "---\ntype: task\nstatus: completed\n---\n# Task\n",
    )
    .expect("task should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["code"], "frontmatter-disallowed-value");
    assert_eq!(findings[0]["path"], "task.md");
    assert_eq!(findings[0]["field"], "status");
    assert_eq!(findings[0]["rule"], "task-status-values");
    assert_eq!(findings[0]["actual_value"], "someday");
    assert_eq!(
        findings[0]["allowed_values"],
        serde_json::json!(["backlog", "in_progress", "completed", "wont_do"])
    );

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_allowed_values_do_not_coerce_types() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: numeric-priority\n      match:\n        path: \"**/*.md\"\n      allowed_values:\n        priority:\n          - \"1\"\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("task.md"), "---\npriority: 1\n---\n# Task\n").expect("task should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["code"], "frontmatter-disallowed-value");
    assert_eq!(findings[0]["actual_value"], 1);
    assert_eq!(findings[0]["allowed_values"], serde_json::json!(["1"]));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_rejects_malformed_allowed_values() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: bad-values\n      allowed_values:\n        status: completed\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("task.md"),
        "---\nstatus: completed\n---\n# Task\n",
    )
    .expect("task should write");

    let error = vault_error(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]);

    assert!(error.contains("invalid config"));
    assert!(error.contains("validate.rules[0].allowed_values.status"));
    assert!(error.contains("expected a sequence"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_rules_match_frontmatter_predicates() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: note-kind\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: note\n      required_frontmatter:\n        - kind\n    - name: task-status\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: task\n      required_frontmatter:\n        - status\n    - name: published-note\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: note\n          published: true\n      required_frontmatter:\n        - published_at\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("note.md"), "---\ntype: note\n---\n# Note\n").expect("note should write");
    fs::write(root.join("task.md"), "---\ntype: task\n---\n# Task\n").expect("task should write");
    fs::write(
        root.join("published-note.md"),
        "---\ntype: note\nkind: reference\npublished: true\n---\n# Published\n",
    )
    .expect("published note should write");
    fs::write(
        root.join("artifact.md"),
        "---\ntype: agent-artifact\n---\n# Artifact\n",
    )
    .expect("artifact should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 3);
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "note.md" && finding["field"] == "kind" && finding["rule"] == "note-kind"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "task.md"
            && finding["field"] == "status"
            && finding["rule"] == "task-status"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "published-note.md"
            && finding["field"] == "published_at"
            && finding["rule"] == "published-note"
    }));
    assert!(!findings
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["path"] == "artifact.md"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_rules_do_not_coerce_frontmatter_predicate_types() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: string-one\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          priority: \"1\"\n      required_frontmatter:\n        - status\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("number.md"), "---\npriority: 1\n---\n# Number\n")
        .expect("note should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 0);

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_rejects_unknown_match_keys() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: typo\n      match:\n        fronmatter:\n          type: note\n      required_frontmatter:\n        - kind\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("note.md"), "---\ntype: note\n---\n# Note\n").expect("note should write");

    let error = vault_error(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]);

    assert!(error.contains("invalid config"));
    assert!(error.contains("validate.rules[0].match"));
    assert!(error.contains("unknown field `fronmatter`"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_rejects_non_scalar_frontmatter_predicates() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: list\n      match:\n        frontmatter:\n          type:\n            - note\n      required_frontmatter:\n        - kind\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("note.md"), "---\ntype: note\n---\n# Note\n").expect("note should write");

    let error = vault_error(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
    ]);

    assert!(error.contains("invalid config"));
    assert!(error.contains("rule list"));
    assert!(error.contains("match.frontmatter.type must be a string, boolean, or number"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_ignore_skips_validation_without_graph_ignore() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  ignore:\n    - \"Templates/**\"\n  required_frontmatter:\n    - title\n",
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Templates")).expect("templates dir should be created");
    fs::write(root.join("Templates/template.md"), "# Template\n").expect("template should write");
    fs::write(root.join("active.md"), "# Active\n").expect("active note should write");

    let validate_output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let findings = serde_json::from_str::<Value>(&validate_output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["path"], "active.md");

    let graph_output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);
    let documents = serde_json::from_str::<Value>(&graph_output).expect("output should be JSON");
    assert!(documents
        .as_array()
        .unwrap()
        .iter()
        .any(|document| document["path"] == "Templates/template.md"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_rule_exclude_and_path_not_skip_matching_documents() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: active-title\n      match:\n        path: \"**/*.md\"\n        path_not: \"Archive/**\"\n      exclude:\n        path: \"Templates/**\"\n      required_frontmatter:\n        - title\n",
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Archive")).expect("archive dir should be created");
    fs::create_dir_all(root.join("Templates")).expect("templates dir should be created");
    fs::write(root.join("active.md"), "# Active\n").expect("active note should write");
    fs::write(root.join("Archive/old.md"), "# Old\n").expect("archive note should write");
    fs::write(root.join("Templates/template.md"), "# Template\n").expect("template should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 1);
    assert_eq!(findings[0]["path"], "active.md");
    assert_eq!(findings[0]["rule"], "active-title");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_reports_frontmatter_field_type_findings() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: note-types\n      match:\n        path: \"**/*.md\"\n      field_types:\n        created: datetime\n        date: date\n        aliases: list_of_strings\n        workspace: wikilink\n        technologies: wikilink_or_list\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("note.md"),
        "---\ncreated: not-a-date\ndate: 2026-99-99\naliases: alias\nworkspace: vault-cli\ntechnologies:\n  - \"[[Rust]]\"\n  - plain\n---\n# Note\n",
    )
    .expect("note should write");
    fs::write(
        root.join("valid.md"),
        "---\ncreated: 2026-05-17T10:01\ndate: 2026-05-17\naliases:\n  - Alias\nworkspace: \"[[vault-cli]]\"\ntechnologies:\n  - \"[[Rust]]\"\n---\n# Valid\n",
    )
    .expect("valid note should write");
    fs::write(
        root.join("valid-exported.md"),
        "---\ncreated: 2026-02-13T00:00:00.000Z\ndate: \"2026-03-20 00:00:00+00:00\"\naliases:\n  - Exported\nworkspace: \"[[vault-cli]]\"\ntechnologies: \"[[Rust]]\"\n---\n# Valid Exported\n",
    )
    .expect("valid exported note should write");
    fs::write(root.join("vault-cli.md"), "# vault-cli\n").expect("target note should write");
    fs::write(root.join("Rust.md"), "# Rust\n").expect("target note should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 5);
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "note.md"
            && finding["field"] == "created"
            && finding["expected_type"] == "datetime"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["field"] == "aliases" && finding["expected_type"] == "list_of_strings"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["field"] == "workspace" && finding["expected_type"] == "wikilink"
    }));
    assert!(!findings
        .as_array()
        .unwrap()
        .iter()
        .any(|finding| finding["path"] == "valid-exported.md"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_reports_forbidden_frontmatter_and_path_violations() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: agent-artifact-location\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: agent-artifact\n      forbidden_frontmatter:\n        - kind\n      allowed_paths:\n        - \"Workspaces/**/agent-artifacts/*.md\"\n",
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Workspaces/demo/agent-artifacts"))
        .expect("artifact dir should be created");
    fs::write(
        root.join("artifact.md"),
        "---\ntype: agent-artifact\nkind: note\n---\n# Artifact\n",
    )
    .expect("artifact should write");
    fs::write(
        root.join("Workspaces/demo/agent-artifacts/valid.md"),
        "---\ntype: agent-artifact\n---\n# Artifact\n",
    )
    .expect("valid artifact should write");

    let output = vault(&[
        "validate",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let findings = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(findings.as_array().unwrap().len(), 2);
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "artifact.md"
            && finding["code"] == "frontmatter-forbidden-field"
            && finding["field"] == "kind"
    }));
    assert!(findings.as_array().unwrap().iter().any(|finding| {
        finding["path"] == "artifact.md"
            && finding["code"] == "document-misrouted"
            && finding["allowed_paths"] == serde_json::json!(["Workspaces/**/agent-artifacts/*.md"])
    }));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn graph_documents_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 10);
    let alpha = documents
        .iter()
        .find(|document| document["path"] == "alpha.md")
        .unwrap();
    let beta = documents
        .iter()
        .find(|document| document["path"] == "beta.md")
        .unwrap();
    let broken = documents
        .iter()
        .find(|document| document["path"] == "broken-frontmatter.md")
        .unwrap();
    let frontmatter_source = documents
        .iter()
        .find(|document| document["path"] == "frontmatter-source.md")
        .unwrap();

    assert_eq!(alpha["frontmatter"]["title"], "Alpha");
    assert_eq!(alpha["headings"][0]["slug"], "alpha");
    assert_eq!(alpha["headings"][0]["source_span"]["line"], 8);
    assert_eq!(alpha["links"].as_array().unwrap().len(), 18);
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[beta|Beta Note]]" && link["label"] == "Beta Note"));
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[#Alpha]]"
            && link["resolved_path"] == "alpha.md"
            && link["status"] == "resolved"));
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[#^alpha-block]]"
            && link["resolved_path"] == "alpha.md"
            && link["status"] == "resolved"));
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[beta#^block-a]]" && link["block_ref"] == "block-a"));
    assert_eq!(alpha["block_ids"][0], "alpha-block");
    assert_eq!(beta["block_ids"][0], "block-a");
    assert_eq!(frontmatter_source["links"].as_array().unwrap().len(), 4);
    assert_eq!(
        frontmatter_source["links"][1]["source_context"],
        serde_json::json!({"area": "frontmatter", "property": "related"})
    );
    assert_eq!(frontmatter_source["links"][1]["source_span"]["line"], 3);
    assert_eq!(frontmatter_source["links"][1]["source_span"]["column"], 11);
    assert_eq!(
        frontmatter_source["links"][2]["source_context"],
        serde_json::json!({"area": "frontmatter", "property": "related_list"})
    );
    assert_eq!(frontmatter_source["links"][2]["source_span"]["line"], 5);
    assert_eq!(frontmatter_source["links"][2]["source_span"]["column"], 6);
    assert_eq!(
        broken["diagnostics"][0],
        serde_json::json!({
            "severity": "warning",
            "code": "frontmatter-parse-failed",
            "message": "frontmatter could not be parsed"
        })
    );
}

#[test]
fn graph_config_ignores_files_before_indexing() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join("__pycache__")).expect("temp dirs should be created");
    fs::write(root.join("kept.md"), "# Kept\n\n[[ignored]]\n").expect("kept note should write");
    fs::write(root.join("ignored.md"), "# Ignored\n").expect("ignored note should write");
    fs::write(root.join("__pycache__/ignored.pyc"), "compiled").expect("ignored file should write");
    fs::write(
        root.join("vault.yaml"),
        "files:\n  ignore:\n    - ignored.md\n    - __pycache__/**\n",
    )
    .expect("config should write");

    let documents = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--config",
        root.join("vault.yaml").to_str().unwrap(),
        "--format",
        "json",
    ]);
    let documents = serde_json::from_str::<Value>(&documents).expect("output should be JSON");
    assert_eq!(documents.as_array().unwrap().len(), 1);
    assert_eq!(documents[0]["path"], "kept.md");

    let files = vault(&[
        "files",
        "-C",
        root.to_str().unwrap(),
        "--config",
        root.join("vault.yaml").to_str().unwrap(),
        "--format",
        "json",
    ]);
    let files = serde_json::from_str::<Value>(&files).expect("output should be JSON");
    assert_eq!(files.as_array().unwrap().len(), 2);
    assert!(files
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "kept.md"));
    assert!(files
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "vault.yaml"));

    fs::remove_dir_all(root).ok();
}

#[test]
fn graph_ignore_config_key_reports_v0_16_rename() {
    let root = temp_cache_dir();
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("note.md"), "# Note\n").expect("note should write");
    fs::write(
        root.join("vault.yaml"),
        "graph:\n  ignore:\n    - ignored.md\n",
    )
    .expect("config should write");

    let error = vault_error(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--config",
        root.join("vault.yaml").to_str().unwrap(),
    ]);

    assert!(error.contains("'graph.ignore' was renamed to 'files.ignore' in v0.16"));

    fs::remove_dir_all(root).ok();
}

#[test]
fn graph_documents_filters_frontmatter_scalars() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--filter",
        "status:draft",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["path"], "alpha.md");
}

#[test]
fn graph_documents_filters_frontmatter_lists() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--filter",
        "aliases:First Note",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["path"], "alpha.md");
}

#[test]
fn graph_documents_filters_frontmatter_value_sets() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--filter",
        "title:Alpha,Frontmatter Source",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    let paths = documents
        .iter()
        .map(|document| document["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["alpha.md", "frontmatter-source.md"]);
}

#[test]
fn graph_documents_filters_by_path_glob() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--path",
        "other/*.md",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["path"], "other/duplicate.md");
}

#[test]
fn graph_documents_filters_by_frontmatter_existence() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--has",
        "aliases",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["path"], "alpha.md");

    let output = vault(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--missing",
        "aliases",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 9);
    assert!(!documents
        .iter()
        .any(|document| document["path"] == "alpha.md"));
}

#[test]
fn graph_documents_warns_when_filter_field_is_absent_everywhere() {
    let root = fixture_root();
    let (stdout, stderr) = vault_success(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--filter",
        "path:alpha.md",
        "--format",
        "json",
    ]);

    let documents = serde_json::from_str::<Value>(&stdout).expect("output should be JSON");
    assert_eq!(documents.as_array().unwrap().len(), 0);
    assert!(stderr.contains(
        "warning: --filter field 'path' is not a frontmatter key in any document; returning empty result"
    ));
}

#[test]
fn graph_documents_warns_when_missing_field_is_absent_everywhere() {
    let root = fixture_root();
    let (stdout, stderr) = vault_success(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--missing",
        "nosuch",
        "--format",
        "json",
    ]);

    let documents = serde_json::from_str::<Value>(&stdout).expect("output should be JSON");
    assert_eq!(documents.as_array().unwrap().len(), 0);
    assert!(stderr.contains(
        "warning: --missing field 'nosuch' is not a frontmatter key in any document; returning empty result"
    ));
}

#[test]
fn docs_summary_counts_frontmatter_values() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "summary",
        "-C",
        root.to_str().unwrap(),
        "--count-by",
        "title",
        "--format",
        "json",
    ]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["count_by"], "title");
    assert_eq!(summary["total"], 10);
    assert_eq!(summary["counts"]["Alpha"], 1);
    assert_eq!(summary["counts"]["Frontmatter Source"], 1);
}

#[test]
fn graph_documents_rejects_invalid_filters() {
    let root = fixture_root();
    let stderr = vault_error(&[
        "docs",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--filter",
        "status",
        "--format",
        "jsonl",
    ]);

    assert!(stderr.contains("invalid filter, expected field:value"));
}

#[test]
fn graph_files_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&["files", "-C", root.to_str().unwrap(), "--format", "jsonl"]);

    let files = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(files.len(), 12);
    assert!(files.iter().any(|file| file["path"] == "Assets/pic.png"
        && file["stem"] == "pic"
        && file["extension"] == "png"));
    assert!(files.iter().any(|file| file["path"] == "alpha.md"
        && file["stem"] == "alpha"
        && file["extension"] == "md"));
}

#[test]
fn graph_links_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "links",
        "list",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let links = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(links.len(), 23);
    assert_eq!(links[0]["kind"], "markdown");
    assert_eq!(links[0]["source_span"]["line"], 20);
    let encoded_with_ext = links
        .iter()
        .find(|link| link["raw"] == "Markdown%20Target.md")
        .unwrap();
    let encoded_without_ext = links
        .iter()
        .find(|link| link["raw"] == "Markdown%20Target")
        .unwrap();
    let encoded_anchor = links
        .iter()
        .find(|link| link["raw"] == "Markdown%20Target.md#Encoded%20Heading")
        .unwrap();
    let beta_alias = links
        .iter()
        .find(|link| link["raw"] == "[[beta|Beta Note]]")
        .unwrap();
    let block_ref = links
        .iter()
        .find(|link| link["raw"] == "[[beta#^block-a]]")
        .unwrap();
    let same_note_anchor = links
        .iter()
        .find(|link| link["raw"] == "[[#Alpha]]")
        .unwrap();
    let same_note_block = links
        .iter()
        .find(|link| link["raw"] == "[[#^alpha-block]]")
        .unwrap();
    let missing_same_note_anchor = links
        .iter()
        .find(|link| link["raw"] == "[[#Missing Same Heading]]")
        .unwrap();
    let missing_same_note_block = links
        .iter()
        .find(|link| link["raw"] == "[[#^missing-same-block]]")
        .unwrap();
    let missing_anchor = links
        .iter()
        .find(|link| link["raw"] == "[[beta#Missing Heading]]")
        .unwrap();
    let missing_block = links
        .iter()
        .find(|link| link["raw"] == "[[beta#^missing-block]]")
        .unwrap();
    let ambiguous = links
        .iter()
        .find(|link| link["raw"] == "[[duplicate]]")
        .unwrap();
    let path_qualified_case = links
        .iter()
        .find(|link| link["raw"] == "[[Other/Duplicate]]")
        .unwrap();
    let attachment_embed = links
        .iter()
        .find(|link| link["raw"] == "![[Assets/diagram.png]]")
        .unwrap();
    let markdown_image = links
        .iter()
        .find(|link| link["raw"] == "Assets/pic.png")
        .unwrap();
    let property_target = links
        .iter()
        .find(|link| link["raw"] == "[[Front Target]]")
        .unwrap();

    assert_eq!(encoded_with_ext["target"], "Markdown Target.md");
    assert_eq!(encoded_with_ext["resolved_path"], "Markdown Target.md");
    assert_eq!(encoded_without_ext["target"], "Markdown Target");
    assert_eq!(encoded_without_ext["resolved_path"], "Markdown Target.md");
    assert_eq!(encoded_anchor["anchor"], "Encoded Heading");
    assert_eq!(encoded_anchor["resolved_path"], "Markdown Target.md");
    assert_eq!(beta_alias["label"], "Beta Note");
    assert_eq!(beta_alias["resolved_path"], "beta.md");
    assert_eq!(block_ref["block_ref"], "block-a");
    assert_eq!(same_note_anchor["target"], "");
    assert_eq!(same_note_anchor["anchor"], "Alpha");
    assert_eq!(same_note_anchor["resolved_path"], "alpha.md");
    assert_eq!(same_note_anchor["status"], "resolved");
    assert_eq!(same_note_block["target"], "");
    assert_eq!(same_note_block["block_ref"], "alpha-block");
    assert_eq!(same_note_block["resolved_path"], "alpha.md");
    assert_eq!(same_note_block["status"], "resolved");
    assert_eq!(
        missing_same_note_anchor["unresolved_reason"],
        "anchor-missing"
    );
    assert_eq!(
        missing_same_note_block["unresolved_reason"],
        "block-ref-missing"
    );
    assert_eq!(missing_anchor["unresolved_reason"], "anchor-missing");
    assert_eq!(missing_block["unresolved_reason"], "block-ref-missing");
    assert_eq!(ambiguous["status"], "ambiguous");
    assert_eq!(ambiguous["unresolved_reason"], "ambiguous");
    assert_eq!(path_qualified_case["resolved_path"], "other/duplicate.md");
    assert_eq!(path_qualified_case["status"], "resolved");
    assert_eq!(attachment_embed["resolved_path"], "Assets/diagram.png");
    assert_eq!(attachment_embed["status"], "resolved");
    assert_eq!(markdown_image["kind"], "embed");
    assert_eq!(markdown_image["target"], "Assets/pic.png");
    assert_eq!(markdown_image["resolved_path"], "Assets/pic.png");
    assert_eq!(markdown_image["status"], "resolved");
    assert_eq!(property_target["raw"], "[[Front Target]]");
    assert_eq!(
        property_target["source_context"],
        serde_json::json!({"area": "frontmatter", "property": "related"})
    );
    assert_eq!(property_target["resolved_path"], "Front Target.md");
    assert!(links
        .iter()
        .any(|link| link["label"] == "Displayed in property"));
    assert!(!links.iter().any(|link| link["target"] == "inline-example"));
    assert!(!links.iter().any(|link| link["target"] == "fenced-example"));
}

#[test]
fn graph_unresolved_json_contract() {
    let root = fixture_root();
    let output = vault(&[
        "links",
        "unresolved",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let links = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(links.as_array().unwrap().len(), 6);
    assert_eq!(links[0]["raw"], "[[missing]]");
    assert_eq!(links[0]["source_span"]["line"], 10);
    assert_eq!(links[0]["unresolved_reason"], "target-missing");
    assert_eq!(links[0]["status"], "unresolved");
    assert_eq!(links[1]["raw"], "[[#Missing Same Heading]]");
    assert_eq!(links[1]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[2]["raw"], "[[#^missing-same-block]]");
    assert_eq!(links[2]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[3]["raw"], "[[beta#Missing Heading]]");
    assert_eq!(links[3]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[4]["raw"], "[[beta#^missing-block]]");
    assert_eq!(links[4]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[5]["raw"], "[[duplicate]]");
    assert_eq!(links[5]["source_span"]["line"], 26);
    assert_eq!(links[5]["status"], "ambiguous");
    assert_eq!(links[5]["unresolved_reason"], "ambiguous");
}

#[test]
fn graph_backlinks_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "links",
        "backlinks",
        "beta",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let links = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 5);
    assert_eq!(links[0]["raw"], "[[beta|Beta Note]]");
    assert_eq!(links[0]["label"], "Beta Note");
    assert_eq!(links[1]["raw"], "[[beta#^block-a]]");
    assert_eq!(links[1]["block_ref"], "block-a");
    assert_eq!(links[2]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[3]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[4]["source_context"]["area"], "frontmatter");
    assert_eq!(links[4]["source_context"]["property"], "related_list");
}

#[test]
fn graph_backlinks_accepts_exact_path() {
    let root = fixture_root();
    let output = vault(&[
        "links",
        "backlinks",
        "folder/delta.md",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let link = serde_json::from_str::<Value>(output.trim()).expect("output should be JSON");
    assert_eq!(link["kind"], "markdown");
    assert_eq!(link["anchor"], "Delta-Heading");
    assert_eq!(link["source_span"]["line"], 20);
}

#[test]
fn graph_backlinks_accepts_file_path() {
    let root = fixture_root();
    let output = vault(&[
        "links",
        "backlinks",
        "Assets/pic.png",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let link = serde_json::from_str::<Value>(output.trim()).expect("output should be JSON");
    assert_eq!(link["kind"], "embed");
    assert_eq!(link["raw"], "Assets/pic.png");
    assert_eq!(link["resolved_path"], "Assets/pic.png");
}

#[test]
fn graph_backlinks_accepts_case_insensitive_stem() {
    let root = fixture_root();
    let output = vault(&[
        "links",
        "backlinks",
        "BETA",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let links = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 5);
    assert_eq!(links[0]["resolved_path"], "beta.md");
}

#[test]
fn graph_inspect_accepts_case_insensitive_stem() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "inspect",
        "ALPHA",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["document"]["path"], "alpha.md");
}

#[test]
fn graph_backlinks_rejects_ambiguous_stem() {
    let root = fixture_root();
    let stderr = vault_error(&[
        "links",
        "backlinks",
        "duplicate",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    assert!(stderr.contains("ambiguous document stem: duplicate"));
    assert!(stderr.contains("duplicate.md"));
    assert!(stderr.contains("other/duplicate.md"));
}

#[test]
fn graph_inspect_json_contract() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "inspect",
        "alpha.md",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["document"]["path"], "alpha.md");
    assert_eq!(value["document"]["frontmatter"]["title"], "Alpha");
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 6);
    assert!(value["incoming_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[#Alpha]]" && link["status"] == "resolved"));
    assert!(value["incoming_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["source_path"] == "beta.md"));
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 18);
    assert_eq!(
        value["unresolved_outgoing_links"].as_array().unwrap().len(),
        6
    );
    assert_eq!(value["unresolved_outgoing_links"][0]["target"], "missing");
    assert_eq!(
        value["unresolved_outgoing_links"][1]["unresolved_reason"],
        "anchor-missing"
    );
    assert_eq!(
        value["unresolved_outgoing_links"][2]["unresolved_reason"],
        "block-ref-missing"
    );
    assert_eq!(
        value["unresolved_outgoing_links"][3]["unresolved_reason"],
        "anchor-missing"
    );
    assert_eq!(
        value["unresolved_outgoing_links"][4]["unresolved_reason"],
        "block-ref-missing"
    );
    assert_eq!(value["unresolved_outgoing_links"][5]["target"], "duplicate");
    assert_eq!(
        value["unresolved_outgoing_links"][5]["unresolved_reason"],
        "ambiguous"
    );
}

#[test]
fn graph_inspect_accepts_unique_stem() {
    let root = fixture_root();
    let output = vault(&[
        "docs",
        "inspect",
        "beta",
        "-C",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["document"]["path"], "beta.md");
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 5);
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["outgoing_links"][0]["resolved_path"],
        serde_json::json!("alpha.md")
    );
}
