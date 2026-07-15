use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Creates an isolated temp directory using a non-hidden prefix so the
/// walker does not treat it as hidden.
///
/// Returns the path; the caller must call `fs::remove_dir_all` for cleanup
/// since we need the directory to persist beyond the `TempDir` scope.
fn vault_root(prefix: &str) -> PathBuf {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("temp dir should be created");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    path
}

/// Runs `norn repair plan --format report` with isolated XDG cache/state trees and NO_COLOR.
/// Returns the raw stdout string.
fn run_plan(root: &Path, config_path: &Path, extra_args: &[&str]) -> String {
    let cache_dir = tempfile::Builder::new()
        .prefix("norn-cache-")
        .tempdir()
        .expect("cache temp dir should be created");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_norn"));
    cmd.args([
        "-C",
        root.to_str().unwrap(),
        "--config",
        config_path.to_str().unwrap(),
        "repair",
        "--plan",
        "--format",
        "report",
    ]);
    cmd.args(extra_args);
    cmd.env("XDG_CACHE_HOME", cache_dir.path())
        .env("XDG_STATE_HOME", cache_dir.path().join("state"))
        .env("NO_COLOR", "1");

    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "vault command failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Builds a vault fixture that produces two distinct skip-reason kinds:
///
/// 1. `missing-default` — a doc has a required field (`status`) missing and
///    there is no repair rule providing a default.
/// 2. `link-decision-needed` — a doc has a broken wikilink with no closest-match candidate.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_mixed_skips_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-report-mixed-");
    let config_path = root.with_extension("yaml");

    // Config: require `status` on all docs; no repair rules (so MissingDefault is the skip path).
    fs::write(
        &config_path,
        "validate:\n  required_frontmatter:\n    - status\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Doc 1: missing `status` field → RequiredFrontmatterMissing → missing-default skip reason.
    fs::write(
        root.join("alpha.md"),
        "---\ntitle: Alpha\n---\n# Alpha\n\nNo status field.\n",
    )
    .expect("alpha.md should write");

    // Doc 2: broken wikilink with a unique name → link-decision-needed skip reason.
    fs::write(
        root.join("beta.md"),
        "---\ntitle: Beta\nstatus: active\n---\n# Beta\n\n[[totally-unknown-xyzzy-report-12345]]\n",
    )
    .expect("beta.md should write");

    (root, config_path)
}

/// Build a vault fixture with 6 source files each containing a broken wikilink
/// pointing to a corresponding target document.  The targets are named so that
/// slug-normalization produces High-confidence closest-match proposals — one
/// planned change per source file.
///
/// File counts (changes per source):
///   alpha-source.md  → [[Alpha Target]]  (10 links, same target repeated)
///   ... not practical with wikilinks; instead use 6 distinct sources each
///       contributing 1 change, with different target counts via multiple links.
///
/// Simpler approach: 6 source files, each with N distinct broken links that
/// each slug-match a corresponding target.  Counts [10, 7, 6, 5, 3, 1] would
/// require 32 target documents, which is fine but verbose.  Use a flat
/// one-link-per-source fixture with 6 files and rely on all-different counts
/// being enforced via the ordering test (file-6 with count 1 must not appear).
///
/// Actually the simplest: each source has a *repeated* broken wikilink so the
/// same target appears N times in the body.  Each occurrence produces exactly
/// one PlannedChange (the closest-match engine deduplicates per-occurrence, not
/// per-target).
///
/// Counts layout: [10, 7, 6, 5, 3, 1] for files [f10, f07, f06, f05, f03, f01].
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_multi_file_changes_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-report-multifile-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Helper: write a target document whose stem slug-normalizes to the given key.
    // Helper: write a source document with `count` broken wikilinks pointing at `target_title`.
    let write_target = |stem: &str, title: &str| {
        fs::write(
            root.join(format!("{stem}.md")),
            format!("---\ntitle: {title}\n---\n# {title}\n\nTarget.\n"),
        )
        .expect("target should write");
    };
    let write_source = |name: &str, target_title: &str, target_stem: &str, count: usize| {
        // Each link "[[Target Title N]]" slug-normalizes to `target_stem-N` — but we
        // need them all to match the SAME stem.  Use a single unique broken target
        // per source and repeat it `count` times, each on its own line so the parser
        // sees N link occurrences.
        let _ = target_stem; // stem used for target file, link text normalizes to it
        let links = (0..count)
            .map(|_| format!("[[{target_title}]]"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(
            root.join(format!("{name}.md")),
            format!("---\ntitle: {name}\n---\n# {name}\n\n{links}\n"),
        )
        .expect("source should write");
    };

    // Targets (each a unique stem so no tie collisions).
    write_target("alpha-target", "Alpha Target");
    write_target("bravo-target", "Bravo Target");
    write_target("charlie-target", "Charlie Target");
    write_target("delta-target", "Delta Target");
    write_target("echo-target", "Echo Target");
    write_target("foxtrot-target", "Foxtrot Target");

    // Sources with counts [10, 7, 6, 5, 3, 1].
    write_source("f10", "Alpha Target", "alpha-target", 10);
    write_source("f07", "Bravo Target", "bravo-target", 7);
    write_source("f06", "Charlie Target", "charlie-target", 6);
    write_source("f05", "Delta Target", "delta-target", 5);
    write_source("f03", "Echo Target", "echo-target", 3);
    write_source("f01", "Foxtrot Target", "foxtrot-target", 1);

    (root, config_path)
}

/// Build a vault fixture that produces only skipped findings (no planned changes).
///
/// Strategy: one doc with a broken wikilink that has no closest match candidate
/// (unique nonsense string), so it routes to link-decision-needed.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_only_skips_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-report-only-skips-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // One doc with a broken link that has no closest match.
    fs::write(
        root.join("source.md"),
        "---\ntitle: Source\n---\n# Source\n\n[[totally-unknown-xyzzy-no-match-12345]]\n",
    )
    .expect("source.md should write");

    (root, config_path)
}

/// Build a vault fixture with 3 source files that each have the same number of
/// planned changes (1 each), named `c.md`, `a.md`, and `b.md` to verify that the
/// alphabetical tiebreak renders them in a → b → c order.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_tied_files_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-report-tied-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Three target documents.
    fs::write(
        root.join("apple-target.md"),
        "---\ntitle: Apple Target\n---\n# Apple Target\n\nTarget.\n",
    )
    .expect("apple-target.md should write");
    fs::write(
        root.join("banana-target.md"),
        "---\ntitle: Banana Target\n---\n# Banana Target\n\nTarget.\n",
    )
    .expect("banana-target.md should write");
    fs::write(
        root.join("cherry-target.md"),
        "---\ntitle: Cherry Target\n---\n# Cherry Target\n\nTarget.\n",
    )
    .expect("cherry-target.md should write");

    // Three sources named c.md, a.md, b.md (intentionally out of order) each with 1 broken link.
    fs::write(
        root.join("c.md"),
        "---\ntitle: C\n---\n# C\n\n[[Apple Target]]\n",
    )
    .expect("c.md should write");
    fs::write(
        root.join("a.md"),
        "---\ntitle: A\n---\n# A\n\n[[Banana Target]]\n",
    )
    .expect("a.md should write");
    fs::write(
        root.join("b.md"),
        "---\ntitle: B\n---\n# B\n\n[[Cherry Target]]\n",
    )
    .expect("b.md should write");

    (root, config_path)
}

/// Build a vault fixture that has at least one planned change with a
/// High-confidence closest-match footnote.
///
/// Strategy: create a document `norn-brand.md` as a target, then a second
/// document with a wikilink `[[Norn Brand]]`.  The slug-normalization of
/// "Norn Brand" → "norn-brand" matches the stem exactly → High confidence.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_report_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-report-");
    let config_path = root.with_extension("yaml");

    // Minimal config: no required fields, no repair rules (built-in closest-match handles links).
    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Target document: norn-brand.md — its stem normalizes to "norn-brand".
    fs::write(
        root.join("norn-brand.md"),
        "---\ntitle: Norn Brand\n---\n# Norn Brand\n\nTarget document.\n",
    )
    .expect("norn-brand.md should write");

    // Source document: broken wikilink [[Norn Brand]] — slug-normalized to "norn-brand",
    // which is a High-confidence slug-identity match to norn-brand.md.
    fs::write(
        root.join("source.md"),
        "---\ntitle: Source\n---\n# Source\n\nSee [[Norn Brand]] for details.\n",
    )
    .expect("source.md should write");

    (root, config_path)
}

#[test]
fn report_format_renders_header_count_and_confidence_breakdown() {
    let (root, config_path) = build_report_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    // Header: "Repair plan against <vault_root>…"
    assert!(
        stdout.contains("Repair plan against"),
        "expected header; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(root.to_str().unwrap()),
        "expected vault root in header; full stdout:\n{stdout}"
    );

    // Count line: "N operations proposed across N files"
    assert!(
        stdout.contains("operations proposed"),
        "expected count line; full stdout:\n{stdout}"
    );

    // The fixture produces a High-confidence closest-match footnote (slug-identity match).
    assert!(
        stdout.contains("Footnotes"),
        "expected footnotes section; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("high") || stdout.contains("medium"),
        "expected confidence band in footnote; full stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn report_renders_skipped_tally_with_codes_and_prose() {
    let (root, config_path) = build_mixed_skips_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    assert!(
        stdout.contains("Skipped"),
        "expected Skipped section; full stdout:\n{stdout}"
    );
    // Both the code and a snippet of the prose should appear.
    assert!(
        stdout.contains("missing-default"),
        "expected missing-default code; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("missing field has no configured"),
        "expected missing-default prose; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("link-decision-needed"),
        "expected link-decision-needed code; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("link repair requires"),
        "expected link-decision-needed prose; full stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn report_omits_skipped_section_when_no_skips() {
    let (root, config_path) = build_report_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    assert!(
        !stdout.contains("Skipped"),
        "Skipped section should not render when no skipped findings, got:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn report_renders_top_5_affected_files_by_count_desc() {
    let (root, config_path) = build_multi_file_changes_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    assert!(
        stdout.contains("Top affected files"),
        "expected Top affected files section; full stdout:\n{stdout}"
    );

    // The fixture has 6 source files with counts [10, 7, 6, 5, 3, 1].
    // The top 5 are f10, f07, f06, f05, f03. f01 (count 1) should NOT appear.
    assert!(
        stdout.contains("f10"),
        "expected f10 (count 10) in top 5; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("f07"),
        "expected f07 (count 7) in top 5; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("f06"),
        "expected f06 (count 6) in top 5; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("f05"),
        "expected f05 (count 5) in top 5; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("f03"),
        "expected f03 (count 3) in top 5; full stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("f01"),
        "f01 (count 1, rank 6) must be capped out by top-5 limit; full stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn report_omits_top_files_when_no_changes() {
    let (root, config_path) = build_only_skips_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    assert!(
        !stdout.contains("Top affected files"),
        "Top affected files section must not render when no planned changes; full stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn report_top_files_tiebreak_alphabetical() {
    let (root, config_path) = build_tied_files_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    assert!(
        stdout.contains("Top affected files"),
        "expected Top affected files section; full stdout:\n{stdout}"
    );

    // Files a.md, b.md, c.md each have 1 change — alphabetical order must hold.
    let a_idx = stdout.find("a.md").expect("a.md must appear in stdout");
    let b_idx = stdout.find("b.md").expect("b.md must appear in stdout");
    let c_idx = stdout.find("c.md").expect("c.md must appear in stdout");
    assert!(
        a_idx < b_idx && b_idx < c_idx,
        "alphabetical tiebreak wrong; got order a={a_idx} b={b_idx} c={c_idx}\nstdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

/// Alias for build_report_fixture — produces at least one planned change.
fn build_plan_with_proposals_fixture() -> (PathBuf, PathBuf) {
    build_report_fixture()
}

#[test]
fn apply_guidance_unfiltered_suggests_high_confidence_narrowing() {
    let (root, config_path) = build_plan_with_proposals_fixture();
    let stdout = run_plan(&root, &config_path, &[]);

    assert!(
        stdout.contains("To inspect"),
        "expected To inspect block; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("norn repair --plan --confidence high --format json"),
        "expected high-confidence narrowing suggestion; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("norn repair --plan --format json"),
        "expected unfiltered inspect baseline; full stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("To apply"),
        "expected To apply block; full stdout:\n{stdout}"
    );
    // Apply guidance pipes the plan into `norn apply -`.
    assert!(
        stdout.contains("| norn apply -"),
        "expected apply suggestion; full stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn apply_guidance_echoes_active_confidence_filter() {
    let (root, config_path) = build_plan_with_proposals_fixture();
    let stdout = run_plan(&root, &config_path, &["--confidence", "high"]);

    assert!(
        stdout.contains("norn repair --plan --confidence high --format json"),
        "expected confidence echoed in command; full stdout:\n{stdout}"
    );
    // The unfiltered baseline should not appear in the apply section when --confidence is active
    let apply_section = stdout.split("To apply").nth(1).unwrap_or("");
    assert!(
        !apply_section.contains("norn repair --plan --format json |"),
        "unfiltered apply suggestion should be dropped when --confidence is active. stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn apply_guidance_single_quotes_glob_patterns() {
    let (root, config_path) = build_plan_with_proposals_fixture();
    let stdout = run_plan(&root, &config_path, &["--code", "link-*"]);

    assert!(
        stdout.contains("--code 'link-*'"),
        "glob pattern not single-quoted. stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn apply_guidance_suppresses_apply_block_when_skip_reason_active() {
    let (root, config_path) = build_plan_with_proposals_fixture();
    let stdout = run_plan(&root, &config_path, &["--skip-reason", "missing-default"]);

    assert!(
        stdout.contains("To inspect"),
        "expected To inspect block even when --skip-reason active; full stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("To apply"),
        "apply block should be suppressed when --skip-reason is active. stdout:\n{stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

/// Subprocess stdout is a pipe, not a tty, so omitting `--format` should default
/// to JSON output (MigrationPlan schema_version 2). Explicit `--format report`
/// still wins.
#[test]
fn piped_default_is_json_explicit_format_overrides() {
    let (root, config_path) = build_plan_with_proposals_fixture();
    let cache_dir = tempfile::Builder::new()
        .prefix("norn-piped-default-")
        .tempdir()
        .expect("cache temp dir should be created");

    // No --format flag → piped → JSON envelope
    let piped = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args([
            "-C",
            root.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
            "repair",
            "--plan",
        ])
        .env("XDG_CACHE_HOME", cache_dir.path())
        .env("XDG_STATE_HOME", cache_dir.path().join("state"))
        .env("NO_COLOR", "1")
        .output()
        .expect("piped run should execute");
    assert!(
        piped.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&piped.stderr)
    );
    let piped_stdout = String::from_utf8_lossy(&piped.stdout);
    assert!(
        piped_stdout.trim_start().starts_with('{'),
        "piped default should emit JSON envelope; got: {:?}",
        &piped_stdout[..50.min(piped_stdout.len())]
    );
    let json: serde_json::Value =
        serde_json::from_str(&piped_stdout).expect("piped default should be valid JSON");
    assert_eq!(json["schema_version"], 2);
    assert_eq!(json["generator"], "norn-repair");
    assert!(
        json["operations"].is_array(),
        "MigrationPlan must carry an operations array"
    );

    // Explicit --format report overrides the piped default
    let report = Command::new(env!("CARGO_BIN_EXE_norn"))
        .args([
            "-C",
            root.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
            "repair",
            "--plan",
            "--format",
            "report",
        ])
        .env("XDG_CACHE_HOME", cache_dir.path())
        .env("XDG_STATE_HOME", cache_dir.path().join("state"))
        .env("NO_COLOR", "1")
        .output()
        .expect("report run should execute");
    assert!(report.status.success());
    let report_stdout = String::from_utf8_lossy(&report.stdout);
    assert!(
        report_stdout.contains("Repair plan against"),
        "explicit --format report should produce the report header; got: {report_stdout}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}
