use std::process::Command;

fn norn() -> Command {
    Command::new(env!("CARGO_BIN_EXE_norn"))
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

/// Create a vault tempdir with a non-hidden prefix so norn's file scanner
/// doesn't skip it. On macOS, `tempfile::tempdir()` creates dirs named `.tmp*`
/// (leading dot) which the scanner treats as hidden and skips.
fn vault_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("norn-audit-")
        .tempdir()
        .unwrap()
}

#[test]
fn audit_surfaces_a_persisted_mutation() {
    let vault = vault_dir();
    std::fs::write(
        vault.path().join("note.md"),
        "---\ntype: note\nstatus: active\n---\nbody\n",
    )
    .unwrap();
    let state = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    prewrite_prune_marker(cache.path());
    let vault_path = vault.path().to_str().unwrap();

    // A real (confirmed) mutation persists an event to the stream.
    // `set` requires --field KEY=VALUE; positional KEY=VALUE is not supported.
    let set = norn()
        .args([
            "--cwd",
            vault_path,
            "set",
            "note.md",
            "--field",
            "status=done",
            "--yes",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", cache.path())
        .output()
        .unwrap();
    assert!(
        set.status.success(),
        "set failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&set.stdout),
        String::from_utf8_lossy(&set.stderr)
    );

    // audit reads it back.
    let audit = norn()
        .args(["--cwd", vault_path, "audit", "--format", "json"])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", cache.path())
        .output()
        .unwrap();
    assert!(audit.status.success());
    let stdout = String::from_utf8(audit.stdout).unwrap();
    let arr: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let events = arr.as_array().unwrap();
    assert!(!events.is_empty(), "audit should surface the set mutation");
    assert!(
        events.iter().any(|e| {
            e["event"].as_str().unwrap_or("").starts_with("action.")
                && e["target"].as_str() == Some("note.md")
        }),
        "expected an action event targeting note.md, got: {stdout}"
    );
}

#[test]
fn audit_empty_when_no_mutations() {
    let vault = vault_dir();
    let state = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    prewrite_prune_marker(cache.path());
    let out = norn()
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "audit",
            "--format",
            "json",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", cache.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "empty audit must exit 0");
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "[]");
}

#[test]
fn audit_bad_since_exits_2() {
    let vault = vault_dir();
    let state = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    prewrite_prune_marker(cache.path());
    let out = norn()
        .args([
            "--cwd",
            vault.path().to_str().unwrap(),
            "audit",
            "--since",
            "nope",
        ])
        .env("XDG_STATE_HOME", state.path())
        .env("XDG_CACHE_HOME", cache.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
