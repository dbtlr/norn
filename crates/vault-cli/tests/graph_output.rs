use std::fs;
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
    assert!(output.contains("deterministic, read-only view of raw Markdown vault structure"));
    assert!(output.contains("without applying standards-pack semantics or mutating files"));
    assert!(output.contains("Emit parsed Markdown documents"));
    assert!(output.contains("Emit inventoried vault files"));
    assert!(output.contains("Write a SQLite graph cache"));
    assert!(output.contains("Emit document parse diagnostics"));
}

#[test]
fn graph_short_help_stays_compact() {
    let output = vault(&["graph", "-h"]);
    assert!(output.contains("Query and cache derived Markdown vault graph facts"));
    assert!(!output.contains("without applying standards-pack semantics or mutating files"));
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
    assert!(output.contains("inventoried files"));
}

#[test]
fn graph_links_help_documents_obsidian_semantics() {
    let output = vault(&["graph", "links", "--help"]);
    assert!(output.contains("frontmatter/property wikilinks"));
    assert!(output.contains("same-note heading/block references"));
    assert!(output.contains("Markdown image links to local files"));
    assert!(output.contains("source_context.area"));
}

#[test]
fn graph_unresolved_help_documents_reasons() {
    let output = vault(&["graph", "unresolved", "--help"]);
    assert!(output.contains("target-missing"));
    assert!(output.contains("anchor-missing"));
    assert!(output.contains("block-ref-missing"));
    assert!(output.contains("ambiguous"));
}

#[test]
fn graph_backlinks_help_documents_file_targets() {
    let output = vault(&["graph", "backlinks", "--help"]);
    assert!(output.contains("non-Markdown files"));
    assert!(output.contains("Stem matching only applies to Markdown documents"));
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
    assert_eq!(alpha["links"].as_array().unwrap().len(), 18);
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[beta|Beta Note]]" && link["label"] == "Beta Note"));
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[#Alpha]]"
            && link["resolved_path"] == "alpha.md"
            && link["status"] == "resolved"));
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[#^alpha-block]]"
            && link["resolved_path"] == "alpha.md"
            && link["status"] == "resolved"));
    assert!(alpha["links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[beta#^block-a]]" && link["block_ref"] == "block-a"));
    assert_eq!(alpha["block_ids"][0], "alpha-block");
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
    assert_eq!(value["files"], 12);
    assert_eq!(value["ignored_files"], 0);
    assert_eq!(value["links"], 23);
    assert!(cache_path.exists());

    let connection = Connection::open(&cache_path).expect("cache should open");
    let document_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .unwrap();
    let file_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
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
    assert_eq!(file_count, 12);
    assert_eq!(link_count, 23);
    assert_eq!(missing_reason, "target-missing");
    assert_eq!(schema_version, "2");

    std::fs::remove_dir_all(cache_dir).ok();
}

#[test]
fn graph_config_ignores_files_before_indexing() {
    let root = temp_cache_dir();
    fs::create_dir_all(root.join("__pycache__")).expect("temp dirs should be created");
    fs::write(root.join("kept.md"), "# Kept\n\n[[ignored]]\n").expect("kept note should write");
    fs::write(root.join("ignored.md"), "# Ignored\n").expect("ignored note should write");
    fs::write(root.join("__pycache__/ignored.pyc"), "compiled").expect("ignored file should write");
    fs::write(
        root.join("vault.yaml"),
        "graph:\n  ignore:\n    - ignored.md\n    - __pycache__/**\n",
    )
    .expect("config should write");

    let documents = vault(&[
        "graph",
        "documents",
        "--root",
        root.to_str().unwrap(),
        "--config",
        root.join("vault.yaml").to_str().unwrap(),
        "--format",
        "json",
    ]);
    let documents = serde_json::from_str::<Value>(&documents).expect("output should be JSON");
    assert_eq!(documents.as_array().unwrap().len(), 1);
    assert_eq!(documents[0]["path"], "kept.md");

    let files = vault(&[
        "graph",
        "files",
        "--root",
        root.to_str().unwrap(),
        "--config",
        root.join("vault.yaml").to_str().unwrap(),
        "--format",
        "json",
    ]);
    let files = serde_json::from_str::<Value>(&files).expect("output should be JSON");
    assert_eq!(files.as_array().unwrap().len(), 2);
    assert!(files
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "kept.md"));
    assert!(files
        .as_array()
        .unwrap()
        .iter()
        .any(|file| file["path"] == "vault.yaml"));

    let build = vault(&[
        "graph",
        "build",
        "--root",
        root.to_str().unwrap(),
        "--config",
        root.join("vault.yaml").to_str().unwrap(),
        "--cache",
        root.join("cache.sqlite").to_str().unwrap(),
        "--format",
        "json",
    ]);
    let build = serde_json::from_str::<Value>(&build).expect("output should be JSON");
    assert_eq!(build["files"], 2);
    assert_eq!(build["ignored_files"], 2);
    assert_eq!(build["documents"], 1);

    fs::remove_dir_all(root).ok();
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
fn graph_files_jsonl_contract() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "files",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let files = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("line should be JSON"))
        .collect::<Vec<_>>();

    assert_eq!(files.len(), 12);
    assert!(files.iter().any(|file| file["path"] == "Assets/pic.png"
        && file["stem"] == "pic"
        && file["extension"] == "png"));
    assert!(files.iter().any(|file| file["path"] == "alpha.md"
        && file["stem"] == "alpha"
        && file["extension"] == "md"));
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

    assert_eq!(links.len(), 23);
    assert_eq!(links[0]["kind"], "markdown");
    assert_eq!(links[0]["source_span"]["line"], 20);
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
    let same_note_anchor = links
        .iter()
        .find(|link| link["raw"] == "[[#Alpha]]")
        .unwrap();
    let same_note_block = links
        .iter()
        .find(|link| link["raw"] == "[[#^alpha-block]]")
        .unwrap();
    let missing_same_note_anchor = links
        .iter()
        .find(|link| link["raw"] == "[[#Missing Same Heading]]")
        .unwrap();
    let missing_same_note_block = links
        .iter()
        .find(|link| link["raw"] == "[[#^missing-same-block]]")
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
    let path_qualified_case = links
        .iter()
        .find(|link| link["raw"] == "[[Other/Duplicate]]")
        .unwrap();
    let attachment_embed = links
        .iter()
        .find(|link| link["raw"] == "![[Assets/diagram.png]]")
        .unwrap();
    let markdown_image = links
        .iter()
        .find(|link| link["raw"] == "Assets/pic.png")
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
    assert_eq!(same_note_anchor["target"], "");
    assert_eq!(same_note_anchor["anchor"], "Alpha");
    assert_eq!(same_note_anchor["resolved_path"], "alpha.md");
    assert_eq!(same_note_anchor["status"], "resolved");
    assert_eq!(same_note_block["target"], "");
    assert_eq!(same_note_block["block_ref"], "alpha-block");
    assert_eq!(same_note_block["resolved_path"], "alpha.md");
    assert_eq!(same_note_block["status"], "resolved");
    assert_eq!(
        missing_same_note_anchor["unresolved_reason"],
        "anchor-missing"
    );
    assert_eq!(
        missing_same_note_block["unresolved_reason"],
        "block-ref-missing"
    );
    assert_eq!(missing_anchor["unresolved_reason"], "anchor-missing");
    assert_eq!(missing_block["unresolved_reason"], "block-ref-missing");
    assert_eq!(ambiguous["status"], "ambiguous");
    assert_eq!(ambiguous["unresolved_reason"], "ambiguous");
    assert_eq!(path_qualified_case["resolved_path"], "other/duplicate.md");
    assert_eq!(path_qualified_case["status"], "resolved");
    assert_eq!(attachment_embed["resolved_path"], "Assets/diagram.png");
    assert_eq!(attachment_embed["status"], "resolved");
    assert_eq!(markdown_image["kind"], "embed");
    assert_eq!(markdown_image["target"], "Assets/pic.png");
    assert_eq!(markdown_image["resolved_path"], "Assets/pic.png");
    assert_eq!(markdown_image["status"], "resolved");
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
    assert_eq!(links.as_array().unwrap().len(), 6);
    assert_eq!(links[0]["raw"], "[[missing]]");
    assert_eq!(links[0]["source_span"]["line"], 10);
    assert_eq!(links[0]["unresolved_reason"], "target-missing");
    assert_eq!(links[0]["status"], "unresolved");
    assert_eq!(links[1]["raw"], "[[#Missing Same Heading]]");
    assert_eq!(links[1]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[2]["raw"], "[[#^missing-same-block]]");
    assert_eq!(links[2]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[3]["raw"], "[[beta#Missing Heading]]");
    assert_eq!(links[3]["unresolved_reason"], "anchor-missing");
    assert_eq!(links[4]["raw"], "[[beta#^missing-block]]");
    assert_eq!(links[4]["unresolved_reason"], "block-ref-missing");
    assert_eq!(links[5]["raw"], "[[duplicate]]");
    assert_eq!(links[5]["source_span"]["line"], 26);
    assert_eq!(links[5]["status"], "ambiguous");
    assert_eq!(links[5]["unresolved_reason"], "ambiguous");
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
    assert_eq!(link["source_span"]["line"], 20);
}

#[test]
fn graph_backlinks_accepts_file_path() {
    let root = fixture_root();
    let output = vault(&[
        "graph",
        "backlinks",
        "Assets/pic.png",
        "--root",
        root.to_str().unwrap(),
        "--format",
        "jsonl",
    ]);

    let link = serde_json::from_str::<Value>(output.trim()).expect("output should be JSON");
    assert_eq!(link["kind"], "embed");
    assert_eq!(link["raw"], "Assets/pic.png");
    assert_eq!(link["resolved_path"], "Assets/pic.png");
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
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 6);
    assert!(value["incoming_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["raw"] == "[[#Alpha]]" && link["status"] == "resolved"));
    assert!(value["incoming_links"]
        .as_array()
        .unwrap()
        .iter()
        .any(|link| link["source_path"] == "beta.md"));
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 18);
    assert_eq!(
        value["unresolved_outgoing_links"].as_array().unwrap().len(),
        6
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
    assert_eq!(
        value["unresolved_outgoing_links"][3]["unresolved_reason"],
        "anchor-missing"
    );
    assert_eq!(
        value["unresolved_outgoing_links"][4]["unresolved_reason"],
        "block-ref-missing"
    );
    assert_eq!(value["unresolved_outgoing_links"][5]["target"], "duplicate");
    assert_eq!(
        value["unresolved_outgoing_links"][5]["unresolved_reason"],
        "ambiguous"
    );
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
