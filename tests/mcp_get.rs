//! Integration round-trip for the `vault.get` MCP tool (NRN-33, Task 3).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (the same process-level shape as `mcp_smoke.rs`, because `McpServer`
//! depends on `pub(crate)` types that can't be named from an external test
//! binary). Asserts:
//!
//! 1. `tools/list` now advertises `vault.get` with a non-empty `inputSchema`.
//! 2. `tools/call` for `vault.get` returns the seeded document's record.
//!
//! The pure handler is unit-tested separately inside `src/mcp/tools/get.rs`;
//! this test covers the rmcp wiring (router registration, schema, call dispatch).
//!
//! Non-flaky by construction: write all requests, then close stdin so the server
//! shuts down cleanly — no sleeps, no timeouts.

use std::io::Write as _;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-get-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("note.md"),
        "---\ntype: note\ntitle: Hello Note\nstatus: active\n---\nNote body\n",
    )
    .unwrap();
    tmp
}

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

#[test]
fn lists_and_calls_vault_get() {
    let vault = seeded_vault();

    prewrite_prune_marker(&vault.path().join(".xdg-cache"));
    let mut child = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"))
        .spawn()
        .expect("failed to spawn norn mcp");

    {
        let stdin = child.stdin.as_mut().expect("stdin not captured");
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "norn-get-client", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.get",
                    "arguments": { "targets": ["note"] }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "vault.get",
                    "arguments": { "targets": ["note"], "format": "markdown" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "vault.get",
                    "arguments": {
                        "targets": ["note", "note"],
                        "format": "markdown"
                    }
                }
            })))
            .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait on norn mcp");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "norn mcp exited non-zero ({})\nstdout: {}\nstderr: {}",
        output.status,
        stdout,
        stderr
    );

    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // ── tools/list (id=2): vault.get present with a non-empty inputSchema ──
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));

    let get_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.get")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.get, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    let schema = &get_tool["inputSchema"];
    assert!(
        schema.is_object(),
        "vault.get inputSchema must be an object, got: {schema}"
    );
    assert!(
        schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "vault.get inputSchema must have non-empty properties, got: {schema}"
    );
    assert!(
        schema["properties"].get("targets").is_some(),
        "vault.get inputSchema must expose a `targets` property, got: {schema}"
    );
    assert!(
        schema["properties"].get("format").is_some(),
        "vault.get inputSchema must expose a `format` property, got: {schema}"
    );

    // ── tools/call (id=3): returns the seeded record ──────────────────────
    let call_resp = responses
        .iter()
        .find(|r| r["id"] == 3)
        .unwrap_or_else(|| panic!("no tools/call response\nstdout: {stdout}\nstderr: {stderr}"));

    assert!(
        call_resp.get("error").is_none(),
        "vault.get call must not error, got: {call_resp}"
    );

    // The `Json<_>` wrapper places the payload in `structuredContent`.
    let structured = &call_resp["result"]["structuredContent"];
    let records = structured["records"]
        .as_array()
        .unwrap_or_else(|| panic!("result must carry a records array, got: {call_resp}"));

    assert_eq!(
        records.len(),
        1,
        "vault.get for `note` should return exactly one record, got: {call_resp}"
    );

    let rec = &records[0];
    assert!(
        rec["path"].as_str().unwrap_or("").ends_with("note.md"),
        "record path should end with note.md, got: {rec}"
    );
    assert_eq!(
        rec["frontmatter"]["title"].as_str(),
        Some("Hello Note"),
        "record frontmatter should reflect the seeded title, got: {rec}"
    );
    assert_eq!(
        rec["frontmatter"]["status"].as_str(),
        Some("active"),
        "record frontmatter should reflect the seeded status, got: {rec}"
    );

    // ── tools/call (id=4): exact Markdown uses a format-specific envelope ──
    let markdown_resp = responses
        .iter()
        .find(|r| r["id"] == 4)
        .unwrap_or_else(|| panic!("no markdown tools/call response\nstdout: {stdout}"));
    assert!(
        markdown_resp.get("error").is_none(),
        "markdown get must not protocol-error, got: {markdown_resp}"
    );
    let markdown = &markdown_resp["result"]["structuredContent"];
    assert_eq!(
        markdown["markdown"]["content"].as_str(),
        Some("---\ntype: note\ntitle: Hello Note\nstatus: active\n---\nNote body\n"),
        "markdown content must be exact on-disk source: {markdown_resp}"
    );
    assert!(
        markdown.get("raw").is_none() && markdown.get("source").is_none(),
        "exact source must not reappear as a raw/source structural field: {markdown}"
    );
    assert_eq!(
        markdown["records"],
        serde_json::json!([]),
        "markdown representation must not mix source into structured records"
    );

    // ── tools/call (id=5): Markdown refuses a multi-document selection ─────
    let multi_resp = responses
        .iter()
        .find(|r| r["id"] == 5)
        .unwrap_or_else(|| panic!("no multi-target tools/call response\nstdout: {stdout}"));
    assert_eq!(
        multi_resp["result"]["isError"], true,
        "multi-document markdown selection must be an MCP error result: {multi_resp}"
    );
    let multi = &multi_resp["result"]["structuredContent"];
    assert!(
        multi["notes"]
            .as_array()
            .is_some_and(|notes| notes.iter().any(|n| n
                .as_str()
                .is_some_and(|n| { n.contains("--format markdown returns a single document") }))),
        "multi-document refusal must explain the single-document contract: {multi_resp}"
    );
    assert!(
        multi.get("markdown").is_none(),
        "a refused multi-document request must not return source: {multi_resp}"
    );
}
