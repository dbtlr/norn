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
    assert_eq!(documents[0]["links"].as_array().unwrap().len(), 5);
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

    assert_eq!(
        output,
        concat!(
            "{\"source_path\":\"alpha.md\",\"raw\":\"folder/delta.md#Delta-Heading\",\"kind\":\"markdown\",\"target\":\"folder/delta.md\",\"anchor\":\"Delta-Heading\",\"resolved_path\":\"folder/delta.md\",\"status\":\"resolved\"}\n",
            "{\"source_path\":\"alpha.md\",\"raw\":\"[[beta]]\",\"kind\":\"wikilink\",\"target\":\"beta\",\"resolved_path\":\"beta.md\",\"status\":\"resolved\"}\n",
            "{\"source_path\":\"alpha.md\",\"raw\":\"[[missing]]\",\"kind\":\"wikilink\",\"target\":\"missing\",\"status\":\"unresolved\"}\n",
            "{\"source_path\":\"alpha.md\",\"raw\":\"![[gamma]]\",\"kind\":\"embed\",\"target\":\"gamma\",\"resolved_path\":\"folder/gamma.md\",\"status\":\"resolved\"}\n",
            "{\"source_path\":\"alpha.md\",\"raw\":\"[[duplicate]]\",\"kind\":\"wikilink\",\"target\":\"duplicate\",\"candidates\":[\"duplicate.md\",\"other/duplicate.md\"],\"status\":\"ambiguous\"}\n",
            "{\"source_path\":\"beta.md\",\"raw\":\"[[alpha#Alpha]]\",\"kind\":\"wikilink\",\"target\":\"alpha\",\"anchor\":\"Alpha\",\"resolved_path\":\"alpha.md\",\"status\":\"resolved\"}\n",
        )
    );
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

    assert_eq!(
        output,
        concat!(
            "[\n",
            "  {\n",
            "    \"source_path\": \"alpha.md\",\n",
            "    \"raw\": \"[[missing]]\",\n",
            "    \"kind\": \"wikilink\",\n",
            "    \"target\": \"missing\",\n",
            "    \"status\": \"unresolved\"\n",
            "  },\n",
            "  {\n",
            "    \"source_path\": \"alpha.md\",\n",
            "    \"raw\": \"[[duplicate]]\",\n",
            "    \"kind\": \"wikilink\",\n",
            "    \"target\": \"duplicate\",\n",
            "    \"candidates\": [\n",
            "      \"duplicate.md\",\n",
            "      \"other/duplicate.md\"\n",
            "    ],\n",
            "    \"status\": \"ambiguous\"\n",
            "  }\n",
            "]\n",
        )
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

    assert_eq!(
        output,
        "{\"source_path\":\"alpha.md\",\"raw\":\"[[beta]]\",\"kind\":\"wikilink\",\"target\":\"beta\",\"resolved_path\":\"beta.md\",\"status\":\"resolved\"}\n"
    );
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

    assert_eq!(
        output,
        "{\"source_path\":\"alpha.md\",\"raw\":\"folder/delta.md#Delta-Heading\",\"kind\":\"markdown\",\"target\":\"folder/delta.md\",\"anchor\":\"Delta-Heading\",\"resolved_path\":\"folder/delta.md\",\"status\":\"resolved\"}\n"
    );
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
