//! Integration tests for `vault count`.

use std::process::Command;
use tempfile::TempDir;

fn synth_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-count-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\nstatus: active\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        root.join("c.md"),
        "---\ntype: log\nstatus: backlog\n---\nbody\n",
    )
    .unwrap();
    tmp
}

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn norn_cmd(tmp: &tempfile::TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
    c
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
fn count_total_only_emits_total() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(v["total"], 3);
    assert!(v.get("by").is_none());
}

#[test]
fn count_by_field_groups() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "status", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["by"], "status");
    assert_eq!(v["total"], 3);
    assert_eq!(v["groups"]["active"], 1);
    assert_eq!(v["groups"]["backlog"], 2);
}

#[test]
fn count_by_single_key_shape_is_unchanged() {
    // Load-bearing for external consumers (Mimir): with ONE key, `by` stays
    // a plain string and `groups` a flat value→count map — not the nested
    // multi-key shape.
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "status", "--format", "json"])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(v["by"].is_string(), "single-key `by` must be a string: {v}");
    let groups = v["groups"].as_object().unwrap();
    assert!(
        groups.values().all(serde_json::Value::is_u64),
        "single-key groups must be flat value→count: {v}"
    );
}

#[test]
fn count_by_multi_key_nests_groups() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "type,status", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["by"], serde_json::json!(["type", "status"]));
    assert_eq!(v["total"], 3);
    assert_eq!(v["groups"]["note"]["active"], 1);
    assert_eq!(v["groups"]["note"]["backlog"], 1);
    assert_eq!(v["groups"]["log"]["backlog"], 1);
}

#[test]
fn count_by_multi_key_text_render_nests() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "type,status"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(stdout.contains("total"), "{stdout}");
    // Branch keys appear as their own lines; leaves indented beneath them.
    assert!(stdout.contains("note"), "{stdout}");
    assert!(stdout.contains("  active"), "{stdout}");
    assert!(stdout.contains("  backlog"), "{stdout}");
}

#[test]
fn count_by_missing_field_nests_missing_marker() {
    let tmp = synth_vault();
    // c.md has no `owner`; grouping type,owner buckets it under (missing).
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "type,owner", "--format", "json"])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["groups"]["log"]["(missing)"], 1);
    assert_eq!(v["groups"]["note"]["(missing)"], 2);
}

#[test]
fn count_by_whitespace_padded_keys_are_trimmed() {
    let tmp = synth_vault();
    // The natural comma-space spelling must group on `status`, not " status".
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", "type, status", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["by"], serde_json::json!(["type", "status"]));
    assert_eq!(v["groups"]["note"]["active"], 1);
    assert_eq!(v["groups"]["log"]["backlog"], 1);
}

#[test]
fn count_by_duplicate_key_errors() {
    let tmp = synth_vault();
    // Both spellings of a duplicate: inside one token and via a repeated flag.
    for args in [
        vec!["count", "--by", "status,status"],
        vec!["count", "--by", "status", "--by", "status"],
    ] {
        let out = norn_cmd(&tmp)
            .args(["--cwd"])
            .arg(tmp.path().join("vault"))
            .args(&args)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{args:?} should be rejected, stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("duplicate field"),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn count_by_too_many_keys_errors_cleanly() {
    let tmp = synth_vault();
    let many = vec!["k"; 17]
        .iter()
        .enumerate()
        .map(|(i, k)| format!("{k}{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["count", "--by", &many])
        .output()
        .unwrap();
    assert!(!out.status.success(), "17 keys should be rejected");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("at most 16"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn count_by_empty_key_errors() {
    let tmp = synth_vault();
    for by in ["type,", ",type", "type,,status", ""] {
        let out = norn_cmd(&tmp)
            .args(["--cwd"])
            .arg(tmp.path().join("vault"))
            .args(["count", "--by", by, "--format", "json"])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "--by {by:?} should be rejected, stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn count_filter_then_by_narrows() {
    let tmp = synth_vault();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "count",
            "--eq",
            "type:note",
            "--by",
            "status",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v["total"], 2);
    assert_eq!(v["groups"]["active"], 1);
    assert_eq!(v["groups"]["backlog"], 1);
}
