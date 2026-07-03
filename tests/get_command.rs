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

/// Build a `norn` Command with `XDG_CACHE_HOME`/`XDG_STATE_HOME` isolated to
/// per-test subdirs of the test tempdir, so the binary never reads or sweeps
/// the developer's real cache/state trees.
fn norn_cmd(tmp: &tempfile::TempDir) -> Command {
    let mut c = Command::new(norn_bin());
    c.env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"));
    c
}

fn json_of(out: &std::process::Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap()
}

/// NRN-105: `get --col .document_hash` emits the full-content blake3 hex, it
/// equals what `find --col .document_hash` reports for the same doc (parity),
/// and it is absent from the default dump (opt-in/identity-class).
#[test]
fn get_document_hash_facet_matches_find_and_content() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let expected = blake3::hash(&std::fs::read(vault.join("a.md")).unwrap())
        .to_hex()
        .to_string();

    // get side
    let g = json_of(
        &norn_cmd(&tmp)
            .args(["--cwd"])
            .arg(&vault)
            .args(["get", "a.md", "--col", ".document_hash", "--format", "json"])
            .output()
            .unwrap(),
    );
    let get_hash = g[0]["document_hash"].as_str().unwrap();
    assert_eq!(get_hash, expected, "get emits full-content blake3: {g}");

    // find side — same doc, same hash (parity across the shared facet).
    let f = json_of(
        &norn_cmd(&tmp)
            .args(["--cwd"])
            .arg(&vault)
            .args([
                "find",
                "--eq",
                "type:note",
                "--col",
                ".document_hash",
                "--format",
                "json",
            ])
            .output()
            .unwrap(),
    );
    let find_hash = f["documents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["path"] == "a.md")
        .unwrap()["document_hash"]
        .as_str()
        .unwrap();
    assert_eq!(find_hash, get_hash, "find and get agree on the hash");

    // default dump omits it (opt-in only) — output stays byte-identical.
    let d = json_of(
        &norn_cmd(&tmp)
            .args(["--cwd"])
            .arg(&vault)
            .args(["get", "a.md", "--format", "json"])
            .output()
            .unwrap(),
    );
    assert!(
        d[0].get("document_hash").is_none(),
        "default get omits document_hash: {d}"
    );
}

#[test]
fn get_single_target_json() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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

    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
fn get_all_cols_includes_body_content() {
    // `--body` is gone; body now comes via `--all-cols` (full dump).
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--all-cols", "--format", "json"])
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
    // Full structured dump: frontmatter + headings + links present; `.raw` not.
    assert!(v[0]["frontmatter"].is_object());
    assert!(v[0].get("headings").is_some());
    assert!(v[0].get("incoming_links").is_some());
    assert!(v[0].get("raw").is_none(), "all-cols excludes .raw");
}

#[test]
fn get_body_flag_is_removed() {
    // Breaking change: `--body` no longer exists; clap rejects it.
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--body"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--body should be an unknown flag");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--body") || stderr.contains("unexpected"),
        "expected unknown-flag error; got: {stderr}"
    );
}

#[test]
fn get_all_cols_conflicts_with_col() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "--all-cols", "--col", "type"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--all-cols + --col should conflict");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "expected conflict error; got: {stderr}"
    );
}

#[test]
fn get_sort_orders_records() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-sort-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\norder: 3\n---\n").unwrap();
    std::fs::write(root.join("b.md"), "---\norder: 1\n---\n").unwrap();
    std::fs::write(root.join("c.md"), "---\norder: 2\n---\n").unwrap();

    let run = |extra: &[&str]| -> Vec<String> {
        let mut args = vec!["get", "a.md", "b.md", "c.md", "--format", "jsonl"];
        args.extend_from_slice(extra);
        let out = norn_cmd(&tmp)
            .args(["--cwd"])
            .arg(&root)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["path"].as_str().unwrap().to_string()
            })
            .collect()
    };

    // Ascending by `order`.
    assert_eq!(run(&["--sort", "order"]), vec!["b.md", "c.md", "a.md"]);
    // Descending reverses.
    assert_eq!(
        run(&["--sort", "order", "--desc"]),
        vec!["a.md", "c.md", "b.md"]
    );
    // No --limit/--sort: all named targets, in the order given.
    assert_eq!(run(&[]), vec!["a.md", "b.md", "c.md"]);
    // --limit truncates (after sort).
    assert_eq!(
        run(&["--sort", "order", "--limit", "2"]),
        vec!["b.md", "c.md"]
    );
    // --starts-at offsets.
    assert_eq!(
        run(&["--sort", "order", "--starts-at", "2"]),
        vec!["c.md", "a.md"]
    );
}

#[test]
fn get_col_stem_facet_and_sort_by_stem() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-get-stem-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    // Subdirs so stem order (apple, banana, cherry) differs from path order
    // (a/…, m/…, z/…) — proving `.stem` and `--sort stem` key on the stem.
    std::fs::create_dir_all(root.join("z")).unwrap();
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::create_dir_all(root.join("m")).unwrap();
    std::fs::write(root.join("z/apple.md"), "---\ntype: note\n---\n").unwrap();
    std::fs::write(root.join("a/cherry.md"), "---\ntype: note\n---\n").unwrap();
    std::fs::write(root.join("m/banana.md"), "---\ntype: note\n---\n").unwrap();

    // `--col .stem`: the bare stem alongside the path identity.
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&root)
        .args(["get", "z/apple.md", "--col", ".stem", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(v[0]["path"], "z/apple.md");
    assert_eq!(v[0]["stem"], "apple");
    assert!(
        v[0].get("frontmatter").is_none(),
        "only the requested .stem facet should appear"
    );

    // Opt-in: default `get --format json` (no --col) carries no stem key.
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&root)
        .args(["get", "z/apple.md", "--format", "json"])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert!(
        v[0].get("stem").is_none(),
        "stem must be opt-in — absent from the default dump"
    );

    // `--sort stem`: stems ascending, distinct from path order.
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&root)
        .args([
            "get",
            "z/apple.md",
            "a/cherry.md",
            "m/banana.md",
            "--sort",
            "stem",
            "--format",
            "jsonl",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let paths: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["path"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(paths, vec!["z/apple.md", "m/banana.md", "a/cherry.md"]);
}

#[test]
fn get_unknown_col_warns_on_stderr() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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

    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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

    let out = norn_cmd(&tmp)
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

    let out = norn_cmd(&tmp)
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
    let out = norn_cmd(&tmp)
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

#[test]
fn get_markdown_single_doc_is_byte_faithful() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["get", "a.md", "--format", "markdown"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // stdout is the source file, verbatim — no count line, no record block.
    let disk = std::fs::read_to_string(vault.join("a.md")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), disk);
}

#[test]
fn get_markdown_multiple_targets_errors() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["get", "a.md", "b.md", "--format", "markdown"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected non-zero exit for >1 doc");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("single document") && stderr.contains("2 selected"),
        "expected limit-1 error; got: {stderr}"
    );
    // No document was printed.
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
}

#[test]
fn get_markdown_col_is_inert_and_warns() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["get", "a.md", "--col", "type", "--format", "markdown"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--col is ignored with --format markdown"),
        "expected col-ignored warning; got: {stderr}"
    );
    // --col had no effect: still the whole faithful document.
    let disk = std::fs::read_to_string(vault.join("a.md")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), disk);
}
