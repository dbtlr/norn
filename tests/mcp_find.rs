//! Integration round-trip for the `vault.find` MCP tool (NRN-33, Task 5).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (the same process-level shape as `mcp_count.rs`, because `McpServer`
//! depends on `pub(crate)` types that can't be named from an external test
//! binary). Asserts:
//!
//! 1. `tools/list` advertises `vault.find` with a non-empty `inputSchema`.
//! 2. `tools/call` for `vault.find` with `eq: ["type:note"]` returns the 2
//!    seeded notes, each carrying `path` + `frontmatter`.
//!
//! The pure handler is unit-tested separately inside `src/mcp/tools/find.rs`;
//! this test covers the rmcp wiring (router registration, schema, call dispatch).
//!
//! Non-flaky by construction: write all requests, then close stdin so the server
//! shuts down cleanly — no sleeps, no timeouts. The cache is pre-built before the
//! MCP child starts to avoid concurrent cold-start races (see `mcp_count.rs`).

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

/// 3 docs: 2 `type: note`, 1 `type: task`. Cache pre-built so the first MCP tool
/// call doesn't race a concurrent cold-start rebuild.
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-find-rt-")
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
fn lists_and_calls_vault_find() {
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
                    "clientInfo": { "name": "norn-find-client", "version": "0.0.1" }
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

        // 3. vault.find — eq type:note
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.find",
                    "arguments": { "eq": ["type:note"] }
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

    // The per-call served marker is a DAEMON-only observability channel
    // (`McpServer::new_daemon`, NRN-222 review): a stdio `norn mcp` process
    // must never write one — it would be mislabeled "norn serve" and pollute
    // the stdio client's stderr channel on every tools/call.
    assert!(
        !stderr.contains("served vault."),
        "stdio `norn mcp` must emit no served markers, got stderr:\n{stderr}"
    );

    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // ── tools/list (id=2): vault.find present with a non-empty inputSchema ──
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));

    let find_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.find")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.find, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    let schema = &find_tool["inputSchema"];
    assert!(
        schema.is_object(),
        "vault.find inputSchema must be an object, got: {schema}"
    );
    assert!(
        schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "vault.find inputSchema must have non-empty properties, got: {schema}"
    );
    assert!(
        schema["properties"].get("eq").is_some(),
        "vault.find inputSchema must expose an `eq` property, got: {schema}"
    );

    // ── tools/call (id=3): eq type:note → 2 notes ─────────────────────────
    let find_resp = responses.iter().find(|r| r["id"] == 3).unwrap_or_else(|| {
        panic!("no tools/call (id=3) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        find_resp.get("error").is_none(),
        "vault.find call must not error, got: {find_resp}"
    );

    let documents = find_resp["result"]["structuredContent"]["documents"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("vault.find result must carry a documents array, got: {find_resp}")
        });
    assert_eq!(
        documents.len(),
        2,
        "eq type:note should return 2 notes, got: {documents:?}"
    );
    for doc in documents {
        assert!(doc.get("path").is_some(), "each doc has a path: {doc}");
        assert_eq!(
            doc["frontmatter"]["type"], "note",
            "every returned doc is type:note: {doc}"
        );
    }
}

/// NRN-79: `vault.find` must return the identical result set whether the
/// queried field is declared `indexed` (routes through the derived
/// `document_fields` EAV table) or not (scan path) — MCP shares the same
/// `Cache::documents_matching` builder as the CLI, so this is the MCP-level
/// half of that non-negotiable (the CLI half lives in
/// `tests/find_index_routing.rs`).
#[test]
fn vault_find_result_identical_when_queried_field_is_indexed() {
    let vault = seeded_vault();
    std::fs::create_dir_all(vault.path().join(".norn")).unwrap();
    std::fs::write(
        vault.path().join(".norn/config.yaml"),
        "validate:\n  rules:\n    - name: r\n      field_types:\n        type: { type: string, indexed: true }\n",
    )
    .unwrap();

    // Re-run cache rebuild so the freshly-declared index set is reflected
    // (the seeded_vault() rebuild above ran before the config existed).
    let rebuild = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("cache")
        .arg("rebuild")
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"))
        .output()
        .expect("failed to run norn cache rebuild");
    assert!(rebuild.status.success());

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
                    "clientInfo": { "name": "norn-find-indexed-client", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.find",
                    "arguments": { "eq": ["type:note"] }
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
    let find_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/call response\nstdout: {stdout}\nstderr: {stderr}"));
    assert!(
        find_resp.get("error").is_none(),
        "vault.find call must not error, got: {find_resp}"
    );

    let mut paths: Vec<String> = find_resp["result"]["structuredContent"]["documents"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("vault.find result must carry a documents array, got: {find_resp}")
        })
        .iter()
        .map(|d| d["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();

    // Same 2 notes as the unindexed round-trip above (note1.md, note2.md).
    assert_eq!(paths, vec!["note1.md", "note2.md"]);
}
