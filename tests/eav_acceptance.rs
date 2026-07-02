//! Wave-2 acceptance (NRN-80), black-box half.
//!
//! `document_fields`'s absent-sentinel row (a `x'00'` BLOB — see
//! `src/cache/document_fields.rs`) is purely an internal index-plumbing
//! detail: query results are always projected from the `documents` table's
//! `frontmatter_json` column, never from `document_fields.value`. This file
//! pins that invariant at the CLI's actual output boundary — `find`/`count`
//! across every format — rather than trusting it never regresses. See
//! `src/cache/eav_acceptance.rs` for the sibling EXPLAIN-QUERY-PLAN guard
//! matrix (that half needs `Cache`/`DocumentQuery` internals unavailable
//! from an integration test — this crate ships only a binary target).

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn isolate_cache(command: &mut Command) -> TempDir {
    let dir = tempfile::tempdir().expect("temp cache dir should be created");
    command.env("XDG_CACHE_HOME", dir.path());
    command.env("XDG_STATE_HOME", dir.path().join("state"));
    dir
}

fn write_config(root: &std::path::Path, body: &str) {
    let dir = root.join(".norn");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.yaml"), body).unwrap();
}

const INDEXED_CONFIG: &str = r#"
validate:
  rules:
    - name: r
      field_types:
        project: string
        lifecycle: string
        anchor: wikilink
        tags: list_of_strings
"#;

/// A vault where every document with a declared, indexed field is missing at
/// least one of them — every declared field gets at least one absent-
/// sentinel `document_fields` row across the vault.
fn sentinel_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-eav-sentinel-")
        .tempdir()
        .unwrap();
    let root = tmp.path();
    write_config(root, INDEXED_CONFIG);
    fs::write(
        root.join("full.md"),
        "---\nproject: NRN\nlifecycle: active\nanchor: \"[[NRN-80]]\"\ntags: [\"release:v0.40\"]\n---\nbody\n",
    )
    .unwrap();
    // Missing `anchor` and `tags` entirely.
    fs::write(
        root.join("bare.md"),
        "---\nproject: NRN\nlifecycle: active\n---\nbody\n",
    )
    .unwrap();
    // `anchor` present but null, `tags` present but empty.
    fs::write(
        root.join("nulled.md"),
        "---\nproject: NRN\nlifecycle: active\nanchor: null\ntags: []\n---\nbody\n",
    )
    .unwrap();
    // No frontmatter fields declared by the rule at all.
    fs::write(root.join("other.md"), "---\nother: x\n---\nbody\n").unwrap();
    tmp
}

/// Bytes that would only ever appear if the raw `document_fields` sentinel
/// (or a naive hex/escape rendering of it) leaked into rendered output.
fn assert_no_sentinel_artifact(label: &str, bytes: &[u8]) {
    assert!(
        !bytes.contains(&0u8),
        "{label}: output contains a literal NUL byte (sentinel leak): {:?}",
        String::from_utf8_lossy(bytes)
    );
    let text = String::from_utf8_lossy(bytes);
    for needle in ["\\u0000", "x'00'", "\\x00", "�"] {
        assert!(
            !text.contains(needle),
            "{label}: output contains sentinel artifact {needle:?}: {text}"
        );
    }
}

fn run(vault: &TempDir, args: &[&str]) -> (Vec<u8>, Vec<u8>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
    command.arg("-C").arg(vault.path());
    command.args(args);
    let _cache = isolate_cache(&mut command);
    let output = command.output().expect("norn should run");
    assert!(
        output.status.success(),
        "command failed\nargs: {args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (output.stdout, output.stderr)
}

#[test]
fn find_missing_query_never_leaks_sentinel_in_json() {
    let vault = sentinel_vault();
    let (stdout, stderr) = run(
        &vault,
        &[
            "find",
            "--missing",
            "anchor",
            "--no-limit",
            "--format",
            "json",
        ],
    );
    assert_no_sentinel_artifact("find --missing anchor --format json (stdout)", &stdout);
    assert_no_sentinel_artifact("find --missing anchor --format json (stderr)", &stderr);
    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    let docs = parsed["documents"].as_array().unwrap();
    assert!(!docs.is_empty(), "expected at least one missing-anchor doc");
}

#[test]
fn find_all_cols_dump_never_leaks_sentinel_across_formats() {
    let vault = sentinel_vault();
    for format in ["json", "jsonl", "paths"] {
        let (stdout, stderr) = run(
            &vault,
            &[
                "find",
                "--all",
                "--all-cols",
                "--no-limit",
                "--format",
                format,
            ],
        );
        assert_no_sentinel_artifact(
            &format!("find --all --all-cols --format {format} (stdout)"),
            &stdout,
        );
        assert_no_sentinel_artifact(
            &format!("find --all --all-cols --format {format} (stderr)"),
            &stderr,
        );
    }
}

#[test]
fn find_col_projection_of_a_missing_field_never_leaks_sentinel() {
    let vault = sentinel_vault();
    let (stdout, stderr) = run(
        &vault,
        &[
            "find",
            "--all",
            "--col",
            "anchor,tags,project",
            "--no-limit",
            "--format",
            "jsonl",
        ],
    );
    assert_no_sentinel_artifact(
        "find --col anchor,tags,project --format jsonl (stdout)",
        &stdout,
    );
    assert_no_sentinel_artifact(
        "find --col anchor,tags,project --format jsonl (stderr)",
        &stderr,
    );
    // Sanity: the projection ran and included the null/missing rows (not an
    // empty no-op).
    let lines: Vec<&str> = std::str::from_utf8(&stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(lines.len(), 4, "expected all four docs in the dump");
}

#[test]
fn find_records_format_never_leaks_sentinel() {
    let vault = sentinel_vault();
    let (stdout, stderr) = run(
        &vault,
        &[
            "find",
            "--all",
            "--all-cols",
            "--no-limit",
            "--format",
            "records",
        ],
    );
    assert_no_sentinel_artifact("find --format records (stdout)", &stdout);
    assert_no_sentinel_artifact("find --format records (stderr)", &stderr);
}

#[test]
fn count_by_field_with_missing_values_never_leaks_sentinel() {
    let vault = sentinel_vault();
    let (stdout, stderr) = run(
        &vault,
        &["count", "--by", "anchor,tags", "--format", "json"],
    );
    assert_no_sentinel_artifact("count --by anchor,tags --format json (stdout)", &stdout);
    assert_no_sentinel_artifact("count --by anchor,tags --format json (stderr)", &stderr);
    let parsed: serde_json::Value = serde_json::from_slice(&stdout).expect("valid JSON");
    assert!(parsed["total"].as_u64().unwrap() >= 4);
}
