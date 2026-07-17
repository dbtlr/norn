use std::fs;
use std::process::Command;
use tempfile::{Builder, TempDir};

/// vault_graph's walker skips the root if its filename starts with `.`,
/// and `tempfile::TempDir::new()` defaults to a `.tmpXXX` prefix on
/// macOS — which would make every test's "vault" invisible to the scan.
/// Force a non-hidden prefix so the tempdir resembles a normal vault.
fn vault_tempdir() -> TempDir {
    Builder::new()
        .prefix("vault-init-test-")
        .tempdir()
        .expect("create non-hidden tempdir")
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

/// Wraps a vault invocation with per-test `XDG_CACHE_HOME` and
/// `XDG_STATE_HOME` trees so each test gets a fresh SQLite cache and the
/// binary never reads or sweeps the developer's real cache/state trees.
/// Mirrors the helper in `tests/cli_output.rs`.
fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    prewrite_prune_marker(dir.path());
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    dir
}

#[test]
fn init_creates_config_with_default_stubs_and_common_ignores() {
    let tmp = vault_tempdir();
    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.args(["--cwd", tmp.path().to_str().unwrap(), "init"]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(0));

    let body = fs::read_to_string(tmp.path().join(".norn/config.yaml")).unwrap();
    assert!(body.contains("version: 1"), "body={body}");
    assert!(body.contains(".obsidian/"), "body={body}");
    assert!(body.contains(".git/"), "body={body}");
    assert!(body.contains(".trash/"), "body={body}");
    assert!(body.contains("node_modules/"), "body={body}");
    assert!(body.contains("validate:"), "body={body}");
    assert!(body.contains("repair:"), "body={body}");
}

#[test]
fn init_refuses_without_force_when_config_exists() {
    let tmp = vault_tempdir();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "version: 1\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.args(["--cwd", tmp.path().to_str().unwrap(), "init"]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(1));

    // Untouched
    let body = fs::read_to_string(tmp.path().join(".norn/config.yaml")).unwrap();
    assert_eq!(body, "version: 1\n");
}

#[test]
fn init_force_overwrites_existing_config() {
    let tmp = vault_tempdir();
    fs::create_dir_all(tmp.path().join(".norn")).unwrap();
    fs::write(tmp.path().join(".norn/config.yaml"), "version: 1\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.args(["--cwd", tmp.path().to_str().unwrap(), "init", "--force"]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(0));

    let body = fs::read_to_string(tmp.path().join(".norn/config.yaml")).unwrap();
    assert!(body.contains(".obsidian/"));
}

#[test]
fn init_scaffold_records_observed_fields_when_markdown_present() {
    let tmp = vault_tempdir();
    fs::write(
        tmp.path().join("a.md"),
        "---\ntype: note\nkind: thing\n---\nbody\n",
    )
    .unwrap();
    fs::write(tmp.path().join("b.md"), "---\ntype: note\n---\nbody\n").unwrap();

    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.args(["--cwd", tmp.path().to_str().unwrap(), "init"]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(0));

    let body = fs::read_to_string(tmp.path().join(".norn/config.yaml")).unwrap();
    assert!(body.contains("Observed in this vault"), "body={body}");
    assert!(body.contains("type"), "body={body}");
    assert!(
        body.contains("2/2"),
        "expected `type` count line, body={body}"
    );
}

#[test]
fn init_with_no_markdown_uses_empty_observation_block() {
    let tmp = vault_tempdir();
    let bin = env!("CARGO_BIN_EXE_norn");
    let mut command = Command::new(bin);
    command.args(["--cwd", tmp.path().to_str().unwrap(), "init"]);
    let _cache_dir = isolate_cache(&mut command);
    let status = command.status().unwrap();
    assert_eq!(status.code(), Some(0));

    let body = fs::read_to_string(tmp.path().join(".norn/config.yaml")).unwrap();
    assert!(body.contains("No markdown files found"), "body={body}");
}
