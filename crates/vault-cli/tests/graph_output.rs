use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use serde_json::Value;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/basic")
}

fn vault(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args(args)
        .output()
        .expect("vault command should run");

    assert!(
        output.status.success(),
        "vault command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("stdout should be UTF-8")
}

fn vault_error(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_vault"))
        .args(args)
        .output()
        .expect("vault command should run");

    assert!(
        !output.status.success(),
        "vault command succeeded unexpectedly\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stderr).expect("stderr should be UTF-8")
}

fn temp_cache_dir() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("vault-cli-cache-{unique}"))
}

#[test]
fn vault_version_reports_package_version() {
    let output = vault(&["--version"]);
    assert!(output.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn graph_help_describes_contracts() {
    let output = vault(&["graph", "--help"]);
    assert!(output.contains("Query and cache derived Markdown vault graph facts"));
    assert!(output.contains("Emit parsed Markdown documents"));
    assert!(output.contains("Write a SQLite graph cache"));
    assert!(output.contains("Emit document parse diagnostics"));
}

#[test]
fn graph_documents_help_documents_frontmatter_filter() {
    let output = vault(&["graph", "documents", "--help"]);
    assert!(output.contains("Frontmatter-only field:value filter"));
}

#[test]
fn graph_build_help_documents_cache_semantics() {
    let output = vault(&["graph", "build", "--help"]);
    assert!(output.contains("SQLite cache file path or directory"));
    assert!(output.contains("--format only controls stdout"));
}

#[test]
fn graph_documents_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "documents",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 10);
    let alpha = documents
        .iter()
        .find(|document| document["path"] == "alpha.md")
        .unwrap();
    let beta = documents
        .iter()
        .find(|document| document["path"] == "beta.md")
        .unwrap();
    let broken = documents
        .iter()
        .find(|document| document["path"] == "broken-frontmatter.md")
        .unwrap();
    let frontmatter_source = documents
        .iter()
        .find(|document| document["path"] == "frontmatter-source.md")
        .unwrap();

    assert_eq!(alpha["frontmatter"]["title"], "Alpha");
    assert_eq!(alpha["headings"][0]["slug"], "alpha");
    assert_eq!(alpha["headings"][0]["source_span"]["line"], 8);
    assert_eq!(alpha["links"].as_array().unwrap().len(), 11);
    assert_eq!(alpha["links"][4]["label"], "Beta Note");
    assert_eq!(alpha["links"][6]["block_ref"], "block-a");
    assert_eq!(beta["block_ids"][0], "block-a");
    assert_eq!(frontmatter_source["links"].as_array().unwrap().len(), 4);
    assert_eq!(
        frontmatter_source["links"][1]["source_context"],
        serde_json::json!({"area": "frontmatter", "property": "related"})
    );
    assert_eq!(
        frontmatter_source["links"][2]["source_context"],
        serde_json::json!({"area": "frontmatter", "property": "related_list"})
    );
    assert_eq!(
        broken["diagnostics"][0],
        serde_json::json!({
            "severity": "warning",
            "code": "frontmatter-parse-failed",
            "message": "frontmatter could not be parsed"
        })
    );
}

#[test]
fn graph_build_writes_sqlite_cache() {
    let root = fixture_root();
    let cache_dir = temp_cache_dir();
    let output = vault(&[
        "graph",
        "build",
        "--root",
        root.to_str().unwrap(),
        "--cache",
        cache_dir.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    let cache_path = cache_dir.join("graph.sqlite");
    assert_eq!(value["cache_path"], cache_path.to_str().unwrap());
    assert_eq!(value["documents"], 10);
    assert_eq!(value["links"], 16);
    assert!(cache_path.exists());

    let connection = Connection::open(&cache_path).expect("cache should open");
    let document_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .unwrap();
    let link_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM links", [], |row| row.get(0))
        .unwrap();
    let missing_reason: String = connection
        .query_row(
            "SELECT unresolved_reason FROM links WHERE raw = '[[missing]]'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let schema_version: String = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(document_count, 10);
    assert_eq!(link_count, 16);
    assert_eq!(missing_reason, "target-missing");
    assert_eq!(schema_version, "1");

    std::fs::remove_dir_all(cache_dir).ok();
}

#[test]
fn graph_build_accepts_sqlite_file_path() {
    let root = fixture_root();
    let cache_path = temp_cache_dir().with_extension("sqlite");
    let output = vault(&[
        "graph",
        "build",
        "--root",
        root.to_str().unwrap(),
        "--cache",
        cache_path.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["cache_path"], cache_path.to_str().unwrap());
    assert!(cache_path.exists());

    let connection = Connection::open(&cache_path).expect("cache should open");
    let document_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(document_count, 10);

    std::fs::remove_file(cache_path).ok();
}

#[test]
fn graph_documents_filters_frontmatter_scalars() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "documents",
        "--root",
        root.to_str().unwrap(),
        "--filter",
        "status:draft",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["path"], "alpha.md");
}

#[test]
fn graph_documents_filters_frontmatter_lists() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "documents",
        "--root",
        root.to_str().unwrap(),
        "--filter",
        "aliases:First Note",
        "--format",
        "jsonl",
    ]);

    let documents = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["path"], "alpha.md");
}

#[test]
fn graph_documents_rejects_invalid_filters() {
    let root = fixture_root();
    let stderr = vault_error(&[
        "graph",
        "documents",
        "--root",
        root.to_str().unwrap(),
        "--filter",
        "status",
        "--format",
        "jsonl",
    ]);

    assert!(stderr.contains("invalid filter, expected field:value"));
}

#[test]
fn graph_links_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "links",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let links = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(links.len(), 16);
    assert_eq!(links[0]["kind"], "markdown");
    assert_eq!(links[0]["source_span"]["line"], 16);
    let encoded_with_ext = links
        .iter()
        .find(|link| link["raw"] == "Markdown%20Target.md")
        .unwrap();
    let encoded_without_ext = links
        .iter()
        .find(|link| link["raw"] == "Markdown%20Target")
        .unwrap();
    let encoded_anchor = links
        .iter()
        .find(|link| link["raw"] == "Markdown%20Target.md#Encoded%20Heading")
        .unwrap();
    let beta_alias = links
        .iter()
        .find(|link| link["raw"] == "[[beta|Beta Note]]")
        .unwrap();
    let block_ref = links
        .iter()
        .find(|link| link["raw"] == "[[beta#^block-a]]")
        .unwrap();
    let missing_anchor = links
        .iter()
        .find(|link| link["raw"] == "[[beta#Missing Heading]]")
        .unwrap();
    let missing_block = links
        .iter()
        .find(|link| link["raw"] == "[[beta#^missing-block]]")
        .unwrap();
    let ambiguous = links
        .iter()
        .find(|link| link["raw"] == "[[duplicate]]")
        .unwrap();
    let property_target = links
        .iter()
        .find(|link| link["raw"] == "[[Front Target]]")
        .unwrap();

    assert_eq!(encoded_with_ext["target"], "Markdown Target.md");
    assert_eq!(encoded_with_ext["resolved_path"], "Markdown Target.md");
    assert_eq!(encoded_without_ext["target"], "Markdown Target");
    assert_eq!(encoded_without_ext["resolved_path"], "Markdown Target.md");
    assert_eq!(encoded_anchor["anchor"], "Encoded Heading");
    assert_eq!(encoded_anchor["resolved_path"], "Markdown Target.md");
    assert_eq!(beta_alias["label"], "Beta Note");
    assert_eq!(beta_alias["resolved_path"], "beta.md");
    assert_eq!(block_ref["block_ref"], "block-a");
    assert_eq!(missing_anchor["unresolved_reason"], "anchor-missing");
    assert_eq!(missing_block["unresolved_reason"], "block-ref-missing");
    assert_eq!(ambiguous["status"], "ambiguous");
    assert_eq!(property_target["raw"], "[[Front Target]]");
    assert_eq!(
        property_target["source_context"],
        serde_json::json!({"area": "frontmatter", "property": "related"})
    );
    assert_eq!(property_target["resolved_path"], "Front Target.md");
    assert!(links
        .iter()
        .any(|link| link["label"] == "Displayed in property"));
    assert!(!links.iter().any(|link| link["target"] == "inline-example"));
    assert!(!links.iter().any(|link| link["target"] == "fenced-example"));
}

#[test]
fn graph_unresolved_json_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "unresolved",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let links = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(links.as_array().unwrap().len(), 4);
    assert_eq!(links[0]["raw"], "[[missing]]");
    assert_eq!(links[0]["source_span"]["line"], 10);
    assert_eq!(links[0]["unresolved_reason"], "target-missing");
    assert_eq!(links[0]["status"], "unresolved");
    assert_eq!(links[1]["raw"], "[[beta#Missing Heading]]");
    assert_eq!(links[1]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[2]["raw"], "[[beta#^missing-block]]");
    assert_eq!(links[2]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[3]["raw"], "[[duplicate]]");
    assert_eq!(links[3]["source_span"]["line"], 18);
    assert_eq!(links[3]["status"], "ambiguous");
}

#[test]
fn graph_diagnostics_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "diagnostics",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let diagnostics = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["path"], "broken-frontmatter.md");
    assert_eq!(
        diagnostics[0]["diagnostic"]["code"],
        "frontmatter-parse-failed"
    );
}

#[test]
fn graph_backlinks_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "backlinks",
        "beta",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let links = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 5);
    assert_eq!(links[0]["raw"], "[[beta|Beta Note]]");
    assert_eq!(links[0]["label"], "Beta Note");
    assert_eq!(links[1]["raw"], "[[beta#^block-a]]");
    assert_eq!(links[1]["block_ref"], "block-a");
    assert_eq!(links[2]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[3]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[4]["source_context"]["area"], "frontmatter");
    assert_eq!(links[4]["source_context"]["property"], "related_list");
}

#[test]
fn graph_backlinks_accepts_exact_path() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "backlinks",
        "folder/delta.md",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let link = serde_json::from_str::<Value>(output.trim()).expect("output should be JSON");
    assert_eq!(link["kind"], "markdown");
    assert_eq!(link["anchor"], "Delta-Heading");
    assert_eq!(link["source_span"]["line"], 16);
}

#[test]
fn graph_backlinks_accepts_case_insensitive_stem() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "backlinks",
        "BETA",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let links = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 5);
    assert_eq!(links[0]["resolved_path"], "beta.md");
}

#[test]
fn graph_inspect_accepts_case_insensitive_stem() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "inspect",
        "ALPHA",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["document"]["path"], "alpha.md");
}

#[test]
fn graph_backlinks_rejects_ambiguous_stem() {
    let root = fixture_root();
    let stderr = vault_error(&[
        "graph",
        "backlinks",
        "duplicate",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    assert!(stderr.contains("ambiguous document stem: duplicate"));
    assert!(stderr.contains("duplicate.md"));
    assert!(stderr.contains("other/duplicate.md"));
}

#[test]
fn graph_inspect_json_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "inspect",
        "alpha.md",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "json",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["document"]["path"], "alpha.md");
    assert_eq!(value["document"]["frontmatter"]["title"], "Alpha");
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 2);
    assert_eq!(value["incoming_links"][0]["source_path"], "beta.md");
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 11);
    assert_eq!(
        value["unresolved_outgoing_links"].as_array().unwrap().len(),
        4
    );
    assert_eq!(value["unresolved_outgoing_links"][0]["target"], "missing");
    assert_eq!(
        value["unresolved_outgoing_links"][1]["unresolved_reason"],
        "anchor-missing"
    );
    assert_eq!(
        value["unresolved_outgoing_links"][2]["unresolved_reason"],
        "block-ref-missing"
    );
    assert_eq!(value["unresolved_outgoing_links"][3]["target"], "duplicate");
}

#[test]
fn graph_inspect_accepts_unique_stem() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "inspect",
        "beta",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let value = serde_json::from_str::<Value>(&output).expect("output should be JSON");
    assert_eq!(value["document"]["path"], "beta.md");
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 5);
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["outgoing_links"][0]["resolved_path"],
        serde_json::json!("alpha.md")
    );
}
