//! Integration tests for `vault delete`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-delete-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    // Minimal vault config required by build_index.
    std::fs::create_dir(root.join(".norn")).unwrap();
    std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
    std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
    std::fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n").unwrap();
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

/// NRN-229: a `--format json` PREFLIGHT refusal emits the structured
/// `{code, message}` envelope on stdout (exit 2) carrying the stable code —
/// matching the `set` NRN-221 contract — instead of format-blind stderr prose.
#[test]
fn delete_preflight_refusal_format_json_emits_coded_envelope() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    // b.md has an incoming link from a.md ([[b]]); no --allow-broken-links /
    // --rewrite-to → backlinks-present refusal.
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["delete", "b.md", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "preflight refusal → exit 2");
    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout must be the JSON error envelope");
    assert_eq!(
        envelope["code"], "backlinks-present",
        "envelope: {envelope}"
    );
    assert!(
        vault.join("b.md").exists(),
        "a refused delete must not remove the file"
    );
}

/// NRN-229: the DEFAULT (Records) preflight refusal is byte-identical to before
/// — prose on stderr, no JSON on stdout.
#[test]
fn delete_preflight_refusal_records_stays_prose_on_stderr() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(&vault)
        .args(["delete", "b.md"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "Records refusal must not emit JSON on stdout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("error: document has 1 incoming link"),
        "stderr prose must be unchanged: {stderr}"
    );
}

#[test]
fn delete_leaf_dry_run_no_op() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        tmp.path().join("vault/d.md").exists(),
        "d.md should not be deleted on dry-run"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("norn delete d.md"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn delete_leaf_yes_removes_file() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/d.md").exists(),
        "d.md should have been deleted"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("✓ deleted d.md"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn delete_with_incoming_links_refused() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--yes"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be deleted when refused"
    );
}

#[test]
fn delete_with_allow_broken_links_succeeds() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--yes", "--allow-broken-links"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );
    // a.md should still have its (now broken) link to b.
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(a.contains("[[b]]"), "a.md link should remain (broken): {a}");
}

#[test]
fn delete_with_rewrite_to_redirects_backlinks() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--yes", "--rewrite-to", "c.md"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(a.contains("[[c]]"), "a.md should now reference c: {a}");
    assert!(
        !a.contains("[[b]]"),
        "a.md should no longer reference b: {a}"
    );
}

#[test]
fn delete_yes_format_json_emits_single_json_object() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--yes", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The output must parse as a single JSON object (ApplyReport), not two concatenated.
    let trimmed = String::from_utf8_lossy(&out.stdout);
    let trimmed = trimmed.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("output must be a single JSON object: {e}\ngot: {trimmed}"));
    // ApplyReport shape: schema_version, dry_run, applied count, operations[].
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["dry_run"], false);
    // applied count = 1: the delete_document op was executed.
    assert_eq!(v["applied"], 1);
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("d.md"),
        "summary should mention d.md: {:?}",
        v["operations"][0]["summary"]
    );
    // File must actually have been deleted
    assert!(
        !tmp.path().join("vault/d.md").exists(),
        "d.md should have been deleted"
    );
}

#[test]
fn delete_dry_run_format_json_emits_envelope() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "d.md", "--dry-run", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("--dry-run --format json must emit a JSON envelope: {e}\ngot: {trimmed}")
    });
    // ApplyReport shape.
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("d.md"),
        "summary should mention d.md: {:?}",
        v["operations"][0]["summary"]
    );
    // Dry-run must not mutate the filesystem.
    assert!(
        tmp.path().join("vault/d.md").exists(),
        "d.md should not be deleted on dry-run"
    );
}

// ---------------------------------------------------------------------------
// T4 — delete --rewrite-to cascade counts in JSON output
// ---------------------------------------------------------------------------

#[test]
fn delete_rewrite_to_cascade_counts_in_json() {
    // Vault seeded by synth(): a.md has [[b]], b.md exists, c.md exists.
    // Delete b.md --rewrite-to c.md → the backlink in a.md should be
    // redirected to c.md; cascade.applied == 1, cascade.files == 1.
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "delete",
            "b.md",
            "--yes",
            "--rewrite-to",
            "c.md",
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
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));

    let ops = v["operations"].as_array().expect("operations array");
    let del_op = ops
        .iter()
        .find(|o| o["kind"] == "delete_document")
        .unwrap_or_else(|| panic!("delete_document op not found in: {ops:?}"));

    let cascade = &del_op["cascade"];
    assert!(
        !cascade.is_null(),
        "cascade must be present on delete_document op with --rewrite-to"
    );
    // a.md has 1 backlink to b.md that was redirected
    assert_eq!(
        cascade["applied"], 1,
        "1 backlink redirect applied; cascade: {cascade}"
    );
    assert_eq!(cascade["files"], 1, "1 file contained the backlink");

    // NRN-237: the index-derived link_impact rides the report — one incoming
    // backlink from a.md, and the raw `c.md` redirect resolved to `c.md`.
    let li = &del_op["link_impact"];
    assert!(
        !li.is_null(),
        "link_impact must be present on delete_document op: {del_op}"
    );
    assert_eq!(
        li["incoming_total"], 1,
        "1 incoming backlink; link_impact: {li}"
    );
    assert_eq!(
        li["incoming_files"].as_array().unwrap(),
        &vec![serde_json::json!("a.md")],
        "incoming_files must list a.md; link_impact: {li}"
    );
    assert_eq!(
        li["redirect_to"], "c.md",
        "redirect_to must be the resolved rewrite target; link_impact: {li}"
    );

    // Verify filesystem + content mutations
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(a.contains("[[c]]"), "a.md should now reference c: {a}");
}

#[test]
fn delete_format_json_emits_envelope() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "b.md", "--allow-broken-links", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("output must parse as JSON");
    // ApplyReport shape: --format json without --yes is implicitly dry-run.
    assert_eq!(v["schema_version"], 2);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("b.md"),
        "summary should mention b.md: {:?}",
        v["operations"][0]["summary"]
    );
    // NRN-237: link_impact rides even the allow-broken-links dry-run report —
    // one incoming backlink from a.md, no redirect target.
    let li = &v["operations"][0]["link_impact"];
    assert_eq!(
        li["incoming_total"], 1,
        "1 incoming backlink; link_impact: {li}"
    );
    assert_eq!(
        li["incoming_files"].as_array().unwrap(),
        &vec![serde_json::json!("a.md")],
        "incoming_files must list a.md; link_impact: {li}"
    );
    assert!(
        li.get("redirect_to").is_none(),
        "redirect_to omitted without --rewrite-to; link_impact: {li}"
    );
    // --format json without --yes is implicitly non-interactive; file must not be deleted.
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be deleted when using --format json without --yes"
    );
}

// ---------------------------------------------------------------------------
// NRN-57 — `norn delete <stem>` must resolve a bare stem the same way
// `norn move <stem>` does, instead of building the migration plan from the
// raw unresolved CLI arg (which previously produced a misleading
// "repair plan targets a document not in the index" exit-2 error).
// ---------------------------------------------------------------------------

#[test]
fn delete_stem_dry_run_resolves_target() {
    // c.md has no incoming links, so this exercises pure stem resolution
    // without tripping the incoming-links refusal path.
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "c", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stem-addressed delete --dry-run should resolve, not error; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        tmp.path().join("vault/c.md").exists(),
        "c.md should not be deleted on dry-run"
    );
}

#[test]
fn delete_stem_yes_removes_resolved_file() {
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "c", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stem-addressed delete --yes should resolve and delete; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/c.md").exists(),
        "c.md should have been deleted via stem addressing"
    );
}

#[test]
fn delete_stem_yes_format_json_plans_resolved_path() {
    // The JSON operation summary/path must reflect the RESOLVED vault-relative
    // path (c.md), not the raw stem arg ("c") that was passed on the CLI.
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "c", "--yes", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));
    assert_eq!(v["operations"][0]["kind"], "delete_document");
    assert!(
        v["operations"][0]["summary"]
            .as_str()
            .unwrap()
            .contains("c.md"),
        "summary should mention the resolved path c.md, not the raw stem: {:?}",
        v["operations"][0]["summary"]
    );
    assert!(
        !tmp.path().join("vault/c.md").exists(),
        "c.md should have been deleted"
    );
}

#[test]
fn delete_full_path_still_works_after_stem_fix() {
    // Regression guard: full vault-relative path addressing must keep working
    // once stem resolution feeds the migration plan.
    let tmp = synth();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["delete", "c.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !tmp.path().join("vault/c.md").exists(),
        "c.md should have been deleted via full-path addressing"
    );
}

// ---------------------------------------------------------------------------
// NRN-248 — the applier's `build_link_impact` fallback branch fires when
// `backlinks()` (resolution-keyed) misses a link that `link_risk`'s textual
// fallback (`link_targets_path`) still catches: `resolved_path == None` links
// that are either an ambiguous same-stem wikilink or a dangling relative
// markdown link whose raw target textually coincides with the deleted path.
// ---------------------------------------------------------------------------

/// Class 1: `x/b.md` and `y/b.md` share the stem `b`; `a.md`'s bare `[[b]]`
/// wikilink is therefore Ambiguous (`resolved_path == None`), invisible to
/// `backlinks()`. `link_risk`'s textual fallback still matches it as a
/// stem-only link once `resolved_path` is None.
fn synth_ambiguous_stem() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-delete-int-ambig-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join(".norn")).unwrap();
    std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
    std::fs::create_dir(root.join("x")).unwrap();
    std::fs::create_dir(root.join("y")).unwrap();
    std::fs::write(root.join("x/b.md"), "---\ntype: note\n---\n# B in x\n").unwrap();
    std::fs::write(root.join("y/b.md"), "---\ntype: note\n---\n# B in y\n").unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
    tmp
}

/// Class 2: `sub/note.md` contains a relative markdown link `[x](b.md)`,
/// which resolves relative to `sub/` (looking for `sub/b.md`) and is
/// therefore dangling (`resolved_path == None`) even though its raw target
/// textually equals the root-level `b.md` being deleted.
fn synth_dangling_markdown() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-delete-int-dangling-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(root.join(".norn")).unwrap();
    std::fs::write(root.join(".norn/config.yaml"), "validate: {}\n").unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
    std::fs::write(
        root.join("sub/note.md"),
        "---\ntype: note\n---\n# Note\n[x](b.md)\n",
    )
    .unwrap();
    std::fs::write(root.join("c.md"), "---\ntype: note\n---\n# C\n").unwrap();
    tmp
}

#[test]
fn rewrite_to_fallback_fires_on_ambiguous_stem_backlink() {
    let tmp = synth_ambiguous_stem();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "delete",
            "x/b.md",
            "--rewrite-to",
            "c.md",
            "--yes",
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
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));

    let del_op = v["operations"]
        .as_array()
        .expect("operations array")
        .iter()
        .find(|o| o["kind"] == "delete_document")
        .expect("delete_document op present");

    // The fallback-only shape: no resolved backlink (a.md's [[b]] is
    // Ambiguous, so backlinks() misses it), yet incoming_files is non-empty
    // because link_risk's textual fallback still caught it as a stem_link.
    let li = &del_op["link_impact"];
    assert_eq!(
        li["incoming_total"], 0,
        "no resolved backlink; link_impact: {li}"
    );
    assert_eq!(
        li["incoming_files"].as_array().unwrap(),
        &vec![serde_json::json!("a.md")],
        "incoming_files must come from the link_risk fallback; link_impact: {li}"
    );
    assert_eq!(
        li["redirect_to"], "c.md",
        "redirect_to must be the resolved rewrite target; link_impact: {li}"
    );

    // Filesystem: x/b.md gone, the unrelated same-stem y/b.md untouched.
    assert!(
        !tmp.path().join("vault/x/b.md").exists(),
        "x/b.md should have been deleted"
    );
    assert!(
        tmp.path().join("vault/y/b.md").exists(),
        "y/b.md (same-stem sibling, not the delete target) must be untouched"
    );

    // What did the cascade do to a.md's body? Observed and pinned, not assumed:
    // apply_link_rewrites iterates the SAME link_risk stem_links/markdown_links
    // used by the link_impact fallback and does a literal raw->rewritten
    // string-replace, so the ambiguous [[b]] wikilink DOES get rewritten to
    // [[c]] despite being invisible to backlinks()/preflight.
    let a = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert_eq!(
        a, "---\ntype: note\n---\n# A\n[[c]]\n",
        "a.md's ambiguous [[b]] wikilink was rewritten to [[c]] by the cascade: {a}"
    );
}

#[test]
fn rewrite_to_fallback_fires_on_dangling_markdown_link() {
    let tmp = synth_dangling_markdown();
    let out = norn_cmd(&tmp)
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "delete",
            "b.md",
            "--rewrite-to",
            "c.md",
            "--yes",
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
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));

    let del_op = v["operations"]
        .as_array()
        .expect("operations array")
        .iter()
        .find(|o| o["kind"] == "delete_document")
        .expect("delete_document op present");

    let li = &del_op["link_impact"];
    assert_eq!(
        li["incoming_total"], 0,
        "sub/note.md's [x](b.md) is dangling (resolves relative to sub/), so \
         backlinks() sees no resolved backlink; link_impact: {li}"
    );
    assert_eq!(
        li["incoming_files"].as_array().unwrap(),
        &vec![serde_json::json!("sub/note.md")],
        "incoming_files must come from the link_risk fallback; link_impact: {li}"
    );
    assert_eq!(
        li["redirect_to"], "c.md",
        "redirect_to must be the resolved rewrite target; link_impact: {li}"
    );

    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been deleted"
    );

    // What did the cascade do to sub/note.md's body? Observed and pinned:
    // the raw markdown href "b.md" textually coincides with the deleted
    // path, so link_risk's markdown_links fallback catches it and
    // apply_link_rewrites rewrites it to the relative path to c.md.
    let note = std::fs::read_to_string(tmp.path().join("vault/sub/note.md")).unwrap();
    assert_eq!(
        note, "---\ntype: note\n---\n# Note\n[x](../c.md)\n",
        "sub/note.md's dangling [x](b.md) link was rewritten by the cascade: {note}"
    );
}
