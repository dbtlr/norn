//! Integration round-trip for the `vault.validate` MCP tool (NRN-33, Task 6).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (same process-level shape as `mcp_find.rs` — `McpServer` depends on
//! `pub(crate)` types that can't be named from an external test binary). Asserts:
//!
//! 1. `tools/list` advertises `vault.validate` with a non-empty `inputSchema`.
//! 2. `tools/call` for `vault.validate` against a vault with a broken wikilink
//!    returns at least one finding whose `code` is `link-target-missing`.
//!
//! The pure handler is unit-tested separately inside
//! `src/mcp/tools/validate.rs`; this test covers the rmcp wiring (router
//! registration, schema, call dispatch, envelope shape).
//!
//! Non-flaky by construction: write all requests, then close stdin so the server
//! shuts down cleanly — no sleeps, no timeouts. The cache is pre-built before
//! the MCP child starts to avoid concurrent cold-start races (NRN-55).

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

/// Vault with one doc that has a broken wikilink to a nonexistent target.
/// Cache is pre-built so the MCP server's first tool call doesn't race a
/// concurrent cold-start rebuild.
fn vault_with_broken_link() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-validate-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("source.md"),
        "---\ntype: note\ntitle: Source\n---\n\nSee [[MissingTarget]] for details.\n",
    )
    .unwrap();

    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
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
fn lists_and_calls_vault_validate() {
    let vault = vault_with_broken_link();

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

        // 1. initialize
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "norn-validate-client", "version": "0.0.1" }
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

        // 3. vault.validate — no filters → should return ≥1 finding
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.validate",
                    "arguments": {}
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

    // ── tools/list (id=2): vault.validate present with a non-empty inputSchema ──
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));

    let validate_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.validate")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.validate, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    let schema = &validate_tool["inputSchema"];
    assert!(
        schema.is_object(),
        "vault.validate inputSchema must be an object, got: {schema}"
    );
    assert!(
        schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "vault.validate inputSchema must have non-empty properties, got: {schema}"
    );

    // ── tools/call (id=3): broken-link vault → ≥1 link-target-missing finding ──
    let validate_resp = responses.iter().find(|r| r["id"] == 3).unwrap_or_else(|| {
        panic!("no tools/call (id=3) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        validate_resp.get("error").is_none(),
        "vault.validate call must not error, got: {validate_resp}"
    );

    let findings = validate_resp["result"]["structuredContent"]["findings"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("vault.validate result must carry a findings array, got: {validate_resp}")
        });

    assert!(
        !findings.is_empty(),
        "broken-link vault must return ≥1 finding, got 0\nfull response: {validate_resp}"
    );

    let link_finding = findings
        .iter()
        .find(|f| f["code"] == "link-target-missing")
        .unwrap_or_else(|| {
            panic!(
                "must include a link-target-missing finding, got codes: {:?}",
                findings
                    .iter()
                    .map(|f| f["code"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    assert!(
        link_finding.get("path").is_some(),
        "finding must carry a path: {link_finding}"
    );
    assert!(
        link_finding.get("message").is_some(),
        "finding must carry a message: {link_finding}"
    );
}
