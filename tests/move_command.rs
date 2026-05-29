//! Integration tests for `vault move`.

use std::process::Command;
use tempfile::TempDir;

fn synth() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n[[b]]\n").unwrap();
    std::fs::write(root.join("b.md"), "---\ntype: note\n---\n# B\n").unwrap();
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
fn move_dry_run_prints_preview_and_exits_clean() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("norn move b.md → renamed.md"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        stdout.contains("1 backlink to rewrite across 1 file"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be moved"
    );
    assert!(
        !tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should not exist"
    );
}

#[test]
fn move_yes_applies_and_rewrites_backlinks() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("✓ moved b.md → renamed.md"),
        "unexpected stdout: {stdout}"
    );
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been moved"
    );
    assert!(
        tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should exist"
    );
    let a_content = std::fs::read_to_string(tmp.path().join("vault/a.md")).unwrap();
    assert!(
        a_content.contains("[[renamed]]"),
        "a.md should now reference renamed: {a_content}"
    );
}

#[test]
fn move_format_json_emits_envelope() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("output must parse as JSON");
    // ApplyReport shape (replaces MoveReport as of Plan Task 14)
    assert_eq!(v["schema_version"], 1);
    assert!(
        v["dry_run"].as_bool().unwrap_or(false),
        "dry_run should be true for implicit non-interactive"
    );
    let ops = v["operations"]
        .as_array()
        .expect("operations must be an array");
    assert_eq!(ops.len(), 1, "exactly one move_document op");
    assert_eq!(ops[0]["kind"], "move_document");
    let summary = ops[0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("b.md"),
        "summary should mention source: {summary}"
    );
    assert!(
        summary.contains("renamed.md"),
        "summary should mention destination: {summary}"
    );
    // --format json without --yes is implicitly non-interactive; file must not move
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be moved"
    );
}

#[test]
fn move_dry_run_format_json_emits_envelope() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args([
            "move",
            "b.md",
            "renamed.md",
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
    let trimmed = stdout.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
        panic!("--dry-run --format json must emit a JSON envelope: {e}\ngot: {trimmed}")
    });
    // ApplyReport shape (replaces MoveReport as of Plan Task 14)
    assert_eq!(v["schema_version"], 1);
    assert!(
        v["dry_run"].as_bool().unwrap_or(false),
        "dry_run must be true"
    );
    let ops = v["operations"]
        .as_array()
        .expect("operations must be an array");
    assert_eq!(ops.len(), 1, "exactly one move_document op");
    assert_eq!(ops[0]["kind"], "move_document");
    let summary = ops[0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("b.md"),
        "summary should mention source: {summary}"
    );
    assert!(
        summary.contains("renamed.md"),
        "summary should mention destination: {summary}"
    );
    // Dry-run must not mutate the filesystem.
    assert!(
        tmp.path().join("vault/b.md").exists(),
        "b.md should not be moved"
    );
    assert!(
        !tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should not exist"
    );
}

#[test]
fn move_destination_exists_refused() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn move_yes_format_json_emits_single_json_object() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "b.md", "renamed.md", "--yes", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The output must parse as a single JSON object, not two concatenated.
    let trimmed = String::from_utf8_lossy(&out.stdout);
    let trimmed = trimmed.trim();
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("output must be a single JSON object: {e}\ngot: {trimmed}"));
    // ApplyReport shape (replaces MoveReport as of Plan Task 14)
    assert_eq!(v["schema_version"], 1);
    // dry_run = false: the mutation was performed
    assert_eq!(
        v["dry_run"], false,
        "dry_run should be false after --yes apply"
    );
    // applied count = 1
    assert_eq!(v["applied"], 1, "applied count should be 1");
    let ops = v["operations"]
        .as_array()
        .expect("operations must be an array");
    assert_eq!(ops.len(), 1, "exactly one move_document op");
    assert_eq!(ops[0]["kind"], "move_document");
    assert_eq!(ops[0]["status"], "applied");
    // File must actually have moved
    assert!(
        !tmp.path().join("vault/b.md").exists(),
        "b.md should have been moved"
    );
    assert!(
        tmp.path().join("vault/renamed.md").exists(),
        "renamed.md should exist"
    );
}

#[test]
fn move_destination_exists_with_force_succeeds() {
    let tmp = synth();
    // Add a third file so the cascade has something to rewrite (c.md links to a.md).
    std::fs::write(
        tmp.path().join("vault/c.md"),
        "---\ntype: note\n---\n# C\n[[a]]\n",
    )
    .unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "b.md", "--force", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // a.md should be gone, b.md should exist (overwritten with a.md content)
    assert!(
        !tmp.path().join("vault/a.md").exists(),
        "a.md should have been moved"
    );
    assert!(tmp.path().join("vault/b.md").exists(), "b.md should exist");
}

#[test]
fn move_doc_with_self_reference_cascades_and_exits_clean() {
    // Regression for the 2026-05-27 atlas migration dogfood: when the moved
    // doc contains a wikilink to itself, Pass 3 used to try to read the doc
    // at its old path (Pass 2 had already moved it), error with "read
    // backlinker failed", abort the cascade, and surface as exit 1. With
    // classify_link_risk translating self-references to the new path, the
    // cascade rewrites the self-link in place and the move exits 0.
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-self-ref-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    // The moved doc references itself (twice, to test multi-occurrence).
    std::fs::write(
        root.join("vault-cli.md"),
        "---\ntype: note\n---\n# vault-cli\n\nThe [[vault-cli]] tool is a CLI.\nSee also [[vault-cli|the vault-cli root]] for context.\n",
    )
    .unwrap();
    // An external doc that also links to it.
    std::fs::write(
        root.join("intro.md"),
        "---\ntype: note\n---\n# Intro\n\nLearn more in [[vault-cli]].\n",
    )
    .unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["move", "vault-cli.md", "norn.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected exit 0, got {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    // External backlink rewritten.
    let intro = std::fs::read_to_string(root.join("intro.md")).unwrap();
    assert!(
        intro.contains("[[norn]]"),
        "intro.md should reference [[norn]]: {intro}"
    );
    assert!(
        !intro.contains("[[vault-cli]]"),
        "intro.md should no longer reference [[vault-cli]]: {intro}"
    );

    // Self-references in the moved doc also rewritten — this is the
    // regression the dogfood surfaced.
    let moved = std::fs::read_to_string(root.join("norn.md")).unwrap();
    assert!(
        moved.contains("[[norn]]"),
        "norn.md should have rewritten self-references to [[norn]]: {moved}"
    );
    assert!(
        moved.contains("[[norn|the vault-cli root]]"),
        "norn.md should preserve the display text in piped self-ref: {moved}"
    );
    assert!(
        !moved.contains("[[vault-cli]]") && !moved.contains("[[vault-cli|"),
        "norn.md should no longer reference [[vault-cli]]: {moved}"
    );
}

#[test]
fn move_cascade_covers_mixed_contexts_with_self_reference() {
    // Multi-context cascade completeness: backlinks in frontmatter, inline
    // body prose, list items, and a self-reference. All must rewrite.
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-cascade-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    // Source doc with a self-reference in its own body.
    std::fs::write(
        root.join("source.md"),
        "---\ntype: note\n---\n# Source\n\nA self-link: [[source]].\n",
    )
    .unwrap();
    // External docs with backlinks in varied contexts.
    std::fs::write(
        root.join("with_fm_link.md"),
        "---\ntype: note\nrelated: \"[[source]]\"\n---\n# Has frontmatter link\n",
    )
    .unwrap();
    std::fs::write(
        root.join("inline.md"),
        "---\ntype: note\n---\n# Inline\n\nProse with [[source]] inline.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("list.md"),
        "---\ntype: note\n---\n# List\n\n- bullet one\n- see [[source]]\n- bullet three\n",
    )
    .unwrap();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["move", "source.md", "renamed.md", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "expected exit 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for (file, label) in [
        ("with_fm_link.md", "frontmatter"),
        ("inline.md", "inline prose"),
        ("list.md", "list item"),
        ("renamed.md", "self-reference"),
    ] {
        let content = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(
            content.contains("[[renamed]]"),
            "{file} should reference [[renamed]] ({label} context): {content}"
        );
        assert!(
            !content.contains("[[source]]"),
            "{file} should no longer reference [[source]] ({label} context): {content}"
        );
    }
}

fn synth_folder_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-recursive-int-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir_all(root.join("src_dir/sub")).unwrap();
    std::fs::write(root.join("src_dir/a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(
        root.join("src_dir/sub/b.md"),
        "---\ntype: note\n---\n# B\n[[a]]\n",
    )
    .unwrap();
    tmp
}

#[test]
fn move_with_parents_creates_missing_dst_dirs() {
    let tmp = synth(); // existing synth() — single-vault with a.md + b.md
    let vault = tmp.path().join("vault");
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["move", "b.md", "deep/nested/new.md", "--parents", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(vault.join("deep/nested/new.md").exists());
}

#[test]
fn move_without_parents_refuses_when_dst_parent_missing() {
    let tmp = synth();
    let vault = tmp.path().join("vault");
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args(["move", "b.md", "deep/nested/new.md", "--yes"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "exit 2 when dst parent missing and --parents not set"
    );
}

#[test]
fn move_recursive_folder_rename() {
    let tmp = synth_folder_vault();
    let vault = tmp.path().join("vault");
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&vault)
        .args([
            "move",
            "src_dir",
            "dst_dir",
            "--recursive",
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
    assert!(vault.join("dst_dir/a.md").exists());
    assert!(vault.join("dst_dir/sub/b.md").exists());
    assert!(
        !vault.join("src_dir").exists() || vault.join("src_dir").read_dir().unwrap().count() == 0
    );
    let report: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout must be valid JSON");
    let moves: Vec<_> = report["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["kind"] == "move_document")
        .collect();
    assert_eq!(moves.len(), 2, "two .md files → two move_document ops");
}

// ---------------------------------------------------------------------------
// T1 — move JSON carries cascade counts on dry-run (no --verbose → no lists)
// ---------------------------------------------------------------------------

#[test]
fn move_json_cascade_dry_run_counts_present_no_rewrite_list() {
    // Seed: a.md is moved; d.md is the backlinker referencing a.md via [[a]].
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-cascade-t1-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n[[a]]\n").unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["move", "a.md", "b.md", "--dry-run", "--format", "json"])
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
    let move_op = ops
        .iter()
        .find(|o| o["kind"] == "move_document")
        .expect("move_document op not found");

    let cascade = &move_op["cascade"];
    assert!(
        !cascade.is_null(),
        "cascade must be present on move_document op"
    );

    // Dry-run forecast: planned == applied == 1, files == 1
    assert_eq!(
        cascade["planned"], 1,
        "planned must be 1 (d.md links to a.md)"
    );
    assert_eq!(
        cascade["applied"], 1,
        "applied must equal planned on dry-run (forecast)"
    );
    assert_eq!(cascade["files"], 1, "1 file contains the backlink");

    // Without --verbose, rewrites list must be absent or empty
    let rewrites = &cascade["rewrites"];
    assert!(
        rewrites.is_null() || rewrites.as_array().map(|a| a.is_empty()).unwrap_or(true),
        "rewrites must be absent/empty without --verbose; got: {rewrites}"
    );
}

// ---------------------------------------------------------------------------
// T2 — move JSON cascade on live apply (actuals match)
// ---------------------------------------------------------------------------

#[test]
fn move_json_cascade_live_apply_counts_actuals() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-cascade-t2-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n[[a]]\n").unwrap();

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args(["move", "a.md", "b.md", "--yes", "--format", "json"])
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
    let move_op = ops
        .iter()
        .find(|o| o["kind"] == "move_document")
        .expect("move_document op not found");

    let cascade = &move_op["cascade"];
    assert!(
        !cascade.is_null(),
        "cascade must be present on live move_document op"
    );
    assert_eq!(cascade["applied"], 1, "1 backlink rewritten on live apply");
    assert_eq!(cascade["skipped"], 0, "no skips expected");
    assert_eq!(cascade["files"], 1, "1 file contained the backlink");

    // File system state
    assert!(!root.join("a.md").exists(), "a.md should have been moved");
    assert!(root.join("b.md").exists(), "b.md should exist");
    // Backlink rewritten
    let d = std::fs::read_to_string(root.join("d.md")).unwrap();
    assert!(d.contains("[[b]]"), "d.md should now reference [[b]]: {d}");
}

// ---------------------------------------------------------------------------
// T3 — --verbose populates rewrites list; without it rewrites is absent/empty
// ---------------------------------------------------------------------------

#[test]
fn move_json_verbose_populates_rewrites_list() {
    let tmp = tempfile::Builder::new()
        .prefix("norn-move-cascade-t3-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.md"), "---\ntype: note\n---\n# A\n").unwrap();
    std::fs::write(root.join("d.md"), "---\ntype: note\n---\n# D\n[[a]]\n").unwrap();

    // With --verbose: rewrites list must be populated
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args([
            "--verbose",
            "move",
            "a.md",
            "b.md",
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
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("must parse as JSON: {e}\ngot: {}", stdout.trim()));

    let ops = v["operations"].as_array().expect("operations array");
    let move_op = ops
        .iter()
        .find(|o| o["kind"] == "move_document")
        .expect("move_document op not found");

    let cascade = &move_op["cascade"];
    let rewrites = cascade["rewrites"]
        .as_array()
        .expect("rewrites must be a non-null array with --verbose");
    assert_eq!(
        rewrites.len(),
        1,
        "exactly 1 rewrite expected (d.md); got: {rewrites:?}"
    );
    // The file field must name the backlinker
    let rewrite_file = rewrites[0]["file"].as_str().unwrap_or("");
    assert_eq!(
        rewrite_file, "d.md",
        "rewrite file must be d.md; got: {rewrite_file}"
    );
}

#[test]
#[cfg(unix)]
fn move_with_unwritable_backlinker_warns_but_exits_zero() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::Builder::new()
        .prefix("norn-move-unwritable-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");
    std::fs::create_dir(&root).unwrap();

    // target.md is the doc being moved.
    std::fs::write(root.join("target.md"), "---\ntype: note\n---\n# Target\n").unwrap();
    // linker.md contains a wikilink [[target]] — the stem form the move cascade
    // rewrites (same shape as a.md → [[b]] in the existing synth() tests).
    std::fs::write(
        root.join("linker.md"),
        "---\ntype: note\n---\n# Linker\n[[target]]\n",
    )
    .unwrap();

    // Make linker.md read-only so the cascade write fails (EACCES).
    let linker_path = root.join("linker.md");
    std::fs::set_permissions(&linker_path, std::fs::Permissions::from_mode(0o444)).unwrap();

    // Probe: if we can still open linker.md for write, we're running as root
    // (or some other context where unix perms are not enforced). Skip in that case.
    if std::fs::OpenOptions::new()
        .write(true)
        .open(&linker_path)
        .is_ok()
    {
        // Restore permissions before returning so tempdir cleanup works.
        std::fs::set_permissions(&linker_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }

    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(&root)
        .args([
            "move",
            "target.md",
            "renamed.md",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    // Restore perms before any assertions so tempdir cleanup always works.
    std::fs::set_permissions(&linker_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    // Primary move succeeded → exit 0.
    assert_eq!(
        out.status.code(),
        Some(0),
        "expected exit 0 (primary move succeeded); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The loud stderr warning must be present.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not be rewritten"),
        "stderr should warn about dangling backlinks; got: {stderr}"
    );

    // JSON output: cascade.failed == 1 and failures[0].reason == "write_failed".
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be valid JSON: {e}\ngot: {}", stdout.trim()));

    let ops = v["operations"]
        .as_array()
        .expect("operations must be an array");
    let move_op = ops
        .iter()
        .find(|o| o["kind"] == "move_document")
        .expect("move_document op not found in operations");

    let cascade = &move_op["cascade"];
    assert!(
        !cascade.is_null(),
        "cascade must be present on move_document op"
    );
    assert_eq!(
        cascade["failed"], 1,
        "cascade.failed should be 1 (linker.md write failed)"
    );
    let failures = cascade["failures"]
        .as_array()
        .expect("failures must be an array");
    assert_eq!(failures.len(), 1, "exactly one failure expected");
    assert_eq!(
        failures[0]["reason"], "write_failed",
        "failure reason should be write_failed"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn move_case_only_difference_refuses_same_path() {
    let tmp = synth();
    let out = Command::new(norn_bin())
        .args(["--cwd"])
        .arg(tmp.path().join("vault"))
        .args(["move", "a.md", "A.md"])
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "expected pre-flight refusal on case-only-different destination"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 (pre-flight refusal): {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("same canonical path") || stderr.contains("same path"),
        "stderr should mention same-path refusal: {stderr}"
    );
}
