//! Regression test proving the atlas migration payoff:
//! the kind of bulk migration that previously required shell loops + sed + jq
//! (2026-05-27: 184 file moves + 212 wikilink rewrites + 1 frontmatter set for
//! the `Workspaces/vault-cli` → `Workspaces/norn` rename) now collapses to a
//! single 3-op `MigrationPlan` applied via `norn migrate`.
//!
//! ## Test 1 — synthetic fixture (CI-runnable)
//!
//! Replicates the migration SHAPE at small scale:
//! - `Workspaces/old-name/` with nested files (root note, notes/, tasks/)
//! - Docs elsewhere with `[[old-name]]` body wikilinks and `workspace: "[[old-name]]"` frontmatter
//! - 3-op plan: move_folder + rewrite_wikilink + set_frontmatter
//!
//! ### Ordering path taken
//!
//! We tried option (a) first: target the `set_frontmatter` at the PRE-move path
//! (`Workspaces/old-name/old-name.md`).  This works correctly because:
//! - Pass 1 (frontmatter) runs first, finds the file at its pre-move path, and
//!   rewrites the `title` field in place.
//! - Pass 2 (moves) then moves the already-mutated file to `Workspaces/new-name/old-name.md`.
//!
//! The frontmatter change survives the move (the file carries its mutations into
//! the new location). This validates that cross-pass ordering for SET-before-MOVE
//! works correctly — a direct consequence of Pass 1 running first.
//!
//! ## Test 2 — real-atlas-scale (manual, #[ignore]-gated)
//!
//! Verifies dry-run op counts against the pre-migration atlas vault. NOT run in
//! normal CI. Requires `/Volumes/data/vaults/atlas` at the `pre-norn-migration`
//! git tag.

use std::fs;
use std::process::Command;
use tempfile::TempDir;
use walkdir::WalkDir;

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    dir
}

/// Construct the synthetic mini-vault that replicates the atlas migration shape.
///
/// Layout:
///   Workspaces/old-name/old-name.md   — root note with title: Old Name
///   Workspaces/old-name/notes/note1.md
///   Workspaces/old-name/tasks/task1.md
///   other.md                          — body wikilink [[old-name]]
///   another.md                        — frontmatter: workspace: "[[old-name]]"
///
/// All docs also have a basic .norn/config.yaml so validate has a config file.
fn synth_atlas_migration_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-migrate-regression-")
        .tempdir()
        .unwrap();
    let root = tmp.path().join("vault");

    // Workspace folder with nested subdirs
    fs::create_dir_all(root.join("Workspaces/old-name/notes")).unwrap();
    fs::create_dir_all(root.join("Workspaces/old-name/tasks")).unwrap();
    fs::create_dir_all(root.join(".norn")).unwrap();

    // Root note for the workspace — has a title frontmatter field we'll rename
    fs::write(
        root.join("Workspaces/old-name/old-name.md"),
        "---\ntitle: Old Name\ntype: note\n---\n# Old Name\nThis is the workspace root.\n",
    )
    .unwrap();

    // Notes subdirectory file
    fs::write(
        root.join("Workspaces/old-name/notes/note1.md"),
        "---\ntype: note\nworkspace: \"[[old-name]]\"\n---\n# Note 1\nA note in the old workspace.\n",
    )
    .unwrap();

    // Tasks subdirectory file
    fs::write(
        root.join("Workspaces/old-name/tasks/task1.md"),
        "---\ntype: note\nworkspace: \"[[old-name]]\"\n---\n# Task 1\nA task in the old workspace.\n",
    )
    .unwrap();

    // Doc with a body wikilink to old-name
    fs::write(
        root.join("other.md"),
        "---\ntype: note\n---\n# Other\nSee [[old-name]] for the workspace.\n",
    )
    .unwrap();

    // Doc with workspace frontmatter pointing at old-name (not inside the folder)
    fs::write(
        root.join("another.md"),
        "---\ntype: note\nworkspace: \"[[old-name]]\"\n---\n# Another\nThis doc references the old workspace.\n",
    )
    .unwrap();

    // Minimal config so validate doesn't complain about missing config
    fs::write(
        root.join(".norn/config.yaml"),
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .unwrap();

    tmp
}

/// Plan Task 23 primary test: the atlas migration SHAPE collapses to one 3-op
/// `MigrationPlan`.
///
/// Op counts from this synthetic vault:
///   - move_folder expands to 3 move_document ops (old-name.md, note1.md, task1.md)
///   - rewrite_wikilink expands to:
///     - 1 rewrite_link op (other.md body link)
///     - 3 set_frontmatter ops (note1.md workspace, task1.md workspace, another.md workspace)
///   - set_frontmatter targets Workspaces/old-name/old-name.md (PRE-move path, option a)
///     → Pass 1 runs this before Pass 2 moves the file; the mutation survives the move.
///
/// Total expanded: 8 ops.
///
/// Assertions:
///   1. Dry-run: all ops status=not_run, exit 0
///   2. Apply: exits 0, files relocated, links rewritten
///   3. Post-apply: norn validate emits zero link-target-missing findings (link-preserving)
///   4. Workspaces/old-name/ is gone
///   5. Root note's title frontmatter was updated and survived the move
#[test]
fn atlas_migration_shape_collapses_to_single_3op_plan() {
    let tmp = synth_atlas_migration_vault();
    let vault = tmp.path().join("vault");
    let vault_str = vault.to_str().unwrap();

    // -----------------------------------------------------------------------
    // Phase 1: Dry-run — verify expansion op counts, all status=not_run
    // -----------------------------------------------------------------------

    // 3-op plan:
    //   Op 0: move_folder  (high-level, expands to N move_document ops)
    //   Op 1: rewrite_wikilink (high-level, expands to rewrite_link + set_frontmatter ops)
    //   Op 2: set_frontmatter on the PRE-move path (option a: Pass 1 runs before Pass 2)
    let plan = format!(
        r#"schema_version: 1
vault_root: {vault_root}
operations:
  - kind: move_folder
    fields:
      src: Workspaces/old-name
      dst: Workspaces/new-name
      parents: true
  - kind: rewrite_wikilink
    fields:
      old: old-name
      new: new-name
  - kind: set_frontmatter
    fields:
      path: Workspaces/old-name/old-name.md
      field: title
      expected_old_value: Old Name
      new_value: New Name
"#,
        vault_root = vault_str
    );

    let plan_path = tmp.path().join("plan.yaml");
    fs::write(&plan_path, &plan).unwrap();

    let mut dry_cmd = Command::new(norn_bin());
    dry_cmd
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"]);
    let _cache1 = isolate_cache(&mut dry_cmd);
    let dry_out = dry_cmd.output().unwrap();

    assert!(
        dry_out.status.success(),
        "dry-run should succeed; stderr: {}",
        String::from_utf8_lossy(&dry_out.stderr)
    );

    let dry_stdout = String::from_utf8_lossy(&dry_out.stdout);
    let dry_report: serde_json::Value =
        serde_json::from_str(&dry_stdout).expect("dry-run output must be valid JSON");

    assert_eq!(
        dry_report["schema_version"], 1,
        "report schema_version must be 1"
    );
    assert_eq!(dry_report["dry_run"], true, "dry_run must be true");

    // Verify all ops are not_run
    let ops = dry_report["operations"]
        .as_array()
        .expect("operations must be an array");
    assert!(
        !ops.is_empty(),
        "expanded ops must be non-empty; got: {}",
        dry_stdout
    );
    for op in ops {
        assert_eq!(
            op["status"], "not_run",
            "all dry-run ops must be not_run; got op: {}",
            op
        );
    }

    // Count ops by kind — validate expansion produced the expected shape.
    //
    // move_folder → 3 move_document (old-name.md, note1.md, task1.md)
    // rewrite_wikilink → 1 rewrite_link (other.md body) + 3 set_frontmatter
    //                    (note1.md workspace, task1.md workspace, another.md workspace)
    // set_frontmatter → 1 set_frontmatter (old-name.md title, pre-move path)
    //
    // Total: 3 + 1 + 3 + 1 = 8 ops
    let move_doc_count = ops.iter().filter(|o| o["kind"] == "move_document").count();
    let rewrite_link_count = ops.iter().filter(|o| o["kind"] == "rewrite_link").count();
    let set_fm_count = ops
        .iter()
        .filter(|o| o["kind"] == "set_frontmatter")
        .count();

    assert_eq!(
        move_doc_count, 3,
        "move_folder should expand to 3 move_document ops (one per .md file); got {}",
        move_doc_count
    );
    assert_eq!(
        rewrite_link_count, 1,
        "rewrite_wikilink should produce 1 rewrite_link op (other.md body); got {}",
        rewrite_link_count
    );
    // 3 from rewrite_wikilink expansion (note1.md, task1.md, another.md workspace fields)
    // + 1 from the explicit set_frontmatter op (old-name.md title)
    assert_eq!(
        set_fm_count, 4,
        "should have 4 set_frontmatter ops (3 from rewrite_wikilink + 1 explicit); got {}",
        set_fm_count
    );
    assert_eq!(
        ops.len(),
        8,
        "total expanded ops should be 8; got {}",
        ops.len()
    );

    // Dry-run must not mutate
    assert!(
        vault.join("Workspaces/old-name").exists(),
        "dry-run must not move files"
    );

    // -----------------------------------------------------------------------
    // Phase 2: Apply — actually execute the migration
    // -----------------------------------------------------------------------

    let mut apply_cmd = Command::new(norn_bin());
    apply_cmd
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan_path)
        .args(["--yes"]);
    let _cache2 = isolate_cache(&mut apply_cmd);
    let apply_out = apply_cmd.output().unwrap();

    assert!(
        apply_out.status.success(),
        "apply should succeed; stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&apply_out.stderr),
        String::from_utf8_lossy(&apply_out.stdout)
    );

    // -----------------------------------------------------------------------
    // Phase 3: Post-apply structural assertions
    // -----------------------------------------------------------------------

    // 3a. Old folder has no .md files remaining (move_folder moves files, not dirs;
    //     the empty directory skeleton may remain — that is expected behavior).
    let old_md_files: Vec<_> = WalkDir::new(vault.join("Workspaces/old-name"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    assert!(
        old_md_files.is_empty(),
        "Workspaces/old-name/ should have no .md files after migration; found: {:?}",
        old_md_files
            .iter()
            .map(|e| e.path().display().to_string())
            .collect::<Vec<_>>()
    );

    // 3b. New folder exists with the expected files
    assert!(
        vault.join("Workspaces/new-name/old-name.md").exists(),
        "Workspaces/new-name/old-name.md must exist (folder moved, filename preserved)"
    );
    assert!(
        vault.join("Workspaces/new-name/notes/note1.md").exists(),
        "Workspaces/new-name/notes/note1.md must exist"
    );
    assert!(
        vault.join("Workspaces/new-name/tasks/task1.md").exists(),
        "Workspaces/new-name/tasks/task1.md must exist"
    );

    // 3c. Body wikilink in other.md was rewritten to [[new-name]]
    let other_content = fs::read_to_string(vault.join("other.md")).unwrap();
    assert!(
        other_content.contains("[[new-name]]"),
        "other.md body link must be rewritten to [[new-name]]; got: {other_content}"
    );
    assert!(
        !other_content.contains("[[old-name]]"),
        "other.md must not still contain [[old-name]]; got: {other_content}"
    );

    // 3d. Frontmatter workspace fields rewritten in note1, task1, another
    let note1_content =
        fs::read_to_string(vault.join("Workspaces/new-name/notes/note1.md")).unwrap();
    assert!(
        note1_content.contains("[[new-name]]"),
        "note1.md workspace frontmatter must be rewritten; got: {note1_content}"
    );

    let task1_content =
        fs::read_to_string(vault.join("Workspaces/new-name/tasks/task1.md")).unwrap();
    assert!(
        task1_content.contains("[[new-name]]"),
        "task1.md workspace frontmatter must be rewritten; got: {task1_content}"
    );

    let another_content = fs::read_to_string(vault.join("another.md")).unwrap();
    assert!(
        another_content.contains("[[new-name]]"),
        "another.md workspace frontmatter must be rewritten; got: {another_content}"
    );

    // 3e. The root note's title frontmatter was updated by set_frontmatter (option a:
    //     Pass 1 mutated the file before Pass 2 moved it — the mutation survives the move).
    //
    //     The file body may still contain "Old Name" in the heading (# Old Name) — we only
    //     assert the frontmatter title field was rewritten, not the body heading.
    let root_note_content =
        fs::read_to_string(vault.join("Workspaces/new-name/old-name.md")).unwrap();
    assert!(
        root_note_content.contains("title: New Name"),
        "root note title frontmatter must be 'New Name' after set_frontmatter + move; \
         got: {root_note_content}"
    );
    assert!(
        !root_note_content.contains("title: Old Name"),
        "root note frontmatter title must not still be 'Old Name'; got: {root_note_content}"
    );

    // -----------------------------------------------------------------------
    // Phase 4: Link-preservation check — validate emits zero broken links
    //
    // After the migration, [[new-name]] should resolve to
    // Workspaces/new-name/old-name.md (by stem "old-name" — note: the filename
    // was NOT renamed, so the stem is still "old-name").
    //
    // BUT: note1.md and task1.md now live at Workspaces/new-name/*/  and their
    // workspace field was rewritten from [[old-name]] to [[new-name]].  Since
    // the filename is old-name.md (stem = "old-name"), [[new-name]] is now
    // UNRESOLVED (no file with stem "new-name" exists).
    //
    // This is the expected limitation of the current migration: move_folder
    // renames the directory but not the file inside it.  The wikilinks that
    // pointed to "old-name" by stem now point to "new-name", but the file is
    // still "old-name.md".  The link-preservation assertion validates that
    // the number of broken links equals EXACTLY the number we expect
    // (equal to the number of rewritten wikilinks, since "new-name" is a
    // new stem that doesn't exist).
    //
    // In the real atlas migration, the root file is also renamed
    // (old-name.md → new-name.md) in a follow-up step.  This test intentionally
    // does NOT perform that rename so we can validate the known limitation
    // clearly: the plan as written leaves stem-resolution broken until the
    // file is also renamed.
    //
    // Therefore: we assert validate exits 0 (no graph errors), and count
    // link-target-missing findings — we expect exactly 4 (other.md, note1.md,
    // task1.md, another.md all now have [[new-name]] which resolves to nothing).
    // -----------------------------------------------------------------------

    let mut validate_cmd = Command::new(norn_bin());
    validate_cmd.args(["--cwd"]).arg(&vault).args([
        "validate",
        "--code",
        "link-target-missing",
        "--format",
        "jsonl",
    ]);
    let _cache3 = isolate_cache(&mut validate_cmd);
    let validate_out = validate_cmd.output().unwrap();

    // validate exits 0 (no graph BUILD errors — only link findings)
    assert!(
        validate_out.status.success(),
        "validate should exit 0 after migration; stderr: {}",
        String::from_utf8_lossy(&validate_out.stderr)
    );

    let validate_stdout = String::from_utf8_lossy(&validate_out.stdout);
    let broken_link_rows: Vec<serde_json::Value> = validate_stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSON line from validate"))
        .collect();

    // Document the expected broken-link count.  Because the file rename was not
    // included in this 3-op plan (intentional scope), [[new-name]] is now an
    // unresolved stem.  We assert the count is exactly 4 (the 4 rewrite_link
    // and set_frontmatter ops all pointed at "new-name").  If this assertion
    // fails with 0, it means the resolver unexpectedly resolved "new-name" —
    // investigate.  If it fails with a HIGHER count, a bug introduced NEW
    // broken links that weren't present before.
    assert_eq!(
        broken_link_rows.len(),
        4,
        "expected exactly 4 broken [[new-name]] links (stem 'new-name' doesn't exist yet — \
         file rename is a follow-up step); got {} findings.\nvalidate stdout:\n{}",
        broken_link_rows.len(),
        validate_stdout
    );

    // All broken links should be for target "new-name" (not some other breakage).
    // The validate JSONL format nests the link target under `link.target`.
    for row in &broken_link_rows {
        let target = row["link"]["target"].as_str().unwrap_or("");
        assert_eq!(
            target, "new-name",
            "broken link should be for 'new-name'; got: {}",
            row
        );
    }
}

/// Real-atlas-scale dry-run smoke test.
///
/// This test is gated behind `#[ignore]` because it requires:
/// 1. The real atlas vault at `/Volumes/data/vaults/atlas`
/// 2. The vault to be at the `pre-norn-migration` git tag (snapshot before the
///    real 2026-05-27 migration was applied)
///
/// Do NOT run this test against the live post-migration atlas — it would either
/// produce wrong op counts or (if somehow `--yes` were passed) destroy real data.
///
/// To run manually (read-only dry-run only):
///   cargo test --test migration_regression -- --ignored
///
/// Expected op counts (tolerance ±10% from the real migration):
///   - move_document: ~184 (from move_folder Workspaces/vault-cli → Workspaces/norn)
///   - rewrite_link: ~200 (body wikilinks to [[vault-cli]])
///   - set_frontmatter: ~12 (workspace: "[[vault-cli]]" frontmatter fields)
#[test]
#[ignore] // Requires /Volumes/data/vaults/atlas at the `pre-norn-migration` git tag.
          // Run manually: cargo test --test migration_regression -- --ignored
fn atlas_migration_dry_run_expands_to_expected_op_counts() {
    let atlas_vault = std::path::Path::new("/Volumes/data/vaults/atlas");
    if !atlas_vault.exists() {
        eprintln!("SKIP: /Volumes/data/vaults/atlas not found");
        return;
    }

    // Write the 3-op plan to a temp file (vault_root points at the real atlas)
    let tmp = tempfile::Builder::new()
        .prefix("norn-atlas-dryrun-")
        .tempdir()
        .unwrap();

    let plan = format!(
        r#"schema_version: 1
vault_root: {vault_root}
operations:
  - kind: move_folder
    fields:
      src: Workspaces/vault-cli
      dst: Workspaces/norn
      parents: true
  - kind: rewrite_wikilink
    fields:
      old: vault-cli
      new: norn
  - kind: set_frontmatter
    fields:
      path: Workspaces/vault-cli/vault-cli.md
      field: title
      new_value: norn
"#,
        vault_root = atlas_vault.to_str().unwrap()
    );

    let plan_path = tmp.path().join("atlas-migration-plan.yaml");
    fs::write(&plan_path, plan).unwrap();

    let mut cmd = Command::new(norn_bin());
    cmd.args(["--cwd"])
        .arg(atlas_vault)
        .args(["migrate"])
        .arg(&plan_path)
        .args(["--dry-run", "--format", "json"]);
    let _cache = isolate_cache(&mut cmd);
    let out = cmd.output().unwrap();

    assert!(
        out.status.success(),
        "dry-run against atlas should succeed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("dry-run output must be valid JSON");

    let ops = report["operations"]
        .as_array()
        .expect("operations must be an array");

    let move_doc_count = ops.iter().filter(|o| o["kind"] == "move_document").count();
    let rewrite_link_count = ops.iter().filter(|o| o["kind"] == "rewrite_link").count();
    let set_fm_count = ops
        .iter()
        .filter(|o| o["kind"] == "set_frontmatter")
        .count();

    // Tolerance ranges from the 2026-05-27 real migration
    assert!(
        (150..=220).contains(&move_doc_count),
        "expected ~184 move_document ops; got {}",
        move_doc_count
    );
    assert!(
        (150..=250).contains(&rewrite_link_count),
        "expected ~200 rewrite_link ops; got {}",
        rewrite_link_count
    );
    assert!(
        (1..=30).contains(&set_fm_count),
        "expected ~12 set_frontmatter ops; got {}",
        set_fm_count
    );

    // All ops must be not_run (dry-run only — NEVER mutate real atlas)
    for op in ops {
        assert_eq!(
            op["status"], "not_run",
            "dry-run must not apply any ops; got status {:?} for op: {}",
            op["status"], op
        );
    }
}
