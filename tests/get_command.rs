//! Integration tests for `vault get`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(
        root.join("b.md"),
        "---\ntype: note\n---\n# B\n[[a]]\n[[missing]]\n",
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

#[test]
fn get_single_target_json() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["path"], "a.md");
}

#[test]
fn get_wikilink_target() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "[[a]]", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v[0]["path"], "a.md");
}

#[test]
fn get_multiple_targets_returns_array() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "b.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
}

#[test]
fn get_col_narrows_output() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "get",
            "a.md",
            "--col",
            ".incoming_links",
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
    let record = &v[0];
    assert!(record.get("incoming_links").is_some());
    assert!(record.get("headings").is_none());
}

#[test]
fn get_col_bare_name_projects_frontmatter_field() {
    // The headline unification: `get --col <field>` selects a frontmatter field
    // (like `find --col`), no longer rejected as an unknown column. Self-contained
    // doc with two frontmatter keys so we can prove the projection filters.
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-col-bare-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\nstatus: active\n---\n# A\n",
    )
    .unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["get", "a.md", "--col", "status", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unknown") && !stderr.contains("not present"),
        "bare frontmatter field must not warn; got: {stderr}"
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let fm = v[0].get("frontmatter").expect("frontmatter object present");
    // Projected to just `status` — `type` is filtered out.
    assert_eq!(fm.get("status").and_then(|s| s.as_str()), Some("active"));
    assert!(
        fm.get("type").is_none(),
        "non-requested keys filtered; got: {fm}"
    );
    // Structural facets are not present unless dot-requested.
    assert!(v[0].get("headings").is_none());
}

#[test]
fn get_col_unknown_facet_warns() {
    // A dot-prefixed token that isn't a known structural facet warns.
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--col", ".bogus", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(".bogus") && stderr.contains("facet"),
        "expected unknown-facet warning; got: {stderr}"
    );
}

#[test]
fn get_body_flag_includes_content() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--body", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(v[0]["body"].as_str().unwrap().contains("A"));
}

#[test]
fn get_unknown_col_warns_on_stderr() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "get",
            "a.md",
            "--col",
            "nonexistent_field",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    // Non-fatal: still succeeds. Warning on stderr.
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("nonexistent_field") || stderr.contains("unknown"),
        "expected stderr warning for unknown col; got: {}",
        stderr
    );
}

#[test]
fn get_paths_format_one_path_per_line() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "b.md", "--format", "paths"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines, vec!["a.md", "b.md"]);
}

#[test]
fn get_jsonl_format_one_object_per_line() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "b.md", "--format", "jsonl"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2);
    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["path"], "a.md");
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["path"], "b.md");
}

#[test]
fn get_col_ignored_with_paths_warns() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--col", "status", "--format", "paths"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--col is ignored with --format paths"),
        "expected col-ignored warning; got: {stderr}"
    );
    // stdout is still just the path — `--col` had no effect.
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "a.md");
}

#[test]
fn get_records_default_frontmatter_is_per_field_lines() {
    // Phase-2 flip: the default `records` view renders each frontmatter key as
    // its own labeled line (matching `find`), not one consolidated
    // `frontmatter` block. `--col .frontmatter` recovers the block form.
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-records-default-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\nstatus: active\n---\n# A\n",
    )
    .unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["get", "a.md"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Both frontmatter keys appear as their own labeled lines.
    assert!(
        stdout.contains("type") && stdout.contains("note"),
        "{stdout}"
    );
    assert!(
        stdout.contains("status") && stdout.contains("active"),
        "{stdout}"
    );
    // The consolidated `frontmatter` block label is gone from the default view.
    assert!(
        !stdout.contains("frontmatter"),
        "default records view should not show a consolidated frontmatter block; got: {stdout}"
    );
}

#[test]
fn get_missing_target_partial_failure_exit() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "nonexistent", "--format", "json"])
        .output()
        .unwrap();
    // Non-zero exit because one target failed; stdout still has the one
    // that succeeded.
    assert!(!out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nonexistent"));
}

#[test]
fn get_col_body_without_body_flag_shows_body() {
    // Regression: `get --col .body` (no `--body`) used to show nothing because
    // the body only loaded when `--body` was passed. A requested heavy facet
    // must load itself.
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-col-body-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.md"),
        "---\ntype: note\n---\n# A heading\n\nthe body text\n",
    )
    .unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["get", "a.md", "--col", ".body", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(
        v[0]["body"].as_str().unwrap().contains("the body text"),
        "expected body without --body flag: {}",
        v
    );
}

#[test]
fn get_col_raw_reads_disk_byte_faithful() {
    // `.raw` reads the whole source file verbatim — frontmatter block, comment,
    // body, and trailing whitespace all preserved — even with no `--body`.
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-col-raw-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    let contents =
        "---\ntype: note\ntitle: Alpha\n# a yaml comment\n---\n\n# Heading\n\nbody text\n\n   \n";
    std::fs::write(root.join("a.md"), contents).unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["get", "a.md", "--col", ".raw", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let file_bytes = std::fs::read_to_string(root.join("a.md")).unwrap();
    assert_eq!(
        v[0]["raw"].as_str().unwrap(),
        file_bytes,
        "raw facet must equal exact file bytes"
    );
}

#[test]
fn get_default_no_col_omits_raw() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(
        v[0].get("raw").is_none(),
        "raw must not appear by default: {}",
        v
    );
}
