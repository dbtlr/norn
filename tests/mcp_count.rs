//! Integration round-trip for the `vault.count` MCP tool (NRN-33, Task 4).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (the same process-level shape as `mcp_get.rs`, because `McpServer`
//! depends on `pub(crate)` types that can't be named from an external test
//! binary). Asserts:
//!
//! 1. `tools/list` advertises `vault.count` with a non-empty `inputSchema`.
//! 2. `tools/call` for `vault.count` (no filter) returns total == 3.
//! 3. `tools/call` for `vault.count` grouped by `type` returns groups
//!    `{note: 2, task: 1}` with total == 3.
//!
//! The pure handler is unit-tested separately inside `src/mcp/tools/count.rs`;
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

/// 3 docs: 2 `type: note`, 1 `type: task`.
///
/// The cache is pre-built (`norn cache rebuild`) before the test drives the MCP
/// server. This ensures the first MCP tool call does not hit the incremental
/// rebuild path on a fresh vault, which can race with a concurrent call when the
/// MCP server dispatches two tool requests in parallel. Pre-building is the
/// correct production pattern anyway — operators (Claude Desktop, etc.) are
/// expected to have the cache warm before MCP sessions. The concurrent-cold-start
/// issue is tracked as a known architecture limitation of the per-call-freshness
/// design.
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-count-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("note1.md"),
        "---\ntype: note\ntitle: Note One\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("note2.md"),
        "---\ntype: note\ntitle: Note Two\n---\nbody\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("task1.md"),
        "---\ntype: task\ntitle: Task One\n---\nbody\n",
    )
    .unwrap();

    // Pre-build the cache so concurrent MCP tool calls don't race on the
    // initial rebuild of a fresh vault.
    let rebuild = Command::new(norn_bin())
        .arg("--cwd")
        .arg(tmp.path())
        .arg("cache")
        .arg("rebuild")
        .env("XDG_CACHE_HOME", tmp.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", tmp.path().join(".xdg-state"))
        .output()
        .expect("failed to run norn cache rebuild");
    assert!(
        rebuild.status.success(),
        "norn cache rebuild failed: {}",
        String::from_utf8_lossy(&rebuild.stderr)
    );

    tmp
}

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

#[test]
fn lists_and_calls_vault_count() {
    let vault = seeded_vault();

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

        // 1. initialize
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "norn-count-client", "version": "0.0.1" }
                }
            })))
            .unwrap();

        // 2. tools/list
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            })))
            .unwrap();

        // 3. vault.count — no filter, no by → total only
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.count",
                    "arguments": {}
                }
            })))
            .unwrap();

        // 4. vault.count — grouped by type
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "vault.count",
                    "arguments": { "by": "type" }
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

    // ── tools/list (id=2): vault.count present with a non-empty inputSchema ──
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));

    let count_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.count")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.count, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    let schema = &count_tool["inputSchema"];
    assert!(
        schema.is_object(),
        "vault.count inputSchema must be an object, got: {schema}"
    );
    assert!(
        schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "vault.count inputSchema must have non-empty properties, got: {schema}"
    );
    assert!(
        schema["properties"].get("by").is_some(),
        "vault.count inputSchema must expose a `by` property, got: {schema}"
    );

    // ── tools/call (id=3): total count — no filter ────────────────────────
    let total_resp = responses.iter().find(|r| r["id"] == 3).unwrap_or_else(|| {
        panic!("no tools/call (id=3) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        total_resp.get("error").is_none(),
        "vault.count (total) call must not error, got: {total_resp}"
    );

    let structured = &total_resp["result"]["structuredContent"];
    assert_eq!(
        structured["total"].as_u64(),
        Some(3),
        "vault.count total should be 3 for 3 seeded docs, got: {total_resp}"
    );
    assert!(
        structured["by"].is_null() || structured.get("by").is_none(),
        "vault.count (total mode) should not set `by`, got: {structured}"
    );
    assert!(
        structured["groups"].is_null() || structured.get("groups").is_none(),
        "vault.count (total mode) should not set `groups`, got: {structured}"
    );

    // ── tools/call (id=4): grouped by type ───────────────────────────────
    let grouped_resp = responses.iter().find(|r| r["id"] == 4).unwrap_or_else(|| {
        panic!("no tools/call (id=4) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        grouped_resp.get("error").is_none(),
        "vault.count (grouped) call must not error, got: {grouped_resp}"
    );

    let structured = &grouped_resp["result"]["structuredContent"];
    assert_eq!(
        structured["total"].as_u64(),
        Some(3),
        "grouped total should be 3, got: {grouped_resp}"
    );
    assert_eq!(
        structured["by"].as_str(),
        Some("type"),
        "grouped `by` field should be 'type', got: {structured}"
    );

    let groups = structured["groups"]
        .as_object()
        .unwrap_or_else(|| panic!("grouped result must have a `groups` object, got: {structured}"));
    assert_eq!(
        groups.get("note").and_then(|v| v.as_u64()),
        Some(2),
        "note group should have count 2, got: {groups:?}"
    );
    assert_eq!(
        groups.get("task").and_then(|v| v.as_u64()),
        Some(1),
        "task group should have count 1, got: {groups:?}"
    );
    assert_eq!(
        groups.len(),
        2,
        "expected exactly 2 groups (note, task), got: {groups:?}"
    );
}
