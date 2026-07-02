//! Integration coverage for the anchored string field operators
//! `--starts-with` / `--ends-with` / `--contains` (NRN-70): scalar and
//! array-element matching, wikilink-bracket collapse, case sensitivity,
//! literal `%`/`_` treatment, count parity, and invocation errors.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    dir
}

/// Vault fixture:
///   tagged-release.md — tags [release:v0.12, tooling]
///   tagged-ws.md      — tags [workspace:mimir]
///   scalar.md         — status in_progress, title "Progress: 100% done"
///   wikilinks.md      — depends_on ["[[NRN-123]]", "[[SK-9]]"]
///   upper.md          — status IN_PROGRESS (case-sensitivity control)
fn build_vault() -> TempDir {
    // A visible prefix matters: `TempDir::new()` yields a dot-prefixed dir
    // (`.tmpXXXX`), which norn's scanner treats as hidden — an empty vault.
    let tmp = tempfile::Builder::new()
        .prefix("norn-strops-")
        .tempdir()
        .unwrap();
    let root = tmp.path();
    fs::write(
        root.join("tagged-release.md"),
        "---\ntags:\n  - release:v0.12\n  - tooling\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("tagged-ws.md"),
        "---\ntags:\n  - workspace:mimir\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("scalar.md"),
        "---\nstatus: in_progress\ntitle: \"Progress: 100% done\"\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("wikilinks.md"),
        "---\ndepends_on:\n  - \"[[NRN-123]]\"\n  - \"[[SK-9]]\"\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("upper.md"),
        "---\nstatus: IN_PROGRESS\n---\nbody\n",
    )
    .unwrap();
    tmp
}

fn find_paths(vault: &TempDir, args: &[&str]) -> Vec<String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.arg("-C").arg(vault.path()).arg("find");
    command.args(args);
    command.args(["--no-limit", "--format", "json"]);
    let _cache = isolate_cache(&mut command);
    let output = command.output().expect("norn find should run");
    assert!(
        output.status.success(),
        "find failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output should be JSON");
    parsed["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn starts_with_enumerates_tag_namespace() {
    let vault = build_vault();
    // The B1 headline job: enumerate every doc carrying a `release:` tag.
    let paths = find_paths(&vault, &["--starts-with", "tags:release:"]);
    assert_eq!(paths, vec!["tagged-release.md"]);
}

#[test]
fn starts_with_matches_scalar_prefix() {
    let vault = build_vault();
    let paths = find_paths(&vault, &["--starts-with", "status:in_"]);
    assert_eq!(paths, vec!["scalar.md"]);
}

#[test]
fn ends_with_matches_scalar_suffix() {
    let vault = build_vault();
    let paths = find_paths(&vault, &["--ends-with", "status:_progress"]);
    assert_eq!(paths, vec!["scalar.md"]);
}

#[test]
fn contains_matches_array_element_substring() {
    let vault = build_vault();
    let paths = find_paths(&vault, &["--contains", "tags:mimir"]);
    assert_eq!(paths, vec!["tagged-ws.md"]);
}

#[test]
fn operators_collapse_wikilink_brackets() {
    let vault = build_vault();
    // Stored "[[NRN-123]]" matches a bare prefix needle...
    let paths = find_paths(&vault, &["--starts-with", "depends_on:NRN-"]);
    assert_eq!(paths, vec!["wikilinks.md"]);
    // ...and a bracketed needle is collapsed the same way `--eq` does it.
    let paths = find_paths(&vault, &["--starts-with", "depends_on:[[NRN-"]);
    assert_eq!(paths, vec!["wikilinks.md"]);
}

#[test]
fn operators_are_case_sensitive() {
    let vault = build_vault();
    let paths = find_paths(&vault, &["--starts-with", "status:IN_"]);
    assert_eq!(
        paths,
        vec!["upper.md"],
        "uppercase needle must match only the uppercase value"
    );
    let paths = find_paths(&vault, &["--contains", "status:progress"]);
    assert_eq!(
        paths,
        vec!["scalar.md"],
        "lowercase needle must not match IN_PROGRESS"
    );
}

#[test]
fn contains_treats_percent_and_underscore_literally() {
    let vault = build_vault();
    // `%` is a literal character, not a wildcard: only the title containing
    // "100%" matches, and "0% d" cannot match anything else.
    let paths = find_paths(&vault, &["--contains", "title:100%"]);
    assert_eq!(paths, vec!["scalar.md"]);
    // `_` is literal too: `in_` must not behave as `in<any>`.
    let paths = find_paths(&vault, &["--contains", "status:n_p"]);
    assert_eq!(paths, vec!["scalar.md"]);
}

#[test]
fn operators_compose_all_of_with_other_filters() {
    let vault = build_vault();
    let paths = find_paths(
        &vault,
        &["--starts-with", "status:in_", "--ends-with", "status:gress"],
    );
    assert_eq!(paths, vec!["scalar.md"]);
    let paths = find_paths(
        &vault,
        &["--starts-with", "status:in_", "--contains", "status:zzz"],
    );
    assert!(paths.is_empty());
}

#[test]
fn count_shares_the_operator_surface() {
    let vault = build_vault();
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.arg("-C").arg(vault.path());
    command.args(["count", "--contains", "tags:release:", "--format", "json"]);
    let _cache = isolate_cache(&mut command);
    let output = command.output().expect("norn count should run");
    assert!(
        output.status.success(),
        "count failed\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let parsed: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("output should be JSON");
    assert_eq!(parsed["total"], 1);
}

#[test]
fn empty_value_is_a_usage_error() {
    let vault = build_vault();
    for flag in ["--starts-with", "--ends-with", "--contains"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
        command.arg("-C").arg(vault.path());
        command.args(["find", flag, "tags:", "--format", "json"]);
        let _cache = isolate_cache(&mut command);
        let output = command.output().expect("norn find should run");
        // Filter-token parse failures follow the existing `--eq` convention:
        // exit 1 via the generic error path (not clap's exit 2).
        assert_eq!(
            output.status.code(),
            Some(1),
            "{flag} with an empty value should exit 1, stderr: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            stderr.contains(flag) && stderr.contains("non-empty"),
            "stderr should name the flag and the empty-value problem: {stderr}"
        );
    }
}

#[test]
fn missing_colon_is_a_usage_error() {
    let vault = build_vault();
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.arg("-C").arg(vault.path());
    command.args(["find", "--starts-with", "nocolon", "--format", "json"]);
    let _cache = isolate_cache(&mut command);
    let output = command.output().expect("norn find should run");
    assert_eq!(output.status.code(), Some(1));
}
