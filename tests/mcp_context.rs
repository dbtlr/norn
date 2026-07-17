//! Integration test for the warm vault context used by the MCP server (NRN-33, Task 2).
//!
//! We exercise `VaultEnv` at the process level — seed a temp vault, start
//! `norn mcp` with `--cwd` pointing at it, pipe an MCP `initialize` request
//! over stdin, and assert the server responds with a valid JSON-RPC
//! `InitializeResult` (exit code 0 when stdin closes).
//!
//! Why process-level rather than in-process unit tests?
//!
//! `Cache` and `VaultEnv` are `pub(crate)` — the integration test binary
//! cannot hold them directly. The unit-level contracts (open succeeds, alias
//! field propagates, per-call freshness) live in `src/env/tests.rs` (the
//! vault-env seam's own unit suite). This file tests the observable contract:
//! the server starts successfully against a real vault with seeded docs and
//! responds to the MCP `initialize` handshake.

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
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

/// Create a temp vault with 3 seeded docs — enough to confirm the cache
/// opens and indexes real content.
fn make_seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-ctx-")
        .tempdir()
        .unwrap();

    let root = tmp.path();
    std::fs::write(
        root.join("alpha.md"),
        "---\ntype: note\nstatus: active\n---\nAlpha note body\n",
    )
    .unwrap();
    std::fs::write(
        root.join("beta.md"),
        "---\ntype: task\nstatus: backlog\n---\nBeta task body\n",
    )
    .unwrap();
    std::fs::write(
        root.join("gamma.md"),
        "---\ntype: log\nstatus: done\n---\nGamma log body\n",
    )
    .unwrap();

    tmp
}

/// Build a minimal JSON-RPC `initialize` request for the MCP protocol so the
/// server can complete the handshake. On stdin-close the server exits cleanly.
fn initialize_request() -> Vec<u8> {
    // MCP uses newline-delimited JSON-RPC over stdio.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "norn-test-client",
                "version": "0.0.1"
            }
        }
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    bytes
}

#[test]
fn mcp_server_starts_against_seeded_vault_and_handles_initialize() {
    let vault = make_seeded_vault();

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

    // Send the initialize request then close stdin so the server shuts down.
    {
        let stdin = child.stdin.as_mut().expect("stdin not captured");
        stdin
            .write_all(&initialize_request())
            .expect("failed to write initialize request");
    }
    // Drop stdin → EOF → server exits.
    drop(child.stdin.take());

    let output = child
        .wait_with_output()
        .expect("failed to wait on norn mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // The server must exit 0 (no crash during VaultEnv::open).
    assert!(
        output.status.success(),
        "norn mcp exited non-zero ({})\nstdout: {}\nstderr: {}",
        output.status,
        stdout,
        stderr
    );

    // The stdout must contain a JSON-RPC response to the initialize request.
    // The response is newline-delimited JSON; find the line with `result`.
    let response_line = stdout
        .lines()
        .find(|l| l.contains("\"result\""))
        .unwrap_or_else(|| {
            panic!(
                "no JSON-RPC result in stdout\nstdout: {}\nstderr: {}",
                stdout, stderr
            )
        });

    let parsed: serde_json::Value =
        serde_json::from_str(response_line).expect("response line is not valid JSON");

    assert_eq!(
        parsed["jsonrpc"], "2.0",
        "response must be JSON-RPC 2.0, got: {}",
        parsed
    );
    assert_eq!(
        parsed["id"], 1,
        "response id must match request id, got: {}",
        parsed
    );

    // The result must include the server's capabilities (tools enabled).
    let capabilities = &parsed["result"]["capabilities"];
    assert!(
        capabilities.get("tools").is_some(),
        "InitializeResult must advertise tools capability, got capabilities: {}",
        capabilities
    );
}
