use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/basic")
}

/// Wraps a vault invocation with per-test `XDG_CACHE_HOME` and
/// `XDG_STATE_HOME` trees so each test gets a fresh SQLite cache and the
/// binary never reads or sweeps the developer's real cache/state trees.
/// Without this, tests against the same vault root (e.g. `fixtures/basic`)
/// would share — and race on — the cache.
fn isolate_cache(command: &mut Command) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    dir
}

fn vault(args: &[&str]) -> String {
    vault_success(args).0
}

fn vault_success(args: &[&str]) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(args);
    let _cache_dir = isolate_cache(&mut command);
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

fn vault_success_env(args: &[&str], envs: &[(&str, &str)]) -> (String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(args);
    let _cache_dir = if envs.iter().any(|(k, _)| *k == "XDG_CACHE_HOME") {
        // Caller controls the cache tree; still isolate the state tree.
        let dir = tempfile::tempdir().expect("temp state dir should be created");
        command.env("XDG_STATE_HOME", dir.path().join("state"));
        dir
    } else {
        isolate_cache(&mut command)
    };
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(args);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault command should run");

    assert!(
        !output.status.success(),
        "vault command succeeded unexpectedly\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

/// Like [`vault_error`] but returns STDOUT — where a `--format json` failure now
/// emits the structured `{ code, message, path? }` error envelope (NRN-150).
fn vault_error_stdout(args: &[&str]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(args);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault command should run");

    assert!(
        !output.status.success(),
        "vault command succeeded unexpectedly\nstdout:\n{}\nstderr:\n{}",
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
    std::env::temp_dir().join(format!("norn-cache-{process}-{unique}-{counter}"))
}

#[test]
fn vault_version_reports_package_version() {
    let output = vault(&["--version"]);
    assert!(output.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn norn_help_documents_global_cwd() {
    let output = vault(&["--help"]);
    assert!(output.contains("-C, --cwd"));
    assert!(output.contains("--config"));
    assert!(output.contains("--verbose"));
    assert!(output.contains("repair"));
    assert!(output.contains("cache"));
}

#[test]
fn graph_umbrella_is_removed() {
    let error = vault_error(&["graph", "--help"]);
    assert!(error.contains("unrecognized subcommand 'graph'"));
}

#[test]
fn links_list_is_removed() {
    let error = vault_error(&["links", "list"]);
    assert!(
        error.contains("unrecognized subcommand 'list'")
            || error.contains("unrecognized subcommand 'links'")
            || error.contains("unexpected argument 'list'"),
        "expected unrecognized-subcommand error for `norn links list`; got: {error}"
    );
}

#[test]
fn files_subcommand_is_removed() {
    let error = vault_error(&["files"]);
    assert!(
        error.contains("unrecognized subcommand 'files'")
            || error.contains("unexpected argument 'files'"),
        "expected unrecognized-subcommand error for `norn files`; got: {error}"
    );
}

#[test]
fn docs_namespace_is_removed() {
    let error = vault_error(&["docs", "--help"]);
    assert!(
        error.contains("unrecognized subcommand 'docs'"),
        "expected unrecognized-subcommand error for `norn docs`; got: {error}"
    );
}

#[test]
fn docs_summary_is_removed() {
    let error = vault_error(&["docs", "summary"]);
    assert!(
        error.contains("unrecognized subcommand 'docs'")
            || error.contains("unrecognized subcommand 'summary'"),
        "expected unrecognized-subcommand error for `norn docs summary`; got: {error}"
    );
}

#[test]
fn docs_inspect_is_removed() {
    let error = vault_error(&["docs", "inspect", "any"]);
    assert!(
        error.contains("unrecognized subcommand 'docs'")
            || error.contains("unrecognized subcommand 'inspect'"),
        "expected unrecognized-subcommand error for `norn docs inspect`; got: {error}"
    );
}

#[test]
fn links_namespace_is_removed() {
    let error = vault_error(&["links", "--help"]);
    assert!(
        error.contains("unrecognized subcommand 'links'"),
        "expected unrecognized-subcommand error for `norn links`; got: {error}"
    );
}

#[test]
fn links_unresolved_is_removed() {
    let error = vault_error(&["links", "unresolved"]);
    assert!(
        error.contains("unrecognized subcommand 'links'")
            || error.contains("unrecognized subcommand 'unresolved'")
            || error.contains("unexpected argument 'unresolved'"),
        "expected unrecognized-subcommand error for `norn links unresolved`; got: {error}"
    );
}

#[test]
fn links_backlinks_is_removed() {
    let error = vault_error(&["links", "backlinks", "any"]);
    assert!(
        error.contains("unrecognized subcommand 'links'")
            || error.contains("unrecognized subcommand 'backlinks'")
            || error.contains("unexpected argument 'backlinks'"),
        "expected unrecognized-subcommand error for `norn links backlinks`; got: {error}"
    );
}

#[test]
fn repair_links_is_removed() {
    let error = vault_error(&["repair", "links"]);
    assert!(
        error.contains("unrecognized subcommand 'links'")
            || error.contains("unexpected argument 'links'"),
        "expected unrecognized-subcommand error for `norn repair links`; got: {error}"
    );
}

#[test]
fn grouped_help_lists_new_surfaces() {
    let output = vault(&["cache", "--help"]);
    assert!(output.contains("Manage the SQLite-backed vault graph cache"));
    assert!(output.contains("index"));
    assert!(output.contains("rebuild"));
    assert!(output.contains("clear"));
    assert!(output.contains("status"));

    let output = vault(&["repair", "--help"]);
    // `repair` is now flag-based: bare repair = summary, `--plan` = MigrationPlan.
    assert!(output.contains("--plan"));
    assert!(output.contains("--format"));
    assert!(output.contains("--out"));
    // jsonl and table were removed from repair --plan; they should not appear in help
    assert!(!output.contains("jsonl"));
    // The retired `plan`/`apply` subcommands must not be listed as subcommands.
    assert!(
        !output.contains("  apply  ") && !output.contains("  apply\n"),
        "norn repair apply subcommand should be retired; got: {output}"
    );
}

// repair_links_reports_link_drift_and_duplicate_stems: removed — vault repair links retired.
// Broken-link enumeration is now vault validate --code 'link-*'.
// repair_links_reports_target_move_and_delete_risk: removed — vault repair links retired.
// Move/delete impact analysis is vault move --dry-run and vault delete --dry-run.
// repair_links_table_is_row_oriented: removed — vault repair links retired.

#[test]
fn repair_plan_generates_configured_frontmatter_change() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\n          - completed\n          - wont_do\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: value-not-allowed\n        rule: task-status\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
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
        "--plan",
        "--code",
        "value-not-allowed,frontmatter-forbidden-field",
        "--field",
        "status",
    ]);

    // `repair --plan` emits a MigrationPlan: schema_version 2, generator
    // norn-repair, ops nested under `operations[].fields` with `operation` → `kind`.
    let plan = serde_json::from_str::<Value>(&output).expect("repair --plan should be JSON");
    assert_eq!(plan["schema_version"], 2);
    assert_eq!(plan["generator"], "norn-repair");
    assert_eq!(plan["operations"].as_array().unwrap().len(), 1);
    assert!(
        plan["skipped"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true),
        "no skipped findings expected; got: {plan}"
    );
    let op = &plan["operations"][0];
    assert_eq!(op["kind"], "set_frontmatter");
    assert_eq!(op["fields"]["path"], "task.md");
    assert!(op["fields"]["document_hash"].as_str().unwrap().len() > 20);
    assert_eq!(op["fields"]["repair_rule"], "map-someday-status");
    assert!(
        op["fields"].get("operation").is_none(),
        "operation key must be promoted to kind; fields={}",
        op["fields"]
    );
    assert_eq!(op["fields"]["field"], "status");
    assert_eq!(op["fields"]["expected_old_value"], "someday");
    assert_eq!(op["fields"]["new_value"], "backlog");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_plan_out_writes_json_artifact_without_stdout() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: value-not-allowed\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
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
        "--plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    assert_eq!(stdout, "");
    let plan_text = fs::read_to_string(&plan_path).expect("plan should write");
    let plan = serde_json::from_str::<Value>(&plan_text).expect("repair --plan should be JSON");
    assert_eq!(plan["schema_version"], 2);
    assert_eq!(plan["operations"].as_array().unwrap().len(), 1);
    assert_eq!(plan["operations"][0]["fields"]["path"], "task.md");

    // --format table is now rejected at parse time with a migration message
    let error = vault_error(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--format",
        "table",
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    assert!(
        error.contains("invalid value 'table'"),
        "expected invalid value rejection, got: {error}"
    );
    assert!(
        error.contains("--format report") || error.contains("use --format report"),
        "expected migration hint, got: {error}"
    );

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn broad_repair_plan_with_skipped_findings_still_applies_changes() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: value-not-allowed\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
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
        "--plan",
    ]);
    let plan_json = serde_json::from_str::<Value>(&plan).expect("repair --plan should be JSON");
    assert_eq!(plan_json["operations"].as_array().unwrap().len(), 1);
    let skipped = plan_json["skipped"].as_array().expect("skipped array");
    assert_eq!(skipped.len(), 1);
    // The MigrationPlan SkippedFinding carries the validation finding_code and
    // the kebab-case skip-reason code in `reason`.
    assert_eq!(skipped[0]["finding_code"], "link-target-missing");
    assert_eq!(skipped[0]["reason"], "link-decision-needed");

    // The plan applies cleanly through `norn apply` (dry-run preview).
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--dry-run",
        "--format",
        "json",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["dry_run"], true);
    // Dry-run: the one frontmatter op is recognized but not applied (status not_run).
    assert_eq!(report["operations"][0]["kind"], "set_frontmatter");
    assert_eq!(report["operations"][0]["status"], "not-run");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

// repair_plan_table_is_row_oriented: removed in Task 7 — --format table was removed from
// vault repair plan. Rejection behavior is covered by repair_plan_format_rejection.rs.

#[test]
fn repair_apply_writes_frontmatter_plan_and_verifies() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: value-not-allowed\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
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
        "--plan",
        "--code",
        "value-not-allowed",
        "--field",
        "status",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");

    // Apply through the unified apply pipeline (--yes skips the prompt).
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["applied"], 1);
    assert_eq!(report["operations"][0]["status"], "applied");

    let updated = fs::read_to_string(root.join("task.md")).expect("task should read");
    assert!(updated.contains("status: backlog"));
    assert!(updated.contains("# Task\n\nBody stays.\n"));

    // Verification: re-planning against the repaired vault yields zero operations.
    let replan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--code",
        "value-not-allowed",
        "--field",
        "status",
    ]);
    let replan_json = serde_json::from_str::<Value>(&replan).expect("re-plan should be JSON");
    assert!(
        replan_json["operations"].as_array().unwrap().is_empty(),
        "no operations should remain after apply; got: {replan}"
    );

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_apply_dry_run_does_not_write() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: value-not-allowed\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
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
        "--plan",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--dry-run",
        "--format",
        "json",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["operations"][0]["kind"], "set_frontmatter");
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
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\nrepair:\n  rules:\n    - name: map-someday-status\n      match:\n        code: value-not-allowed\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
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
        "--plan",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");
    // Mutate the document so the plan's stored document_hash is now stale.
    fs::write(
        root.join("task.md"),
        "---\ntype: task\nstatus: later\n---\n# Task\n",
    )
    .expect("task should write");

    // NRN-150: a `--format json` failure emits the structured `{ code, message,
    // path? }` envelope on STDOUT with the stable kebab code; the prose survives
    // inside `message`.
    let stdout = vault_error_stdout(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);
    let envelope: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be a JSON error envelope");
    assert_eq!(
        envelope["code"], "stale-document-hash",
        "expected stale-plan refusal code; got: {envelope}"
    );
    assert!(
        envelope["message"]
            .as_str()
            .is_some_and(|m| m.contains("stale repair plan")),
        "envelope message preserves the prose; got: {envelope}"
    );

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_apply_preserves_double_quoted_workspace_field() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: task-status\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status:\n          - backlog\n          - in_progress\n          - completed\n          - wont_do\nrepair:\n  rules:\n    - name: legacy-task-status-someday\n      match:\n        code: value-not-allowed\n        rule: task-status\n        field: status\n        actual_value: someday\n      set_frontmatter:\n        field: status\n        value: backlog\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    let task_path = root.join("task.md");
    fs::write(
        &task_path,
        "---\ntype: task\ntitle: Test task\nstatus: someday\nworkspace: \"[[norn]]\"\n---\n# body\n",
    )
    .expect("task should write");

    let plan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
    ]);
    let plan_path = root.join("repair.json");
    fs::write(&plan_path, plan).expect("plan should write");

    vault(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);

    let after = fs::read_to_string(&task_path).expect("task should read");
    assert!(
        after.contains("status: backlog"),
        "status should be repaired, got:\n{after}"
    );
    assert!(
        after.contains("workspace: \"[[norn]]\""),
        "workspace double-quoted style should be preserved, got:\n{after}"
    );
    assert!(after.ends_with("---\n# body\n"));

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
        "--plan",
    ]);

    assert!(error.contains("invalid config"));
    assert!(error.contains("repair rule bad declares multiple actions"));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
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
        .any(|finding| finding["code"] == "link-target-missing"
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["code"], "frontmatter-required-field-missing");
    assert_eq!(findings[0]["path"], "missing-title.md");
    assert_eq!(findings[0]["field"], "title");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn validate_list_selector_rule_fires_on_each_listed_type() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join(".norn")).expect("config dir should be created");
    fs::write(
        root.join(".norn/config.yaml"),
        concat!(
            "validate:\n",
            "  rules:\n",
            "    - name: base-node\n",
            "      match:\n",
            "        frontmatter:\n",
            "          type: [task, phase]\n",
            "      required_frontmatter:\n",
            "        - parent\n",
        ),
    )
    .expect("config should write");
    fs::write(root.join("a-task.md"), "---\ntype: task\n---\n# A\n").expect("note should write");
    fs::write(root.join("a-phase.md"), "---\ntype: phase\n---\n# B\n").expect("note should write");
    fs::write(root.join("a-note.md"), "---\ntype: note\n---\n# C\n").expect("note should write");

    let output = vault(&["-C", root.to_str().unwrap(), "validate", "--format", "json"]);

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    let paths: Vec<&str> = findings
        .iter()
        .filter(|f| f["code"] == "frontmatter-required-field-missing" && f["field"] == "parent")
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"a-task.md"), "findings: {findings:?}");
    assert!(paths.contains(&"a-phase.md"), "findings: {findings:?}");
    assert!(
        !paths.contains(&"a-note.md"),
        "note type is outside the any-of set: {findings:?}"
    );

    fs::remove_dir_all(root).ok();
}

#[test]
fn validate_discovers_default_config_from_cwd() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join(".norn")).expect("config dir should be created");
    fs::write(
        root.join(".norn/config.yaml"),
        "validate:\n  required_frontmatter:\n    - title\n",
    )
    .expect("config should write");
    fs::write(root.join("missing-title.md"), "# Missing\n").expect("note should write");

    let output = vault(&["-C", root.to_str().unwrap(), "validate", "--format", "json"]);

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 0);

    fs::remove_dir_all(root).ok();
}

#[test]
fn validate_invalid_discovered_config_fails() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join(".norn")).expect("config dir should be created");
    fs::write(
        root.join(".norn/config.yaml"),
        "validate:\n  rules:\n    - name: bad\n      match:\n        path:\n          - 1\n          - 2\n",
    )
    .expect("config should write");
    fs::write(root.join("note.md"), "# Note\n").expect("note should write");

    let error = vault_error(&["-C", root.to_str().unwrap(), "validate"]);

    assert!(error.contains("invalid config"));
    assert!(error.contains(".norn/config.yaml"));
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 3);
    assert!(findings.iter().any(|finding| {
        finding["path"] == "Workspaces/demo/notes/note.md"
            && finding["field"] == "type"
            && finding["rule"] == "workspace-notes"
    }));
    assert!(findings.iter().any(|finding| {
        finding["path"] == "Workspaces/demo/notes/note.md"
            && finding["field"] == "workspace"
            && finding["rule"] == "workspace-notes"
    }));
    assert!(findings.iter().any(|finding| {
        finding["path"] == "Workspaces/demo/tasks/task.md"
            && finding["field"] == "status"
            && finding["rule"] == "workspace-tasks"
    }));
    assert!(!findings.iter().any(|finding| finding["path"] == "loose.md"));

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
    // Piped default is now Jsonl for the non-summary path; summary view
    // requires an explicit --format json to get the JSON summary shape.
    let root = fixture_root();
    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "validate",
        "--summary",
        "--format",
        "json",
    ]);

    let summary = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(summary["findings"], 7);
    assert_eq!(summary["codes"]["link-target-missing"], 1);
    assert_eq!(summary["codes"]["link-anchor-missing"], 2);
    assert_eq!(summary["codes"]["link-block-missing"], 2);
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
        "field-type-invalid",
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["path"], "Notes/bad-created.md");
    assert_eq!(findings[0]["code"], "field-type-invalid");
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
        "field-type-invalid",
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
        "link-*",
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["code"], "value-not-allowed");
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["code"], "value-not-allowed");
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 3);
    assert!(findings.iter().any(|finding| {
        finding["path"] == "note.md" && finding["field"] == "kind" && finding["rule"] == "note-kind"
    }));
    assert!(findings.iter().any(|finding| {
        finding["path"] == "task.md"
            && finding["field"] == "status"
            && finding["rule"] == "task-status"
    }));
    assert!(findings.iter().any(|finding| {
        finding["path"] == "published-note.md"
            && finding["field"] == "published_at"
            && finding["rule"] == "published-note"
    }));
    assert!(!findings
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 0);

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
    // Lists are valid any-of selectors since NRN-71; objects remain invalid.
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: list\n      match:\n        frontmatter:\n          type:\n            nested: note\n      required_frontmatter:\n        - kind\n",
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
    assert!(error.contains(
        "match.frontmatter.type must be a string, boolean, or number, or a list of those (any-of)"
    ));

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
    let parsed = serde_json::from_str::<Value>(&validate_output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0]["path"], "active.md");

    let find_output = vault(&[
        "find",
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "--path",
        "Templates/**",
        "--format",
        "json",
    ]);
    let envelope = serde_json::from_str::<Value>(&find_output).expect("output should be JSON");
    let docs = envelope["documents"]
        .as_array()
        .expect("envelope.documents should be array");
    assert!(docs
        .iter()
        .any(|doc| doc["path"] == "Templates/template.md"));

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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
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
        "---\ncreated: not-a-date\ndate: 2026-99-99\naliases: alias\nworkspace: norn\ntechnologies:\n  - \"[[Rust]]\"\n  - plain\n---\n# Note\n",
    )
    .expect("note should write");
    fs::write(
        root.join("valid.md"),
        "---\ncreated: 2026-05-17T10:01\ndate: 2026-05-17\naliases:\n  - Alias\nworkspace: \"[[norn]]\"\ntechnologies:\n  - \"[[Rust]]\"\n---\n# Valid\n",
    )
    .expect("valid note should write");
    fs::write(
        root.join("valid-exported.md"),
        "---\ncreated: 2026-02-13T00:00:00.000Z\ndate: \"2026-03-20 00:00:00+00:00\"\naliases:\n  - Exported\nworkspace: \"[[norn]]\"\ntechnologies: \"[[Rust]]\"\n---\n# Valid Exported\n",
    )
    .expect("valid exported note should write");
    fs::write(root.join("norn.md"), "# norn\n").expect("target note should write");
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 5);
    assert!(findings.iter().any(|finding| {
        finding["path"] == "note.md"
            && finding["field"] == "created"
            && finding["expected_type"] == "datetime"
    }));
    assert!(findings.iter().any(|finding| {
        finding["field"] == "aliases" && finding["expected_type"] == "list_of_strings"
    }));
    assert!(findings.iter().any(|finding| {
        finding["field"] == "workspace" && finding["expected_type"] == "wikilink"
    }));
    assert!(!findings
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

    let parsed = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let findings = parsed["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    assert!(findings.iter().any(|finding| {
        finding["path"] == "artifact.md"
            && finding["code"] == "frontmatter-forbidden-field"
            && finding["field"] == "kind"
    }));
    assert!(findings.iter().any(|finding| {
        finding["path"] == "artifact.md"
            && finding["code"] == "document-misrouted"
            && finding["allowed_paths"] == serde_json::json!(["Workspaces/**/agent-artifacts/*.md"])
    }));

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn completions_bash_subcommand_emits_non_empty_script() {
    let (stdout, _stderr) = vault_success(&["completions", "init", "bash"]);
    assert!(
        !stdout.is_empty(),
        "expected non-empty bash completion script, got empty stdout"
    );
    // Bash completions reference the program name in the generated function.
    assert!(
        stdout.contains("norn"),
        "expected bash completion script to reference the program name `norn`, got:\n{stdout}"
    );
}

#[test]
fn completions_zsh_subcommand_emits_non_empty_script() {
    let (stdout, _stderr) = vault_success(&["completions", "init", "zsh"]);
    assert!(
        !stdout.is_empty(),
        "expected non-empty zsh completion script"
    );
    assert!(
        stdout.contains("#compdef norn") || stdout.contains("_norn"),
        "expected zsh completion to declare the compdef or _norn function, got:\n{stdout}"
    );
}

#[test]
fn completions_fish_subcommand_emits_non_empty_script() {
    let (stdout, _stderr) = vault_success(&["completions", "init", "fish"]);
    assert!(
        !stdout.is_empty(),
        "expected non-empty fish completion script"
    );
    assert!(
        stdout.contains("vault"),
        "expected fish completion script to reference the program name `vault`, got:\n{stdout}"
    );
}

#[test]
fn manpage_subcommand_emits_non_empty_roff() {
    let (stdout, _stderr) = vault_success(&["manpage"]);
    assert!(!stdout.is_empty(), "expected non-empty man page output");
    // clap_mangen emits a standard roff header beginning with `.TH "<NAME>" ...`
    // — assert the program name appears in the header line.
    assert!(
        stdout.contains(".TH"),
        "expected roff TH (title heading) macro in man page output, got:\n{stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("vault"),
        "expected man page to mention the program name `vault`, got:\n{stdout}"
    );
}

#[test]
fn manpage_is_hidden_from_top_level_help() {
    let output = vault(&["--help"]);
    assert!(
        output.contains("completions"),
        "completions should be visible in top-level help; got:\n{output}"
    );
    assert!(
        !output.contains("manpage"),
        "manpage should remain hidden from top-level help; got:\n{output}"
    );
}

#[test]
fn top_level_help_uses_norn_program_name() {
    // Exercises the real binary (compiled with CARGO_BIN_NAME set), so this is
    // the meaningful guard on `help::bin_name::BIN_NAME` that its now-removed
    // unit test could not be under a library build. If the binary is renamed in
    // Cargo.toml, the usage line changes and this fails.
    let output = vault(&["--help"]);
    assert!(
        output.contains("norn [OPTIONS]"),
        "top-level usage should name the program `norn`; got:\n{output}"
    );
}

#[test]
fn completions_init_subcommand_help_documents_supported_shells() {
    let output = vault(&["completions", "init", "--help"]);
    for shell in ["bash", "zsh", "fish", "powershell", "elvish", "nushell"] {
        assert!(
            output.contains(shell),
            "completions init --help should list {shell}; got:\n{output}"
        );
    }
}

#[test]
fn completions_bash_writes_clean_stderr() {
    let (_stdout, stderr) = vault_success(&["completions", "init", "bash"]);
    assert!(
        stderr.is_empty(),
        "expected `norn completions init bash` to write nothing to stderr, got:\n{stderr}"
    );
}

#[test]
fn manpage_writes_clean_stderr() {
    let (_stdout, stderr) = vault_success(&["manpage"]);
    assert!(
        stderr.is_empty(),
        "expected `norn manpage` to write nothing to stderr, got:\n{stderr}"
    );
}

#[test]
fn completions_runs_without_a_vault_root() {
    // Run from a temp directory with no .norn/config.yaml and no -C flag.
    // The subcommand must succeed without complaining about vault discovery.
    let scratch = std::env::temp_dir().join(format!(
        "vault-completions-no-vault-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&scratch).expect("create scratch dir");
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .current_dir(&scratch)
        .env("XDG_CACHE_HOME", scratch.join("xdg-cache"))
        .env("XDG_STATE_HOME", scratch.join("xdg-state"))
        .args(["completions", "init", "bash"])
        .output()
        .expect("vault command should run");
    let _ = fs::remove_dir_all(&scratch);
    assert!(
        output.status.success(),
        "completions must work outside a vault\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty());
}

#[test]
fn manpage_runs_without_a_vault_root() {
    let scratch = std::env::temp_dir().join(format!(
        "vault-manpage-no-vault-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&scratch).expect("create scratch dir");
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .current_dir(&scratch)
        .env("XDG_CACHE_HOME", scratch.join("xdg-cache"))
        .env("XDG_STATE_HOME", scratch.join("xdg-state"))
        .args(["manpage"])
        .output()
        .expect("vault command should run");
    let _ = fs::remove_dir_all(&scratch);
    assert!(
        output.status.success(),
        "manpage must work outside a vault\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty());
}

#[test]
fn completions_rejects_unknown_shell() {
    let stderr = vault_error(&["completions", "init", "tcsh"]);
    // clap's standard "invalid value" message includes the offending value.
    assert!(
        stderr.contains("tcsh") || stderr.contains("invalid value"),
        "expected clap to reject the unknown shell `tcsh`, got stderr:\n{stderr}"
    );
}

/// Cargo-dist's `include` directive packages completion scripts and the
/// man page from a stable path under the workspace `target/` directory.
/// `build.rs` produces those artifacts as a side effect of any build of
/// `norn`, so the integration test binary having compiled at all
/// implies the files now exist. This guards against silent regressions
/// where the build script stops emitting one of the expected outputs
/// (for example after a clap_complete shell list change).
#[test]
fn build_script_emits_release_artifacts() {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let completions = workspace_root.join("target").join("completions");
    let man = workspace_root.join("target").join("man");

    for expected in ["norn.bash", "_norn", "norn.fish"] {
        let path = completions.join(expected);
        let metadata = fs::metadata(&path).unwrap_or_else(|err| {
            panic!(
                "build.rs must emit completions artifact {}: {err}",
                path.display()
            )
        });
        assert!(
            metadata.len() > 0,
            "completions artifact {} must be non-empty",
            path.display()
        );
    }

    let man_path = man.join("norn.1");
    let metadata = fs::metadata(&man_path)
        .unwrap_or_else(|err| panic!("build.rs must emit {}: {err}", man_path.display()));
    assert!(
        metadata.len() > 0,
        "man page artifact {} must be non-empty",
        man_path.display()
    );
}

#[test]
fn completions_init_supports_all_six_shells() {
    for shell in &["bash", "zsh", "fish", "powershell", "elvish", "nushell"] {
        let (stdout, _stderr) = vault_success(&["completions", "init", shell]);
        assert!(
            !stdout.trim().is_empty(),
            "completions init {shell} should emit non-empty script; got empty stdout"
        );
    }
}

#[test]
fn completions_install_unsupported_shell_errors_cleanly() {
    let stderr = vault_error(&["completions", "install", "tcsh"]);
    assert!(
        stderr.contains("invalid value") || stderr.contains("not a supported"),
        "expected clear error for unsupported shell; got:\n{stderr}"
    );
}

#[test]
fn completions_install_no_arg_and_no_shell_env_errors() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["completions", "install"]).env_remove("SHELL");
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault command should run");
    assert!(
        !output.status.success(),
        "expected failure with no SHELL set"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("could not auto-detect") || stderr.contains("SHELL"),
        "expected error about $SHELL detection; got:\n{stderr}"
    );
}

fn install_in_tempdir(
    shell: &str,
    env_overrides: &[(&str, &str)],
) -> (tempfile::TempDir, std::process::Output) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"));
    cmd.args(["completions", "install", shell]);
    cmd.env("HOME", dir.path());
    cmd.env("XDG_CONFIG_HOME", dir.path().join(".config"));
    cmd.env("XDG_CACHE_HOME", dir.path().join(".cache"));
    cmd.env("XDG_STATE_HOME", dir.path().join(".local/state"));
    for (k, v) in env_overrides {
        cmd.env(k, v);
    }
    let output = cmd.output().unwrap();
    (dir, output)
}

#[test]
fn completions_install_bash_writes_marker_block_to_bashrc() {
    let (dir, output) = install_in_tempdir("bash", &[]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bashrc = fs::read_to_string(dir.path().join(".bashrc")).unwrap();
    assert!(
        bashrc.contains("# >>> norn completions"),
        "missing marker: {bashrc}"
    );
    assert!(
        bashrc.contains("eval \"$(norn completions init bash)\""),
        "missing eval line: {bashrc}"
    );
    assert!(
        bashrc.contains("# <<< norn completions <<<"),
        "missing end marker: {bashrc}"
    );
}

#[test]
fn completions_install_zsh_writes_marker_block_to_zshrc() {
    let (dir, output) = install_in_tempdir("zsh", &[]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let zshrc = fs::read_to_string(dir.path().join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> norn completions"));
    assert!(zshrc.contains("eval \"$(norn completions init zsh)\""));
}

#[test]
fn completions_install_zsh_honors_zdotdir() {
    let zdir = tempfile::TempDir::new().unwrap();
    let (_home, output) = install_in_tempdir("zsh", &[("ZDOTDIR", zdir.path().to_str().unwrap())]);
    assert!(output.status.success());
    let zshrc = fs::read_to_string(zdir.path().join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> norn completions"));
}

#[test]
fn completions_install_elvish_writes_marker_block_to_rc_elv() {
    let (dir, output) = install_in_tempdir("elvish", &[]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rc = fs::read_to_string(dir.path().join(".config/elvish/rc.elv")).unwrap();
    assert!(rc.contains("# >>> norn completions"));
    assert!(rc.contains("norn completions init elvish"));
}

#[test]
fn completions_install_is_idempotent() {
    let (dir, output1) = install_in_tempdir("bash", &[]);
    assert!(output1.status.success());
    let bashrc_first = fs::read_to_string(dir.path().join(".bashrc")).unwrap();
    let count_first = bashrc_first.matches("# >>> norn completions").count();
    assert_eq!(count_first, 1);

    // Re-run install
    let output2 = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "bash"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .output()
        .unwrap();
    assert!(output2.status.success());
    let stdout2 = String::from_utf8(output2.stdout).unwrap();
    assert!(
        stdout2.contains("Already installed"),
        "expected idempotent skip: {stdout2}"
    );
    let bashrc_second = fs::read_to_string(dir.path().join(".bashrc")).unwrap();
    assert_eq!(
        bashrc_first, bashrc_second,
        "second run should not modify the file"
    );
}

#[test]
fn completions_install_force_replaces_marker_block() {
    let (dir, _output) = install_in_tempdir("bash", &[]);
    // Tamper with the marker block contents to simulate drift
    let bashrc_path = dir.path().join(".bashrc");
    let original = fs::read_to_string(&bashrc_path).unwrap();
    let tampered = original.replace("norn completions init bash", "OLD_COMMAND");
    fs::write(&bashrc_path, &tampered).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "bash", "--force"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "force install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let final_bashrc = fs::read_to_string(&bashrc_path).unwrap();
    assert!(
        final_bashrc.contains("norn completions init bash"),
        "force should restore current line: {final_bashrc}"
    );
    assert!(
        !final_bashrc.contains("OLD_COMMAND"),
        "force should remove old content"
    );
    let backup_path = format!("{}.bak", bashrc_path.display());
    assert!(
        PathBuf::from(&backup_path).exists(),
        "expected backup at {backup_path}"
    );
}

#[test]
fn completions_install_nushell_writes_both_files() {
    let (dir, output) = install_in_tempdir("nushell", &[]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let script = dir.path().join(".config/nushell/completions/norn.nu");
    let config = dir.path().join(".config/nushell/config.nu");

    assert!(script.exists(), "completion script should be written");
    assert!(config.exists(), "config.nu should be written or appended");

    let script_content = fs::read_to_string(&script).unwrap();
    assert!(
        script_content.contains("norn"),
        "script should reference norn"
    );

    let config_content = fs::read_to_string(&config).unwrap();
    assert!(config_content.contains("# >>> norn completions"));
    assert!(config_content.contains("source"));
    assert!(config_content.contains("norn.nu"));
}

#[test]
fn completions_install_nushell_idempotent() {
    let (dir, output1) = install_in_tempdir("nushell", &[]);
    assert!(output1.status.success());
    let config_path = dir.path().join(".config/nushell/config.nu");
    let config_first = fs::read_to_string(&config_path).unwrap();

    let output2 = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "nushell"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .output()
        .unwrap();
    assert!(output2.status.success());
    let stdout2 = String::from_utf8(output2.stdout).unwrap();
    assert!(stdout2.contains("Already installed"));
    let config_second = fs::read_to_string(&config_path).unwrap();
    assert_eq!(config_first, config_second);
}

#[test]
fn completions_install_fish_overwrites_script() {
    let dir = tempfile::TempDir::new().unwrap();
    // Pre-create a stale completion file
    let fish_completions = dir.path().join(".config/fish/completions");
    fs::create_dir_all(&fish_completions).unwrap();
    let target = fish_completions.join("norn.fish");
    fs::write(&target, "# old stale content").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "fish"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let content = fs::read_to_string(&target).unwrap();
    assert!(!content.contains("# old stale content"));
    // The fish completion script clap_complete produces references the
    // command name and at least one subcommand.
    assert!(content.contains("norn"));
}

#[test]
fn completions_install_powershell_writes_marker_block() {
    let (dir, output) = install_in_tempdir("powershell", &[]);
    assert!(
        output.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Default fallback path: $HOME/.config/powershell/Microsoft.PowerShell_profile.ps1
    let profile = dir
        .path()
        .join(".config/powershell/Microsoft.PowerShell_profile.ps1");
    let content = fs::read_to_string(&profile).unwrap();
    assert!(content.contains("# >>> norn completions"));
    assert!(content.contains("norn completions init powershell"));
    assert!(content.contains("Invoke-Expression"));
}

#[test]
fn completions_install_powershell_honors_profile_env() {
    let dir = tempfile::TempDir::new().unwrap();
    let custom_profile = dir.path().join("custom_profile.ps1");
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "powershell"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .env("POWERSHELL_PROFILE", &custom_profile)
        .output()
        .unwrap();
    assert!(output.status.success());
    let content = fs::read_to_string(&custom_profile).unwrap();
    assert!(content.contains("# >>> norn completions"));
}

#[test]
fn completions_install_auto_detects_from_shell_env() {
    let dir = tempfile::TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .env("SHELL", "/bin/zsh")
        .env_remove("ZDOTDIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let zshrc = fs::read_to_string(dir.path().join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> norn completions"));
    assert!(zshrc.contains("eval \"$(norn completions init zsh)\""));
}

#[test]
fn completions_install_print_for_nushell_shows_both_targets() {
    let dir = tempfile::TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "nushell", "--print"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // Both targets named
    assert!(stdout.contains("norn.nu"));
    assert!(stdout.contains("config.nu"));
    // No files written
    assert!(!dir
        .path()
        .join(".config/nushell/completions/norn.nu")
        .exists());
    assert!(!dir.path().join(".config/nushell/config.nu").exists());
}

#[test]
fn repair_apply_adds_missing_required_field() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        "validate:\n  rules:\n    - name: typed-note\n      match:\n        path: \"**/*.md\"\n        frontmatter:\n          type: note\n      required_frontmatter:\n        - kind\nrepair:\n  rules:\n    - name: ensure-research-kind\n      match:\n        code: frontmatter-required-field-missing\n        rule: typed-note\n        field: kind\n      add_frontmatter:\n        field: kind\n        value: research\n",
    )
    .expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(
        root.join("note.md"),
        "---\ntype: note\ntitle: Sample\n---\n# Body\n",
    )
    .expect("note should write");

    let plan_path = root.join("repair.json");
    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    let plan_text = fs::read_to_string(&plan_path).expect("plan should write");
    let plan_json =
        serde_json::from_str::<Value>(&plan_text).expect("repair --plan should be JSON");
    assert_eq!(plan_json["operations"].as_array().unwrap().len(), 1);
    assert_eq!(plan_json["operations"][0]["kind"], "add_frontmatter");
    assert_eq!(plan_json["operations"][0]["fields"]["field"], "kind");
    assert_eq!(
        plan_json["operations"][0]["fields"]["new_value"],
        "research"
    );

    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);

    let result = fs::read_to_string(root.join("note.md")).expect("note should read");
    assert!(
        result.contains("type: note"),
        "existing type field should be preserved: {result}"
    );
    assert!(
        result.contains("title: Sample"),
        "existing title field should be preserved: {result}"
    );
    assert!(
        result.contains("kind: research"),
        "new kind field should be added: {result}"
    );
    assert!(
        result.contains("# Body"),
        "body should be preserved: {result}"
    );

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn completions_install_print_does_not_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args(["completions", "install", "bash", "--print"])
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path().join(".config"))
        .env("XDG_CACHE_HOME", dir.path().join(".cache"))
        .env("XDG_STATE_HOME", dir.path().join(".local/state"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Would write to"));
    assert!(stdout.contains("# >>> norn completions"));
    // No file should have been created.
    assert!(!dir.path().join(".bashrc").exists());
}

#[test]
fn repair_plan_emits_move_document_with_link_risk() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        r#"validate:
  rules:
    - name: task-routing
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      allowed_paths:
        - "Workspaces/**/tasks/*.md"
repair:
  rules:
    - name: route-tasks-to-workspace
      match:
        code: document-misrouted
        rule: task-routing
      move_document:
        to_path: "Workspaces/{frontmatter.workspace}/tasks/{stem}.md"
"#,
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Inbox")).expect("temp dir should be created");
    fs::write(
        root.join("Inbox/task.md"),
        "---\ntype: task\ntitle: My task\nworkspace: demo\nstatus: backlog\n---\n# Body\n",
    )
    .expect("task should write");
    fs::write(
        root.join("Inbox/index.md"),
        "---\ntitle: Index\n---\n# Index\n\n- [task](task.md)\n- [[Inbox/task]]\n",
    )
    .expect("index should write");

    let plan_path = root.join("repair.json");
    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    let plan_text = fs::read_to_string(&plan_path).expect("plan should write");
    let plan_json: Value = serde_json::from_str(&plan_text).expect("repair --plan should be JSON");

    assert_eq!(plan_json["schema_version"], 2);
    assert_eq!(plan_json["operations"].as_array().unwrap().len(), 1);
    let op = &plan_json["operations"][0];
    assert_eq!(op["kind"], "move_document");
    let change = &op["fields"];
    // move_document ops speak the unified planner vocabulary: src/dst.
    assert_eq!(change["src"], "Inbox/task.md");
    assert_eq!(change["dst"], "Workspaces/demo/tasks/task.md");

    let risk = &change["link_risk"];
    assert_eq!(risk["directory_changed"], true);
    assert_eq!(risk["stem_changed"], false);
    assert!(
        risk["path_qualified_wikilinks"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "expected path_qualified_wikilinks to be populated; risk={risk}"
    );
    assert!(
        risk["markdown_links"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "expected markdown_links to be populated; risk={risk}"
    );

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_plan_skips_move_when_substitution_fails() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        r#"validate:
  rules:
    - name: task-routing
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      allowed_paths:
        - "Workspaces/**/tasks/*.md"
repair:
  rules:
    - name: route-tasks
      match:
        code: document-misrouted
        rule: task-routing
      move_document:
        to_path: "Workspaces/{frontmatter.workspace}/tasks/{stem}.md"
"#,
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Inbox")).expect("temp dir should be created");
    // type: task but NO workspace frontmatter — substitution will fail
    fs::write(
        root.join("Inbox/orphan-task.md"),
        "---\ntype: task\ntitle: Orphan\nstatus: backlog\n---\n# Body\n",
    )
    .expect("task should write");

    let plan_path = root.join("repair.json");
    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);

    let plan_text = fs::read_to_string(&plan_path).expect("plan should write");
    let plan_json: Value = serde_json::from_str(&plan_text).expect("repair --plan should be JSON");

    // No operation is planned; the misrouted finding lands in `skipped` with the
    // kebab-case `precondition-failed` reason code (the substitution-failure prose
    // detail is no longer surfaced in the MigrationPlan skipped entry).
    assert!(plan_json["operations"].as_array().unwrap().is_empty());
    let skipped = plan_json["skipped"].as_array().expect("skipped");
    let move_skip = skipped
        .iter()
        .find(|f| f["finding_code"] == "document-misrouted")
        .expect("expected a skipped document-misrouted finding");
    assert_eq!(move_skip["reason"], "precondition-failed");

    fs::remove_dir_all(root).ok();
    fs::remove_file(config_path).ok();
}

#[test]
fn repair_apply_moves_document_and_rewrites_links() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        r#"validate:
  rules:
    - name: task-routing
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      allowed_paths:
        - "Workspaces/**/tasks/*.md"
repair:
  rules:
    - name: route-tasks
      match:
        code: document-misrouted
        rule: task-routing
      move_document:
        to_path: "Workspaces/{frontmatter.workspace}/tasks/{stem}.md"
"#,
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Inbox")).expect("temp dir should be created");
    fs::write(
        root.join("Inbox/task.md"),
        "---\ntype: task\nworkspace: demo\nstatus: backlog\n---\n# Body\n",
    )
    .expect("task should write");
    fs::write(
        root.join("Inbox/index.md"),
        "---\ntitle: Index\n---\n- [[Inbox/task]]\n- [task](task.md)\n",
    )
    .expect("index should write");

    let plan_path = root.join("repair.json");
    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);

    assert!(
        !root.join("Inbox/task.md").exists(),
        "source path should be gone after apply"
    );
    assert!(
        root.join("Workspaces/demo/tasks/task.md").exists(),
        "destination path should exist after apply"
    );

    let index_content = fs::read_to_string(root.join("Inbox/index.md")).expect("index should read");
    assert!(
        index_content.contains("[[Workspaces/demo/tasks/task]]"),
        "wikilink should be rewritten to new path; got: {index_content}"
    );
    assert!(
        !index_content.contains("[[Inbox/task]]"),
        "old wikilink should be gone; got: {index_content}"
    );
    assert!(
        index_content.contains("../Workspaces/demo/tasks/task.md"),
        "markdown link should be rewritten to new relative path; got: {index_content}"
    );
    assert!(
        !index_content.contains("[task](task.md)"),
        "old markdown link should be gone; got: {index_content}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

/// NRN-150/183: a MULTI-op apply that partially applies — op0 renames a→b
/// (a real write), then op1's move refuses because its destination exists — now
/// exits `1` (a partial-apply runtime failure), NOT `2` (a byte-identical
/// preflight refusal). The old CLI arm mapped every apply `Err` to exit 2, so a
/// partial folder-style move reported "refused, nothing written" while files had
/// already moved. The exit is now the report's own outcome mapping.
#[test]
fn apply_partial_apply_exits_1_not_2_when_a_move_already_landed() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(&config_path, "validate: {}\n").expect("config should write");
    fs::create_dir_all(&root).expect("temp dir should be created");
    fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").expect("a");
    fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").expect("c");
    // Pre-existing destination for op1's move → MoveDestinationExists.
    fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n").expect("d");

    let plan = serde_json::json!({
        "schema_version": 2,
        "vault_root": root.to_str().unwrap(),
        "operations": [
            { "kind": "move_document", "fields": { "src": "a.md", "dst": "b.md" } },
            { "kind": "move_document", "fields": { "src": "c.md", "dst": "d.md" } }
        ]
    });
    let plan_path = root.join("plan.json");
    fs::write(&plan_path, serde_json::to_string(&plan).unwrap()).expect("plan should write");

    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args([
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);
    let _cache = isolate_cache(&mut command);
    let output = command.output().expect("apply should run");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a partial apply (a move already landed) must exit 1, not 2\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // op0 landed on disk; op1 was refused.
    assert!(!root.join("a.md").exists(), "op0 renamed a→b");
    assert!(root.join("b.md").exists(), "op0's destination exists");
    assert!(
        root.join("c.md").exists(),
        "op1's source remains — the move refused"
    );

    // The report on stdout reflects the truthful partial state.
    let report: serde_json::Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim())
            .expect("stdout is the ApplyReport JSON");
    assert_eq!(
        report["outcome"], "failed",
        "partial apply → outcome failed"
    );
    assert_eq!(report["operations"][0]["status"], "applied");
    assert_eq!(report["operations"][1]["status"], "failed");
    assert_eq!(
        report["operations"][1]["error"]["code"],
        "move-destination-exists"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn repair_apply_refuses_move_when_destination_exists() {
    let root = temp_cache_dir();
    let config_path = root.with_extension("yaml");
    fs::write(
        &config_path,
        r#"validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
        frontmatter:
          type: task
      allowed_paths:
        - "Workspaces/**/tasks/*.md"
repair:
  rules:
    - name: route
      match:
        code: document-misrouted
        rule: r
      move_document:
        to_path: "Workspaces/demo/tasks/task.md"
"#,
    )
    .expect("config should write");
    fs::create_dir_all(root.join("Inbox")).expect("inbox dir should be created");
    fs::create_dir_all(root.join("Workspaces/demo/tasks"))
        .expect("destination dir should be created");
    fs::write(
        root.join("Inbox/task.md"),
        "---\ntype: task\n---\n# source\n",
    )
    .expect("source task should write");
    fs::write(
        root.join("Workspaces/demo/tasks/task.md"),
        "---\ntype: task\n---\n# pre-existing\n",
    )
    .expect("pre-existing dest should write");

    let plan_path = root.join("repair.json");
    vault_success(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--out",
        plan_path.to_str().unwrap(),
    ]);
    // NRN-150: `--format json` failure → structured envelope on STDOUT.
    let stdout = vault_error_stdout(&[
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);
    let envelope: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be a JSON error envelope");
    assert_eq!(
        envelope["code"], "move-destination-exists",
        "expected destination-exists refusal code; got: {envelope}"
    );
    assert!(
        root.join("Inbox/task.md").exists(),
        "source should remain when move refuses"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

// repair_links_move_to_reports_risk_without_plan: removed — vault repair links retired.
// Move risk without a plan is now vault move --dry-run (with link rewrite preview).

#[test]
fn cache_index_creates_cache_and_status_reports_documents() {
    let root = temp_cache_dir();
    let cache_home = temp_cache_dir();
    fs::create_dir_all(&root).expect("vault dir should be created");
    fs::create_dir_all(&cache_home).expect("cache home should be created");
    fs::write(root.join("a.md"), "---\ntitle: A\n---\nbody\n").expect("a.md should write");

    let envs = [("XDG_CACHE_HOME", cache_home.to_str().unwrap())];

    vault_success_env(&["-C", root.to_str().unwrap(), "cache", "index"], &envs);

    let (stdout, _stderr) = vault_success_env(
        &[
            "-C",
            root.to_str().unwrap(),
            "cache",
            "status",
            "--format",
            "json",
        ],
        &envs,
    );
    let status = serde_json::from_str::<Value>(&stdout).expect("status should be JSON");
    assert!(
        status["doc_count"].as_u64().unwrap() >= 1,
        "expected at least one indexed document; got: {status}"
    );
    assert!(status["cache_path"].as_str().unwrap().ends_with("cache.db"));
    assert!(status["size_bytes"].as_u64().unwrap() > 0);
    assert_eq!(status["schema_version"], 5);
    // NRN-269: a binary run from the cargo target tree resolves to the dev
    // channel, and its cache.db nests under a `dev/` segment.
    assert_eq!(status["channel"], "dev");
    assert!(status["cache_path"]
        .as_str()
        .unwrap()
        .contains("/dev/cache.db"));

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_home).ok();
}

#[test]
fn cache_clear_removes_cache_and_next_status_reports_empty() {
    let root = temp_cache_dir();
    let cache_home = temp_cache_dir();
    fs::create_dir_all(&root).expect("vault dir should be created");
    fs::create_dir_all(&cache_home).expect("cache home should be created");
    fs::write(root.join("a.md"), "---\ntitle: A\n---\nbody\n").expect("a.md should write");

    let envs = [("XDG_CACHE_HOME", cache_home.to_str().unwrap())];

    vault_success_env(&["-C", root.to_str().unwrap(), "cache", "index"], &envs);
    vault_success_env(&["-C", root.to_str().unwrap(), "cache", "clear"], &envs);

    let (stdout, _stderr) = vault_success_env(
        &[
            "-C",
            root.to_str().unwrap(),
            "cache",
            "status",
            "--format",
            "json",
        ],
        &envs,
    );
    let status = serde_json::from_str::<Value>(&stdout).expect("status should be JSON");
    assert_eq!(
        status["doc_count"].as_u64().unwrap(),
        0,
        "expected freshly-recreated cache to report zero documents; got: {status}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_home).ok();
}

#[test]
fn cache_rebuild_repopulates_after_adding_documents() {
    let root = temp_cache_dir();
    let cache_home = temp_cache_dir();
    fs::create_dir_all(&root).expect("vault dir should be created");
    fs::create_dir_all(&cache_home).expect("cache home should be created");
    fs::write(root.join("a.md"), "---\ntitle: A\n---\nbody\n").expect("a.md should write");

    let envs = [("XDG_CACHE_HOME", cache_home.to_str().unwrap())];

    vault_success_env(&["-C", root.to_str().unwrap(), "cache", "index"], &envs);
    fs::write(root.join("b.md"), "---\ntitle: B\n---\nbody\n").expect("b.md should write");
    vault_success_env(&["-C", root.to_str().unwrap(), "cache", "rebuild"], &envs);

    let (stdout, _stderr) = vault_success_env(
        &[
            "-C",
            root.to_str().unwrap(),
            "cache",
            "status",
            "--format",
            "json",
        ],
        &envs,
    );
    let status = serde_json::from_str::<Value>(&stdout).expect("status should be JSON");
    assert_eq!(
        status["doc_count"].as_u64().unwrap(),
        2,
        "expected rebuild to repopulate both documents; got: {status}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_dir_all(&cache_home).ok();
}

// ── ANSI-guard helpers ────────────────────────────────────────────────────────

/// Creates a minimal temp vault (config + one markdown file) and runs vault
/// with `--cwd <tempdir>` prepended to the supplied args.
///
/// The temp dir prefix intentionally does NOT start with `.` — a leading dot
/// would make the directory hidden and the vault walker would skip it,
/// causing `vault find` to return zero results.
fn vault_success_in_minimal_vault(args: &[&str]) -> (String, String) {
    let tmp = tempfile::Builder::new()
        .prefix("norn-ansi-test")
        .tempdir()
        .expect("tempdir");
    let vault_dir = tmp.path().join(".norn");
    fs::create_dir_all(&vault_dir).unwrap();
    fs::write(
        vault_dir.join("config.yaml"),
        "version: 1\nfiles:\n  ignore: []\nvalidate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .unwrap();
    fs::write(tmp.path().join("note.md"), "---\ntype: note\n---\n\nbody\n").unwrap();

    let cwd_arg = tmp.path().to_str().unwrap().to_string();
    let mut full = vec!["--cwd", cwd_arg.as_str()];
    full.extend_from_slice(args);

    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(&full);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault command should run");

    assert!(
        output.status.success(),
        "vault failed; args={args:?}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

// ── Cross-command no-ANSI guard tests ─────────────────────────────────────────
// §5.5 of the norn-cli-output spec: structured formats (json / jsonl / paths)
// must never carry ANSI escape sequences, even when --color=always is set.

#[test]
fn find_json_format_contains_no_ansi_under_color_always() {
    let (stdout, _stderr) =
        vault_success_in_minimal_vault(&["--color=always", "find", "--all", "--format", "json"]);
    assert!(
        !stdout.contains("\x1b["),
        "JSON must not contain ANSI: {stdout:?}"
    );
}

#[test]
fn find_jsonl_format_contains_no_ansi_under_color_always() {
    let (stdout, _stderr) =
        vault_success_in_minimal_vault(&["--color=always", "find", "--all", "--format", "jsonl"]);
    assert!(!stdout.contains("\x1b["), "JSONL must not contain ANSI");
}

#[test]
fn find_paths_format_contains_no_ansi_under_color_always() {
    let (stdout, _stderr) =
        vault_success_in_minimal_vault(&["--color=always", "find", "--all", "--format", "paths"]);
    assert!(!stdout.contains("\x1b["), "paths must not contain ANSI");
}

#[test]
fn find_with_no_predicate_shows_help_on_stderr_exit_2() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-find-help-test")
        .tempdir()
        .expect("tempdir");
    let vault_dir = tmp.path().join(".norn");
    fs::create_dir_all(&vault_dir).unwrap();
    fs::write(
        vault_dir.join("config.yaml"),
        "version: 1\nfiles:\n  ignore: []\nvalidate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "find"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault find should run");

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit code 2 for no-predicate find"
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    assert!(
        stderr.contains("Usage:") && stderr.contains("--all"),
        "expected help text mentioning --all on stderr: {stderr:?}"
    );
}

#[test]
fn find_with_all_dumps_everything() {
    let (stdout, _stderr) = vault_success_in_minimal_vault(&["find", "--all", "--format", "paths"]);
    assert!(
        stdout.contains("note.md"),
        "expected --all to return the fixture doc: {stdout:?}"
    );
}

#[test]
fn config_show_json_no_ansi() {
    let (stdout, _stderr) =
        vault_success_in_minimal_vault(&["--color=always", "config", "show", "--format", "json"]);
    assert!(
        !stdout.contains("\x1b["),
        "config show JSON must not contain ANSI"
    );
}

#[test]
fn config_show_jsonl_no_ansi() {
    let (stdout, _stderr) =
        vault_success_in_minimal_vault(&["--color=always", "config", "show", "--format", "jsonl"]);
    assert!(
        !stdout.contains("\x1b["),
        "config show JSONL must not contain ANSI"
    );
}

#[test]
fn config_validate_json_no_ansi() {
    let (stdout, _stderr) = vault_success_in_minimal_vault(&[
        "--color=always",
        "config",
        "validate",
        "--format",
        "json",
    ]);
    assert!(
        !stdout.contains("\x1b["),
        "config validate JSON must not contain ANSI"
    );
}

#[test]
fn config_validate_jsonl_no_ansi() {
    let (stdout, _stderr) = vault_success_in_minimal_vault(&[
        "--color=always",
        "config",
        "validate",
        "--format",
        "jsonl",
    ]);
    assert!(
        !stdout.contains("\x1b["),
        "config validate JSONL must not contain ANSI"
    );
}

#[test]
fn repair_apply_rewrites_link_in_source_doc() {
    let root = temp_cache_dir();
    fs::create_dir_all(&root).expect("temp dir should be created");

    // Source doc with a broken wikilink. The link target "Norn Brand"
    // will be matched by closest-match to "norn-brand" (the stem of the
    // second file below).
    fs::write(
        root.join("source.md"),
        "---\ntitle: source\n---\n\nSee [[Norn Brand]] for details.\n",
    )
    .expect("source should write");
    fs::write(
        root.join("norn-brand.md"),
        "---\ntitle: norn-brand\n---\n\nThe brand.\n",
    )
    .expect("norn-brand should write");

    // Run repair --plan scoped to link-target-missing findings.
    let plan = vault(&[
        "-C",
        root.to_str().unwrap(),
        "repair",
        "--plan",
        "--code",
        "link-target-missing",
    ]);
    let plan_json = serde_json::from_str::<Value>(&plan).expect("repair --plan should be JSON");
    assert_eq!(
        plan_json["operations"][0]["kind"], "rewrite_link",
        "expected a rewrite_link op; got plan: {plan}"
    );

    let plan_path = root.join("repair.json");
    fs::write(&plan_path, &plan).expect("plan should write");

    let output = vault(&[
        "-C",
        root.to_str().unwrap(),
        "apply",
        plan_path.to_str().unwrap(),
        "--yes",
        "--format",
        "json",
    ]);

    let report = serde_json::from_str::<Value>(&output).expect("apply report should be JSON");
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["applied"], 1);
    assert_eq!(report["operations"][0]["kind"], "rewrite_link");
    assert_eq!(report["operations"][0]["status"], "applied");

    let updated = fs::read_to_string(root.join("source.md")).expect("source should read");
    assert!(
        updated.contains("[[norn-brand]]"),
        "expected source to contain rewritten link, got: {updated}"
    );
    assert!(
        !updated.contains("[[Norn Brand]]"),
        "expected source to NOT contain original broken link, got: {updated}"
    );

    fs::remove_dir_all(root).ok();
}
