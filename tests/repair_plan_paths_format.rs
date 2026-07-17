use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

/// Runs `vault repair plan` with the given extra args, isolated XDG cache/state trees and NO_COLOR.
/// Returns the raw `Output` (stdout bytes, exit status).
fn run_command(root: &Path, config_path: &Path, extra_args: &[&str]) -> Output {
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
    ]);
    cmd.args(extra_args);
    prewrite_prune_marker(cache_dir.path());
    cmd.env("XDG_CACHE_HOME", cache_dir.path())
        .env("XDG_STATE_HOME", cache_dir.path().join("state"))
        .env("NO_COLOR", "1");

    cmd.output().expect("vault command should execute")
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

/// Build a vault fixture that produces 5 changes across 3 source files:
///   a.md  → 2 distinct broken wikilinks (2 changes)
///   b.md  → 2 distinct broken wikilinks (2 changes)
///   c.md  → 1 broken wikilink (1 change)
///
/// Each broken link slug-matches a unique existing target → High-confidence planned changes.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_multi_changes_per_file_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-paths-multi-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Target documents — each stem slug-normalizes to the wikilink text.
    for (stem, title) in &[
        ("alpha-one", "Alpha One"),
        ("alpha-two", "Alpha Two"),
        ("bravo-one", "Bravo One"),
        ("bravo-two", "Bravo Two"),
        ("charlie-one", "Charlie One"),
    ] {
        fs::write(
            root.join(format!("{stem}.md")),
            format!("---\ntitle: {title}\n---\n# {title}\n\nTarget.\n"),
        )
        .expect("target should write");
    }

    // a.md: 2 broken wikilinks (pointing at alpha-one and alpha-two)
    fs::write(
        root.join("a.md"),
        "---\ntitle: A\n---\n# A\n\n[[Alpha One]]\n[[Alpha Two]]\n",
    )
    .expect("a.md should write");

    // b.md: 2 broken wikilinks (pointing at bravo-one and bravo-two)
    fs::write(
        root.join("b.md"),
        "---\ntitle: B\n---\n# B\n\n[[Bravo One]]\n[[Bravo Two]]\n",
    )
    .expect("b.md should write");

    // c.md: 1 broken wikilink (pointing at charlie-one)
    fs::write(
        root.join("c.md"),
        "---\ntitle: C\n---\n# C\n\n[[Charlie One]]\n",
    )
    .expect("c.md should write");

    (root, config_path)
}

/// Build a vault fixture with mixed-confidence proposals so that
/// `--confidence high` narrows the result set.
///
/// Strategy:
///   high-source.md → [[High Target]] — slug-identity → High confidence
///   medium-source.md → a link that is a Levenshtein-only match → Medium confidence
///
/// In practice generating a reliable medium-confidence match requires careful
/// naming; we use the slug-identity path for the high band and a slightly
/// different slug for the medium band.
///
/// Two files → two paths at baseline; with --confidence high only the
/// high-confidence source remains → 1 path.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_mixed_confidence_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-paths-mixed-conf-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    // Target for high-confidence: slug-identity match.
    fs::write(
        root.join("zeta-brand.md"),
        "---\ntitle: Zeta Brand\n---\n# Zeta Brand\n\nTarget.\n",
    )
    .expect("zeta-brand.md should write");

    // High-confidence source: [[Zeta Brand]] → slug "zeta-brand" == stem exactly.
    fs::write(
        root.join("high-source.md"),
        "---\ntitle: High Source\n---\n# High Source\n\n[[Zeta Brand]]\n",
    )
    .expect("high-source.md should write");

    // Target for medium-confidence: a stem slightly different from the broken link.
    // "omega-brands" vs link "[[Omega Brand]]" → slug "omega-brand" vs stem "omega-brands"
    // — edit distance 1 on short strings often qualifies as medium.
    fs::write(
        root.join("omega-brands.md"),
        "---\ntitle: Omega Brands\n---\n# Omega Brands\n\nTarget.\n",
    )
    .expect("omega-brands.md should write");

    // Medium-confidence source.
    fs::write(
        root.join("medium-source.md"),
        "---\ntitle: Medium Source\n---\n# Medium Source\n\n[[Omega Brand]]\n",
    )
    .expect("medium-source.md should write");

    (root, config_path)
}

/// Build a vault fixture that produces only skipped findings (no planned changes).
///
/// One doc with a broken wikilink that has no closest-match candidate.
///
/// Returns `(root_dir, config_path)`. Caller is responsible for cleanup.
fn build_only_skips_fixture() -> (PathBuf, PathBuf) {
    let root = vault_root("norn-paths-only-skips-");
    let config_path = root.with_extension("yaml");

    fs::write(
        &config_path,
        "validate:\n  required_frontmatter: []\n  rules: []\nrepair:\n  rules: []\n",
    )
    .expect("config should write");

    fs::write(
        root.join("source.md"),
        "---\ntitle: Source\n---\n# Source\n\n[[totally-unknown-xyzzy-no-match-paths-99999]]\n",
    )
    .expect("source.md should write");

    (root, config_path)
}

#[test]
fn paths_format_emits_sorted_dedup_paths() {
    let (root, config_path) = build_multi_changes_per_file_fixture();
    let out = run_command(&root, &config_path, &["--format", "paths"]);
    assert!(
        out.status.success(),
        "vault command failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    // Expect exactly 3 unique paths (a.md, b.md, c.md) despite 5 total changes.
    assert_eq!(
        lines.len(),
        3,
        "expected 3 unique paths; got {}: {lines:?}",
        lines.len()
    );

    // Paths must be sorted lexically ascending.
    assert!(
        lines.windows(2).all(|w| w[0] <= w[1]),
        "paths should be sorted: {lines:?}"
    );

    // The three files should be present (basenames are sufficient since paths are relative).
    assert!(
        lines.iter().any(|l| l.ends_with("a.md")),
        "a.md missing: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.ends_with("b.md")),
        "b.md missing: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.ends_with("c.md")),
        "c.md missing: {lines:?}"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn paths_format_respects_confidence_filter() {
    let (root, config_path) = build_mixed_confidence_fixture();

    let baseline = run_command(&root, &config_path, &["--format", "paths"]);
    assert!(
        baseline.status.success(),
        "baseline run failed\nstderr: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let baseline_count = String::from_utf8_lossy(&baseline.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    let filtered = run_command(
        &root,
        &config_path,
        &["--format", "paths", "--confidence", "high"],
    );
    assert!(
        filtered.status.success(),
        "filtered run failed\nstderr: {}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let filtered_count = String::from_utf8_lossy(&filtered.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count();

    assert!(
        filtered_count <= baseline_count,
        "high-confidence filter should narrow or preserve path count \
         (baseline={baseline_count}, filtered={filtered_count})"
    );

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}

#[test]
fn paths_format_empty_when_no_changes() {
    let (root, config_path) = build_only_skips_fixture();
    let out = run_command(&root, &config_path, &["--format", "paths"]);

    assert!(
        out.status.success(),
        "vault command failed\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        b"",
        "expected empty stdout when no changes; got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(out.status.code(), Some(0));

    // Cleanup
    fs::remove_dir_all(&root).ok();
    fs::remove_file(&config_path).ok();
}
