use std::fs;
use std::process::Command;

use tempfile::TempDir;

/// Wraps a vault invocation with per-test `XDG_CACHE_HOME` and
/// `XDG_STATE_HOME` trees so each test gets a fresh SQLite cache and the
/// binary never reads or sweeps the developer's real cache/state trees.
/// Mirrors the helper in `tests/cli_output.rs`.
fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    prewrite_prune_marker(dir.path());
    dir
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

#[test]
fn config_help_lists_show_validate_migrate_edit() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["config", "--help"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config --help should run");

    assert!(
        output.status.success(),
        "vault config --help failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let text = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(text.contains("show"), "help should list `show`:\n{text}");
    assert!(
        text.contains("validate"),
        "help should list `validate`:\n{text}"
    );
    assert!(
        text.contains("migrate"),
        "help should list `migrate`:\n{text}"
    );
    assert!(text.contains("edit"), "help should list `edit`:\n{text}");
}

fn write_config(dir: &std::path::Path, body: &str) {
    let vault_dir = dir.join(".norn");
    fs::create_dir_all(&vault_dir).unwrap();
    fs::write(vault_dir.join("config.yaml"), body).unwrap();
}

#[test]
fn config_show_without_config_errors_with_hint() {
    let tmp = TempDir::new().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "config", "show"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config show should run");

    assert!(!output.status.success(), "expected non-zero exit");
    assert_eq!(output.status.code(), Some(1), "expected exit code 1");
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    assert!(
        stderr.contains("no .norn/config.yaml found"),
        "stderr={stderr}"
    );
    assert!(stderr.contains("norn init"), "stderr={stderr}");
}

#[test]
fn config_show_records_includes_paths_and_counts() {
    let tmp = TempDir::new().unwrap();
    write_config(
        tmp.path(),
        "version: 1\nfiles:\n  ignore:\n    - a\n    - b\nvalidate:\n  required_frontmatter: [x]\n  rules: []\nrepair:\n  rules: []\n",
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "show",
        "--format",
        "json",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command
        .output()
        .expect("vault config show --format json should run");

    assert!(
        output.status.success(),
        "vault config show --format json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be JSON");
    assert_eq!(parsed["version"], 1);
    assert!(parsed["file"]
        .as_str()
        .unwrap()
        .ends_with(".norn/config.yaml"));
    assert_eq!(parsed["files"]["ignore_count"], 2);
    assert_eq!(parsed["validate"]["required_count"], 1);
    assert_eq!(parsed["validate"]["rule_count"], 0);
    assert_eq!(parsed["repair"]["rule_count"], 0);
}

#[test]
fn config_show_uses_records_default_on_tty_like_output() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\nfiles:\n  ignore: []\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "show",
        "--format",
        "records",
        "--no-pager",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command
        .output()
        .expect("vault config show --format records should run");

    assert!(
        output.status.success(),
        "vault config show --format records failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let text = String::from_utf8(output.stdout).expect("stdout UTF-8");
    // lines[0] is the leading blank for prompt breathing room.
    // lines[1] is the header — the config file path.
    let header_line = text.lines().nth(1).unwrap_or("");
    assert!(
        header_line.ends_with(".norn/config.yaml"),
        "expected file path header, got: {header_line:?}"
    );
    // Field rows are 2-indent.
    assert!(text.contains("  vault_root"));
    assert!(text.contains("  cache"));
    assert!(text.contains("  version"));
    // "file" is in the header, not as a field row.
    assert!(
        !text.contains("\n  file "),
        "file should be header, not a field row"
    );
}

#[test]
fn config_validate_clean_returns_exit_0() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\nfiles:\n  ignore: []\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "config", "validate"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config validate should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0 for clean config\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn config_validate_unknown_version_reports_error_exit_2() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 99\nfiles:\n  ignore: []\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "validate",
        "--format",
        "json",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command
        .output()
        .expect("vault config validate --format json should run");

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 for unknown version\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    assert!(
        stdout.contains("unknown-schema-version"),
        "stdout did not contain unknown-schema-version code:\n{stdout}"
    );
}

#[test]
fn config_validate_unknown_field_reports_error_exit_2() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\nbogus: true\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "config", "validate"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config validate should run");

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 for unknown field\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn config_validate_missing_file_returns_exit_3() {
    let tmp = TempDir::new().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "config", "validate"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config validate should run");

    assert_eq!(
        output.status.code(),
        Some(3),
        "expected exit 3 for missing config\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn config_migrate_v1_prints_nothing_to_migrate() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\nfiles:\n  ignore: []\n");
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "config", "migrate"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config migrate should run");

    assert_eq!(
        output.status.code(),
        Some(0),
        "expected exit 0 for v1 config\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let text = String::from_utf8(output.stdout).expect("stdout UTF-8");
    assert!(
        text.contains("nothing to migrate"),
        "stdout did not contain 'nothing to migrate':\n{text}"
    );
}

#[test]
fn config_migrate_missing_file_returns_exit_1() {
    let tmp = TempDir::new().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.args(["--cwd", tmp.path().to_str().unwrap(), "config", "migrate"]);
    let _cache_dir = isolate_cache(&mut command);
    let output = command.output().expect("vault config migrate should run");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for missing config\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn config_edit_no_editor_set_returns_exit_1() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\n");
    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.env_remove("VISUAL").env_remove("EDITOR").args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "edit",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let out = command.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("EDITOR"), "stderr={stderr}");
}

#[test]
fn config_edit_no_config_file_returns_exit_1() {
    let tmp = TempDir::new().unwrap();
    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.env("EDITOR", "true").env_remove("VISUAL").args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "edit",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let out = command.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("no .norn/config.yaml"), "stderr={stderr}");
}

#[test]
fn config_edit_with_true_editor_exits_0_after_post_validate() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\nfiles:\n  ignore: []\n");
    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.env("EDITOR", "true").env_remove("VISUAL").args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "edit",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn config_edit_visual_takes_precedence_over_editor() {
    let tmp = TempDir::new().unwrap();
    write_config(tmp.path(), "version: 1\n");
    // `false` exits 1; if VISUAL wins, the wrapper sees editor failure.
    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.env("VISUAL", "false").env("EDITOR", "true").args([
        "--cwd",
        tmp.path().to_str().unwrap(),
        "config",
        "edit",
        "--no-validate",
    ]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(
        status.code(),
        Some(1),
        "VISUAL=false should run and exit 1, proving VISUAL won over EDITOR=true"
    );
}
