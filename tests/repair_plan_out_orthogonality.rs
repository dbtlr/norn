use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Creates an isolated temp directory using a non-hidden prefix so the
/// walker does not treat it as hidden.
///
/// Returns the path; the caller must call `fs::remove_dir_all` for cleanup
/// since we need the directory to persist beyond the `TempDir` scope.
fn vault_root(prefix: &str) -> PathBuf {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("temp dir should be created");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// Build a vault fixture that produces at least one planned change.
///
/// Strategy: a target document `norn-brand.md` whose stem slug-normalizes to
/// "norn-brand", and a source document with a wikilink `[[Norn Brand]]`.  The
/// slug-normalization produces a High-confidence closest-match proposal.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_fixture_with_changes() -> (PathBuf, PathBuf) {
    let root = vault_root("vault-cli-out-orthog-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    fs::write(
        root.join("norn-brand.md"),
        "---\ntitle: Norn Brand\n---\n# Norn Brand\n\nTarget document.\n",
    )
    .expect("norn-brand.md should write");

    fs::write(
        root.join("source.md"),
        "---\ntitle: Source\n---\n# Source\n\nSee [[Norn Brand]] for details.\n",
    )
    .expect("source.md should write");

    (root, config_path)
}

/// Runs `vault repair plan` with the given extra args against the fixture.
/// Returns the raw `std::process::Output`.
fn run_plan(root: &Path, config_path: &Path, extra_args: &[&str]) -> std::process::Output {
    let cache_dir = tempfile::Builder::new()
        .prefix("vault-cli-cache-")
        .tempdir()
        .expect("cache temp dir should be created");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vault"));
    cmd.args([
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "plan",
    ]);
    cmd.args(extra_args);
    cmd.env("XDG_CACHE_HOME", cache_dir.path())
        .env("NO_COLOR", "1");

    cmd.output().expect("vault command should execute")
}

/// --out alone: file is written with JSON, stdout is silent.
#[test]
fn out_alone_writes_file_and_keeps_stdout_silent() {
    let (root, config_path) = build_fixture_with_changes();
    let out_path = root.join("plan.json");

    let output = run_plan(&root, &config_path, &["--out", out_path.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "command failed\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout, "",
        "stdout must be silent when --out alone, got: {stdout:?}"
    );
    assert!(out_path.exists(), "file must be written");
    let body = fs::read_to_string(&out_path).unwrap();
    assert!(
        body.trim_start().starts_with('{'),
        "file must contain JSON, got: {body}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

/// --out + --format report: file gets JSON, stdout gets the report summary.
#[test]
fn out_plus_format_report_writes_file_and_emits_summary_to_stdout() {
    let (root, config_path) = build_fixture_with_changes();
    let out_path = root.join("plan.json");

    let output = run_plan(
        &root,
        &config_path,
        &["--out", out_path.to_str().unwrap(), "--format", "report"],
    );
    assert!(
        output.status.success(),
        "command failed\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Repair plan against"),
        "stdout must contain report header, got: {stdout}"
    );
    assert!(out_path.exists(), "file must be written");
    let body = fs::read_to_string(&out_path).unwrap();
    assert!(
        body.trim_start().starts_with('{'),
        "file must contain JSON, got: {body}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

/// --out + --format paths: file gets JSON, stdout gets path lines (no JSON, no report).
#[test]
fn out_plus_format_paths_writes_json_to_file_and_paths_to_stdout() {
    let (root, config_path) = build_fixture_with_changes();
    let out_path = root.join("plan.json");

    let output = run_plan(
        &root,
        &config_path,
        &["--out", out_path.to_str().unwrap(), "--format", "paths"],
    );
    assert!(
        output.status.success(),
        "command failed\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Paths format emits one path per line — no JSON braces, no report header.
    assert!(
        !stdout.contains('{'),
        "stdout must not contain JSON braces in paths mode, got: {stdout}"
    );
    assert!(
        !stdout.contains("Repair plan against"),
        "stdout must not contain report header in paths mode, got: {stdout}"
    );
    // The fixture produces at least one change, so at least one path line must appear.
    assert!(
        !stdout.trim().is_empty(),
        "stdout must contain at least one path line, got: {stdout:?}"
    );
    assert!(out_path.exists(), "file must be written");
    let body = fs::read_to_string(&out_path).unwrap();
    assert!(
        body.trim_start().starts_with('{'),
        "file must contain JSON, got: {body}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

/// --out + --format json: both file and stdout receive JSON.
#[test]
fn out_plus_format_json_writes_json_to_both_streams() {
    let (root, config_path) = build_fixture_with_changes();
    let out_path = root.join("plan.json");

    let output = run_plan(
        &root,
        &config_path,
        &["--out", out_path.to_str().unwrap(), "--format", "json"],
    );
    assert!(
        output.status.success(),
        "command failed\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim_start().starts_with('{'),
        "stdout must contain JSON envelope, got: {stdout}"
    );
    assert!(out_path.exists(), "file must be written");
    let body = fs::read_to_string(&out_path).unwrap();
    assert!(
        body.trim_start().starts_with('{'),
        "file must contain JSON, got: {body}"
    );

    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}
