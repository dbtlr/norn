use std::path::PathBuf;
use std::process::Command;

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

    assert_eq!(documents.len(), 7);
    assert_eq!(documents[0]["path"], "alpha.md");
    assert_eq!(documents[0]["frontmatter"]["title"], "Alpha");
    assert_eq!(documents[0]["headings"][0]["slug"], "alpha");
    assert_eq!(documents[0]["headings"][0]["source_span"]["line"], 8);
    assert_eq!(documents[0]["links"].as_array().unwrap().len(), 6);
    assert_eq!(documents[0]["links"][1]["label"], "Beta Note");
    assert_eq!(documents[0]["links"][3]["block_ref"], "block-a");
    assert_eq!(documents[2]["path"], "broken-frontmatter.md");
    assert_eq!(
        documents[2]["diagnostics"][0],
        serde_json::json!({
            "severity": "warning",
            "code": "frontmatter-parse-failed",
            "message": "frontmatter could not be parsed"
        })
    );
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

    assert_eq!(links.len(), 7);
    assert_eq!(links[0]["kind"], "markdown");
    assert_eq!(links[0]["source_span"]["line"], 14);
    assert_eq!(links[1]["raw"], "[[beta|Beta Note]]");
    assert_eq!(links[1]["label"], "Beta Note");
    assert_eq!(links[1]["resolved_path"], "beta.md");
    assert_eq!(links[3]["raw"], "[[beta#^block-a]]");
    assert_eq!(links[3]["block_ref"], "block-a");
    assert_eq!(links[5]["status"], "ambiguous");
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
    assert_eq!(links.as_array().unwrap().len(), 2);
    assert_eq!(links[0]["raw"], "[[missing]]");
    assert_eq!(links[0]["source_span"]["line"], 10);
    assert_eq!(links[0]["status"], "unresolved");
    assert_eq!(links[1]["raw"], "[[duplicate]]");
    assert_eq!(links[1]["source_span"]["line"], 16);
    assert_eq!(links[1]["status"], "ambiguous");
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
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["raw"], "[[beta|Beta Note]]");
    assert_eq!(links[0]["label"], "Beta Note");
    assert_eq!(links[1]["raw"], "[[beta#^block-a]]");
    assert_eq!(links[1]["block_ref"], "block-a");
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
    assert_eq!(link["source_span"]["line"], 14);
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
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 1);
    assert_eq!(value["incoming_links"][0]["source_path"], "beta.md");
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 6);
    assert_eq!(
        value["unresolved_outgoing_links"].as_array().unwrap().len(),
        2
    );
    assert_eq!(value["unresolved_outgoing_links"][0]["target"], "missing");
    assert_eq!(value["unresolved_outgoing_links"][1]["target"], "duplicate");
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
    assert_eq!(value["incoming_links"].as_array().unwrap().len(), 2);
    assert_eq!(value["outgoing_links"].as_array().unwrap().len(), 1);
    assert_eq!(
        value["outgoing_links"][0]["resolved_path"],
        serde_json::json!("alpha.md")
    );
}
