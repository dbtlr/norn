//! Smoke test for the `norn mcp` server scaffold (NRN-33, Task 1).
//!
//! Originally this drove `McpServer` via an in-process `tokio::io::duplex`
//! transport using `#[path]` to pull server.rs into the test binary. Task 2
//! wired a `VaultContext` into `McpServer`, which transitively depends on
//! `pub(crate)` types (`Cache`, `CacheError`, etc.) that cannot be named or
//! used from an external integration-test binary. The test is therefore
//! rewritten as a process-level test: spawn the `norn mcp` binary against a
//! minimal temp vault, perform the MCP `initialize` handshake, and assert the
//! server advertises the tools capability and lists zero tools.
//!
//! The contract being tested: the server must respond to `initialize` (with the
//! tools capability advertised) and to `tools/list`. Task 2 changed HOW the
//! binary is exercised (in-process → child process); Task 3 added the first real
//! tool (`vault.get`), so the list is no longer empty — this smoke test now
//! asserts the server lists at least its registered tool(s). The full `vault.get`
//! list+call round-trip lives in `tests/mcp_get.rs`.

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

fn make_minimal_vault() -> TempDir {
    tempfile::Builder::new()
        .prefix("norn-mcp-smoke-")
        .tempdir()
        .unwrap()
}

fn initialize_request() -> Vec<u8> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "norn-smoke-client",
                "version": "0.0.1"
            }
        }
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    bytes
}

fn tools_list_request() -> Vec<u8> {
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    bytes
}

#[test]
fn server_initializes_and_lists_tools() {
    let vault = make_minimal_vault();

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

    // Send initialize + tools/list then close stdin so the server shuts down.
    {
        let stdin = child.stdin.as_mut().expect("stdin not captured");
        stdin
            .write_all(&initialize_request())
            .expect("failed to write initialize");
        stdin
            .write_all(&tools_list_request())
            .expect("failed to write tools/list");
    }
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .expect("failed to wait on norn mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "norn mcp exited non-zero ({})\nstdout: {}\nstderr: {}",
        output.status,
        stdout,
        stderr
    );

    // Collect JSON-RPC responses.
    let responses: Vec<serde_json::Value> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();

    // Find the initialize response (id = 1).
    let init_resp = responses.iter().find(|r| r["id"] == 1).unwrap_or_else(|| {
        panic!(
            "no initialize response (id=1) in stdout\nstdout: {}\nstderr: {}",
            stdout, stderr
        )
    });

    // The InitializeResult must advertise the tools capability.
    assert!(
        init_resp["result"]["capabilities"].get("tools").is_some(),
        "InitializeResult must advertise tools capability, got: {}",
        init_resp
    );

    // Find the tools/list response (id = 2).
    let tools_resp = responses.iter().find(|r| r["id"] == 2).unwrap_or_else(|| {
        panic!(
            "no tools/list response (id=2) in stdout\nstdout: {}\nstderr: {}",
            stdout, stderr
        )
    });

    // tools/list must be a non-empty array now that Task 3 registered vault.get.
    let tools = tools_resp["result"]["tools"].as_array().unwrap_or_else(|| {
        panic!(
            "tools/list result.tools must be an array, got: {}",
            tools_resp
        )
    });

    assert!(
        tools
            .iter()
            .any(|t| t["name"].as_str() == Some("vault.get")),
        "tools/list must include vault.get, got: {:?}",
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );
}
