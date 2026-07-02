//! Integration coverage for the Wave-2 `document_fields` query router
//! (NRN-79): `norn find`/`norn count` results must be byte-identical whether
//! a predicate's field is declared `indexed` (routes through the derived
//! `document_fields` EAV table) or not (falls back to the `json_extract`
//! scan path) — see `src/cache/query_documents.rs`'s router and
//! `src/cache/scan_semantics_probe.rs`'s pinned scan-path truths.
//!
//! Also covers the cold-scan warning: `find`/`count` warn on stderr (never
//! stdout) when falling back because a referenced field isn't indexed, but
//! only once the vault crosses 1,000 documents.

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

const INDEXED_STATUS_CONFIG: &str = r#"
validate:
  rules:
    - name: r
      field_types:
        status: { type: string, indexed: true }
"#;

fn small_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-idxroute-")
        .tempdir()
        .unwrap();
    let root = tmp.path();
    fs::write(
        root.join("a.md"),
        "---\nstatus: active\nkind: log\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        root.join("b.md"),
        "---\nstatus: backlog\nkind: log\n---\nbody\n",
    )
    .unwrap();
    fs::write(root.join("c.md"), "---\nstatus: active\n---\nbody\n").unwrap();
    fs::write(root.join("d.md"), "---\nother: x\n---\nbody\n").unwrap();
    tmp
}

fn find_json(vault: &TempDir, args: &[&str]) -> (serde_json::Value, String) {
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
    (parsed, String::from_utf8_lossy(&output.stderr).to_string())
}

fn find_paths(vault: &TempDir, args: &[&str]) -> Vec<String> {
    let (parsed, _stderr) = find_json(vault, args);
    let mut paths: Vec<String> = parsed["documents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    paths
}

// ── EAV routing does not change find/count results ──────────────────────

#[test]
fn eq_result_identical_whether_field_is_indexed_or_not() {
    let unindexed = small_vault();
    let indexed = small_vault();
    write_config(indexed.path(), INDEXED_STATUS_CONFIG);

    assert_eq!(
        find_paths(&unindexed, &["--eq", "status:active"]),
        find_paths(&indexed, &["--eq", "status:active"]),
    );
}

#[test]
fn not_eq_result_identical_whether_field_is_indexed_or_not() {
    let unindexed = small_vault();
    let indexed = small_vault();
    write_config(indexed.path(), INDEXED_STATUS_CONFIG);

    let args = ["--has", "status", "--not-eq", "status:active"];
    assert_eq!(find_paths(&unindexed, &args), find_paths(&indexed, &args));
}

#[test]
fn has_and_missing_results_identical_whether_field_is_indexed_or_not() {
    let unindexed = small_vault();
    let indexed = small_vault();
    write_config(indexed.path(), INDEXED_STATUS_CONFIG);

    assert_eq!(
        find_paths(&unindexed, &["--has", "status"]),
        find_paths(&indexed, &["--has", "status"]),
    );
    assert_eq!(
        find_paths(&unindexed, &["--missing", "status"]),
        find_paths(&indexed, &["--missing", "status"]),
    );
}

#[test]
fn in_and_not_in_results_identical_whether_field_is_indexed_or_not() {
    let unindexed = small_vault();
    let indexed = small_vault();
    write_config(indexed.path(), INDEXED_STATUS_CONFIG);

    let in_args = ["--in", "status:active,backlog"];
    assert_eq!(
        find_paths(&unindexed, &in_args),
        find_paths(&indexed, &in_args),
    );

    let not_in_args = ["--has", "status", "--not-in", "status:active"];
    assert_eq!(
        find_paths(&unindexed, &not_in_args),
        find_paths(&indexed, &not_in_args),
    );
}

#[test]
fn count_result_identical_whether_field_is_indexed_or_not() {
    let unindexed = small_vault();
    let indexed = small_vault();
    write_config(indexed.path(), INDEXED_STATUS_CONFIG);

    let run = |vault: &TempDir| -> serde_json::Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_norn"));
        command.arg("-C").arg(vault.path()).arg("count").args([
            "--eq",
            "status:active",
            "--format",
            "json",
        ]);
        let _cache = isolate_cache(&mut command);
        let output = command.output().expect("norn count should run");
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    };

    assert_eq!(run(&unindexed), run(&indexed));
}

// ── mixed non-string / string-op / date predicates still fall back ──────
// (router limitation, not an unindexed-field gap — silent, no warning)

#[test]
fn non_string_eq_result_identical_whether_field_is_indexed_or_not() {
    let unindexed = small_vault();
    let indexed = small_vault();
    write_config(
        indexed.path(),
        "validate:\n  rules:\n    - name: r\n      field_types:\n        n: { type: text, indexed: true }\n",
    );
    fs::write(
        unindexed.path().join("num.md"),
        "---\nn: [5, 6]\n---\nbody\n",
    )
    .unwrap();
    fs::write(indexed.path().join("num.md"), "---\nn: [5, 6]\n---\nbody\n").unwrap();
    fs::write(
        unindexed.path().join("num-scalar.md"),
        "---\nn: 5\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        indexed.path().join("num-scalar.md"),
        "---\nn: 5\n---\nbody\n",
    )
    .unwrap();

    assert_eq!(
        find_paths(&unindexed, &["--eq", "n:5"]),
        find_paths(&indexed, &["--eq", "n:5"]),
    );
}

// ── cold-scan warning ─────────────────────────────────────────────────

fn write_n_docs(root: &std::path::Path, n: usize) {
    for i in 0..n {
        fs::write(
            root.join(format!("doc{i:04}.md")),
            format!("---\nkind: k{}\n---\nbody\n", i % 7),
        )
        .unwrap();
    }
}

#[test]
fn warns_on_stderr_when_scanning_unindexed_field_over_1000_docs() {
    let vault = tempfile::Builder::new()
        .prefix("norn-idxroute-cold-")
        .tempdir()
        .unwrap();
    write_n_docs(vault.path(), 1000);
    // No config at all — `kind` is never in any index set.
    let (parsed, stderr) = find_json(&vault, &["--eq", "kind:k1"]);
    assert!(!parsed["documents"].as_array().unwrap().is_empty());
    assert!(
        stderr.contains("scanned 1000 documents on unindexed field(s) 'kind'"),
        "expected cold-scan warning, got stderr={stderr:?}"
    );
    assert!(
        stderr.contains("declare indexed: true"),
        "expected accelerate hint, got stderr={stderr:?}"
    );
}

#[test]
fn no_warning_under_1000_docs() {
    let vault = small_vault();
    let (_parsed, stderr) = find_json(&vault, &["--eq", "kind:log"]);
    assert!(
        !stderr.contains("unindexed field"),
        "small vault must not warn: stderr={stderr:?}"
    );
}

#[test]
fn no_warning_when_field_is_indexed_even_over_1000_docs() {
    let vault = tempfile::Builder::new()
        .prefix("norn-idxroute-indexed-1k-")
        .tempdir()
        .unwrap();
    write_n_docs(vault.path(), 1000);
    write_config(
        vault.path(),
        "validate:\n  rules:\n    - name: r\n      field_types:\n        kind: { type: string, indexed: true }\n",
    );
    let (_parsed, stderr) = find_json(&vault, &["--eq", "kind:k1"]);
    assert!(
        !stderr.contains("unindexed field"),
        "indexed field must not warn even over 1000 docs: stderr={stderr:?}"
    );
}

#[test]
fn no_warning_for_router_limitation_fallback_over_1000_docs() {
    // `--starts-with` always falls back to the scan path (router limitation,
    // not an unindexed-field gap) — silent regardless of doc count, even
    // when the field IS declared indexed.
    let vault = tempfile::Builder::new()
        .prefix("norn-idxroute-startswith-1k-")
        .tempdir()
        .unwrap();
    write_n_docs(vault.path(), 1000);
    write_config(
        vault.path(),
        "validate:\n  rules:\n    - name: r\n      field_types:\n        kind: { type: string, indexed: true }\n",
    );
    let (_parsed, stderr) = find_json(&vault, &["--starts-with", "kind:k"]);
    assert!(
        !stderr.contains("unindexed field"),
        "router-limitation fallback must stay silent: stderr={stderr:?}"
    );
}
