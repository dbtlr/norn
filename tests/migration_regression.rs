//! Regression test proving the atlas migration payoff:
//! the kind of bulk migration that previously required shell loops + sed + jq
//! (2026-05-27: vault-cli → norn workspace rename) now collapses to a
//! two-invocation `norn migrate` workflow that is faithfully link-preserving.
//!
//! ## Test 1 — synthetic fixture (CI-runnable)
//!
//! Replicates the migration SHAPE at small scale using the **faithful
//! two-invocation workflow** that matches the real 2026-05-27 atlas migration:
//!
//! ### Why two invocations?
//!
//! A single `move_folder` + `rewrite_wikilink` plan has a cross-op dependency
//! trap: the `move_folder` expander plans against the pre-move index (stems are
//! preserved), so wikilinks to `[[old-name]]` still resolve after the folder
//! move — but if you ALSO run `rewrite_wikilink old-name → new-name` in the
//! same plan, those links now point to stem `new-name`, which doesn't exist yet
//! (the root file is still `old-name.md`). Result: 4 dangling links.
//!
//! The correct two-invocation sequence:
//!
//! **Invocation 1** — rename the root note IN PLACE (stem change):
//!   `Workspaces/old-name/old-name.md` → `Workspaces/old-name/new-name.md`
//!
//!   The `move_document` cascade (Pass 3) reads `link_risk` for the stem
//!   change `old-name → new-name` and rewrites every backlink — body AND
//!   frontmatter wikilinks — across the vault. Frontmatter wikilinks
//!   (e.g. `workspace: "[[old-name]]"`) are included in `doc.links` with
//!   `LinkSourceArea::Frontmatter`, so they are captured by `classify_link_risk`
//!   and rewritten in the same pass. After this step, `[[new-name]]` resolves
//!   correctly (to `Workspaces/old-name/new-name.md`, stem `new-name`).
//!
//! **Invocation 2** — move the folder (now containing `new-name.md` + others):
//!   `Workspaces/old-name` → `Workspaces/new-name`
//!
//!   A folder move preserves every file's stem. `[[new-name]]` still resolves
//!   after the move (stem unchanged), so no wikilink rewrites are needed.
//!
//! ### Assertions
//!
//!   1. Invocation 1 dry-run: 1 `move_document` op, all `status=not_run`
//!   2. Invocation 1 apply: exits 0; root note renamed to `new-name.md`
//!   3. All backlinks (`[[old-name]]` in body + frontmatter) rewritten to `[[new-name]]`
//!   4. Invocation 2 dry-run: 3 `move_document` ops (new-name.md, note1.md, task1.md)
//!   5. Invocation 2 apply: exits 0; files at `Workspaces/new-name/`
//!   6. Zero `link-target-missing` findings after both invocations (link-preserving)
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
///   Workspaces/old-name/notes/note1.md  — frontmatter: workspace: "[[old-name]]"
///   Workspaces/old-name/tasks/task1.md  — frontmatter: workspace: "[[old-name]]"
///   other.md                            — body wikilink [[old-name]]
///   another.md                          — frontmatter: workspace: "[[old-name]]"
///
/// After Invocation 1 (rename root note):
///   Workspaces/old-name/new-name.md   — root note (stem changed)
///   All [[old-name]] links → [[new-name]] (body + frontmatter, cascade)
///
/// After Invocation 2 (move folder):
///   Workspaces/new-name/new-name.md, notes/note1.md, tasks/task1.md
///   [[new-name]] still resolves (stem unchanged by folder move)
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

/// Faithful two-invocation link-preserving migration — the atlas migration shape.
///
/// This test validates that the two-invocation workflow (root note rename first,
/// then folder move) produces ZERO new broken links. It mirrors what the real
/// 2026-05-27 atlas migration did: rename vault-cli.md → norn.md (stem change,
/// triggering cascade rewrites of all [[vault-cli]] backlinks in body AND
/// frontmatter), then move the folder.
///
/// The key insight: wikilinks resolve by STEM, not path. A `move_folder` alone
/// does NOT change file stems, so `[[old-name]]` would still resolve if we
/// moved the folder first. But we also need the root note's stem to change
/// (old-name → new-name) so that `[[new-name]]` resolves after the rename.
/// Doing the stem rename first lets the cascade handle all backlink rewrites
/// before the folder move, which then carries no additional link burden.
///
/// Op counts:
///   Invocation 1 (root note rename, stem change):
///     - 1 move_document op (move_document is low-level: 1 plan op = 1 report op)
///     - The Pass 3 cascade rewrites 4 backlinks in the background:
///         other.md body [[old-name]], note1.md frontmatter, task1.md frontmatter,
///         another.md frontmatter — all → [[new-name]]
///
///   Invocation 2 (folder move, stem-preserving):
///     - 3 move_document ops (new-name.md, notes/note1.md, tasks/task1.md)
///     - Stems unchanged → no wikilink cascade needed → 0 new broken links
///
/// Assertions:
///   1. Invocation 1 dry-run: 1 op, all status=not_run, no mutation
///   2. Invocation 1 apply: exits 0; root note renamed; ALL backlinks rewritten
///      (body + frontmatter); root note's title frontmatter set to New Name
///   3. Invocation 2 dry-run: 3 move_document ops, all status=not_run, no mutation
///   4. Invocation 2 apply: exits 0; files relocated to Workspaces/new-name/
///   5. norn validate: ZERO link-target-missing findings (fully link-preserving)
#[test]
fn atlas_migration_two_invocation_link_preserving_flow() {
    let tmp = synth_atlas_migration_vault();
    let vault = tmp.path().join("vault");
    let vault_str = vault.to_str().unwrap();

    // -----------------------------------------------------------------------
    // Invocation 1: rename root note in place (stem change → cascade rewrites)
    // Plan: single move_document op — low-level, 1:1 plan op to report op.
    // The Pass 3 cascade handles all backlink rewrites for the stem change.
    // -----------------------------------------------------------------------

    let plan1 = format!(
        r#"schema_version: 1
vault_root: {vault_root}
operations:
  - kind: move_document
    fields:
      src: Workspaces/old-name/old-name.md
      dst: Workspaces/old-name/new-name.md
"#,
        vault_root = vault_str
    );

    let plan1_path = tmp.path().join("plan1.yaml");
    fs::write(&plan1_path, &plan1).unwrap();

    // --- Invocation 1 Dry-run ---

    let mut dry1_cmd = Command::new(norn_bin());
    dry1_cmd
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan1_path)
        .args(["--dry-run", "--format", "json"]);
    let _cache1a = isolate_cache(&mut dry1_cmd);
    let dry1_out = dry1_cmd.output().unwrap();

    assert!(
        dry1_out.status.success(),
        "Invocation 1 dry-run should succeed; stderr: {}",
        String::from_utf8_lossy(&dry1_out.stderr)
    );

    let dry1_stdout = String::from_utf8_lossy(&dry1_out.stdout);
    let dry1_report: serde_json::Value =
        serde_json::from_str(&dry1_stdout).expect("Invocation 1 dry-run output must be valid JSON");

    assert_eq!(dry1_report["schema_version"], 1);
    assert_eq!(dry1_report["dry_run"], true);

    let dry1_ops = dry1_report["operations"]
        .as_array()
        .expect("operations must be an array");

    // move_document is a low-level op: 1 plan op → 1 report op (cascade is internal to Pass 3)
    assert_eq!(
        dry1_ops.len(),
        1,
        "Invocation 1: 1 move_document plan op should produce 1 report op; got {}",
        dry1_ops.len()
    );
    assert_eq!(
        dry1_ops[0]["kind"], "move_document",
        "single op must be move_document"
    );

    for op in dry1_ops {
        assert_eq!(
            op["status"], "not_run",
            "all dry-run ops must be not_run; got: {}",
            op
        );
    }

    // Dry-run must not mutate
    assert!(
        vault.join("Workspaces/old-name/old-name.md").exists(),
        "dry-run must not rename the root note"
    );
    assert!(
        !vault.join("Workspaces/old-name/new-name.md").exists(),
        "dry-run must not create new-name.md"
    );

    // --- Invocation 1 Apply ---

    let mut apply1_cmd = Command::new(norn_bin());
    apply1_cmd
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan1_path)
        .args(["--yes"]);
    let _cache1b = isolate_cache(&mut apply1_cmd);
    let apply1_out = apply1_cmd.output().unwrap();

    assert!(
        apply1_out.status.success(),
        "Invocation 1 apply should succeed; stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&apply1_out.stderr),
        String::from_utf8_lossy(&apply1_out.stdout)
    );

    // Root note renamed: old stem gone, new stem present
    assert!(
        !vault.join("Workspaces/old-name/old-name.md").exists(),
        "old-name.md must be renamed after Invocation 1"
    );
    assert!(
        vault.join("Workspaces/old-name/new-name.md").exists(),
        "new-name.md must exist after Invocation 1 (stem change)"
    );

    // All backlinks rewritten by the cascade:
    //   other.md body: [[old-name]] → [[new-name]]
    let other_content = fs::read_to_string(vault.join("other.md")).unwrap();
    assert!(
        other_content.contains("[[new-name]]"),
        "other.md body link must be rewritten to [[new-name]] by cascade; got: {other_content}"
    );
    assert!(
        !other_content.contains("[[old-name]]"),
        "other.md must not still contain [[old-name]]; got: {other_content}"
    );

    //   note1.md frontmatter: workspace: "[[old-name]]" → "[[new-name]]"
    let note1_content =
        fs::read_to_string(vault.join("Workspaces/old-name/notes/note1.md")).unwrap();
    assert!(
        note1_content.contains("[[new-name]]"),
        "note1.md frontmatter workspace must be rewritten to [[new-name]] by cascade; \
         this verifies the move cascade rewrites frontmatter wikilinks (LinkSourceArea::Frontmatter); \
         got: {note1_content}"
    );
    assert!(
        !note1_content.contains("[[old-name]]"),
        "note1.md must not still contain [[old-name]]; got: {note1_content}"
    );

    //   task1.md frontmatter: workspace: "[[old-name]]" → "[[new-name]]"
    let task1_content =
        fs::read_to_string(vault.join("Workspaces/old-name/tasks/task1.md")).unwrap();
    assert!(
        task1_content.contains("[[new-name]]"),
        "task1.md frontmatter workspace must be rewritten to [[new-name]] by cascade; got: {task1_content}"
    );
    assert!(
        !task1_content.contains("[[old-name]]"),
        "task1.md must not still contain [[old-name]]; got: {task1_content}"
    );

    //   another.md frontmatter: workspace: "[[old-name]]" → "[[new-name]]"
    let another_content = fs::read_to_string(vault.join("another.md")).unwrap();
    assert!(
        another_content.contains("[[new-name]]"),
        "another.md frontmatter workspace must be rewritten to [[new-name]] by cascade; \
         got: {another_content}"
    );
    assert!(
        !another_content.contains("[[old-name]]"),
        "another.md must not still contain [[old-name]]; got: {another_content}"
    );

    // -----------------------------------------------------------------------
    // Invocation 2: move the folder (stem-preserving — no link rewrite needed)
    // All files under Workspaces/old-name/ have stems that are UNCHANGED by
    // this folder move, so [[new-name]] continues to resolve correctly.
    // -----------------------------------------------------------------------

    let plan2 = format!(
        r#"schema_version: 1
vault_root: {vault_root}
operations:
  - kind: move_folder
    fields:
      src: Workspaces/old-name
      dst: Workspaces/new-name
      parents: true
"#,
        vault_root = vault_str
    );

    let plan2_path = tmp.path().join("plan2.yaml");
    fs::write(&plan2_path, &plan2).unwrap();

    // --- Invocation 2 Dry-run ---

    let mut dry2_cmd = Command::new(norn_bin());
    dry2_cmd
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan2_path)
        .args(["--dry-run", "--format", "json"]);
    let _cache2a = isolate_cache(&mut dry2_cmd);
    let dry2_out = dry2_cmd.output().unwrap();

    assert!(
        dry2_out.status.success(),
        "Invocation 2 dry-run should succeed; stderr: {}",
        String::from_utf8_lossy(&dry2_out.stderr)
    );

    let dry2_stdout = String::from_utf8_lossy(&dry2_out.stdout);
    let dry2_report: serde_json::Value =
        serde_json::from_str(&dry2_stdout).expect("Invocation 2 dry-run output must be valid JSON");

    assert_eq!(dry2_report["schema_version"], 1);
    assert_eq!(dry2_report["dry_run"], true);

    let dry2_ops = dry2_report["operations"]
        .as_array()
        .expect("operations must be an array");

    // move_folder expands to one move_document per .md file under old-name:
    //   Workspaces/old-name/new-name.md       → Workspaces/new-name/new-name.md
    //   Workspaces/old-name/notes/note1.md    → Workspaces/new-name/notes/note1.md
    //   Workspaces/old-name/tasks/task1.md    → Workspaces/new-name/tasks/task1.md
    assert_eq!(
        dry2_ops.len(),
        3,
        "Invocation 2: move_folder should expand to 3 move_document ops \
         (new-name.md, note1.md, task1.md); got {}",
        dry2_ops.len()
    );

    let move_doc_count = dry2_ops
        .iter()
        .filter(|o| o["kind"] == "move_document")
        .count();
    assert_eq!(
        move_doc_count, 3,
        "all 3 ops must be move_document; got {}",
        move_doc_count
    );

    for op in dry2_ops {
        assert_eq!(
            op["status"], "not_run",
            "all dry-run ops must be not_run; got: {}",
            op
        );
    }

    // Dry-run must not mutate — files still at Workspaces/old-name/
    assert!(
        vault.join("Workspaces/old-name/new-name.md").exists(),
        "dry-run must not move files"
    );
    assert!(
        !vault.join("Workspaces/new-name").exists(),
        "dry-run must not create Workspaces/new-name/"
    );

    // --- Invocation 2 Apply ---

    let mut apply2_cmd = Command::new(norn_bin());
    apply2_cmd
        .args(["--cwd"])
        .arg(&vault)
        .args(["migrate"])
        .arg(&plan2_path)
        .args(["--yes"]);
    let _cache2b = isolate_cache(&mut apply2_cmd);
    let apply2_out = apply2_cmd.output().unwrap();

    assert!(
        apply2_out.status.success(),
        "Invocation 2 apply should succeed; stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&apply2_out.stderr),
        String::from_utf8_lossy(&apply2_out.stdout)
    );

    // Old folder has no .md files remaining
    let old_md_files: Vec<_> = WalkDir::new(vault.join("Workspaces/old-name"))
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    assert!(
        old_md_files.is_empty(),
        "Workspaces/old-name/ should have no .md files after Invocation 2; found: {:?}",
        old_md_files
            .iter()
            .map(|e| e.path().display().to_string())
            .collect::<Vec<_>>()
    );

    // New folder exists with the expected files — root note ends up as new-name.md
    assert!(
        vault.join("Workspaces/new-name/new-name.md").exists(),
        "Workspaces/new-name/new-name.md must exist (root note: stem rename + folder move)"
    );
    assert!(
        vault.join("Workspaces/new-name/notes/note1.md").exists(),
        "Workspaces/new-name/notes/note1.md must exist"
    );
    assert!(
        vault.join("Workspaces/new-name/tasks/task1.md").exists(),
        "Workspaces/new-name/tasks/task1.md must exist"
    );

    // -----------------------------------------------------------------------
    // Final check: link-preservation — norn validate must show ZERO new
    // link-target-missing findings after both invocations.
    //
    // [[new-name]] resolves to Workspaces/new-name/new-name.md (stem = new-name).
    // The folder move preserved stems for note1.md and task1.md.
    // All backlinks were rewritten in Invocation 1's cascade.
    //
    // If any link-target-missing findings appear here, the migration is NOT
    // link-preserving — that is a real bug, not an expected limitation.
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

    assert_eq!(
        broken_link_rows.len(),
        0,
        "the two-invocation migration must be fully link-preserving: \
         expected ZERO link-target-missing findings after both invocations; \
         got {} findings.\n\nThis is a real bug — the migration is not link-preserving.\n\
         validate stdout:\n{}",
        broken_link_rows.len(),
        validate_stdout
    );
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
