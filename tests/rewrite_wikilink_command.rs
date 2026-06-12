//! Integration test for `norn rewrite-wikilink OLD NEW`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-rewrite-wikilink-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("target.md"), "---\ntype: note\n---\n# Target\n").unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\nworkspace: \"[[target]]\"\n---\n# A\n[[target]]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\n---\n# B\nReferences [[target|with display]] in body.\n",
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
    c
}

#[test]
fn rewrite_wikilink_dry_run_shows_body_and_frontmatter() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args([
            "rewrite-wikilink",
            "target",
            "new-target",
            "--dry-run",
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let ops = report["operations"].as_array().unwrap();
    let body_rewrites: Vec<_> = ops.iter().filter(|o| o["kind"] == "rewrite_link").collect();
    let fm_updates: Vec<_> = ops
        .iter()
        .filter(|o| o["kind"] == "set_frontmatter")
        .collect();
    assert_eq!(body_rewrites.len(), 2, "a.md + b.md → 2 rewrite_link ops");
    assert_eq!(fm_updates.len(), 1, "a.md workspace → 1 set_frontmatter op");
}

#[test]
fn rewrite_wikilink_refuses_when_old_unresolvable() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args([
            "rewrite-wikilink",
            "no-such-target",
            "new-target",
            "--dry-run",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "exit 2 on pre-flight refusal");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("does not resolve")
            || stderr.to_lowercase().contains("no document")
            || stderr.to_lowercase().contains("not found")
            || stderr.to_lowercase().contains("unresolvable"),
        "stderr should explain refusal; got: {}",
        stderr
    );
}
