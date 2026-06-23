//! Integration tests for the `vault.audit` MCP tool (NRN-53, Task 4).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (same process-level shape as mcp_get.rs and mcp_set.rs). Asserts:
//!
//! 1. `tools/list` under `--read-only` advertises `vault.audit`.
//! 2. A confirmed `vault.set` mutation written in one server session is surfaced
//!    by `vault.audit` in a subsequent session.
//!
//! Non-flaky by construction: each session is a separate child process; stdin
//! is closed to signal the end of requests, and we wait for the process to exit
//! before reading the results. The two sessions are strictly serialized, so
//! vault.set always completes (and writes its event file) before vault.audit
//! reads it. Mirrors the batch-write / close-stdin / wait pattern from
//! mcp_set.rs and mcp_get.rs.

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

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

/// Pre-build the cache so vault.set can resolve documents (warm path).
/// Mirrors the pattern used in mcp_set.rs.
fn prebuild_cache(vault: &TempDir) {
    let status = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("cache")
        .arg("index")
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run norn cache index");
    assert!(status.success(), "cache index should succeed");
}

#[test]
fn vault_audit_listed_under_read_only() {
    let vault = tempfile::tempdir().unwrap();

    let mut child = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("mcp")
        .arg("--read-only")
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
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                            "clientInfo": { "name": "audit-list-client", "version": "0" } }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/list"
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

    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    assert!(
        names.contains(&"vault.audit"),
        "vault.audit must be available read-only; got {names:?}"
    );
}

#[test]
fn vault_audit_returns_a_persisted_mutation() {
    let vault = tempfile::Builder::new()
        .prefix("norn-mcp-audit-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        vault.path().join("note.md"),
        "---\ntype: note\nstatus: active\n---\nbody\n",
    )
    .unwrap();
    prebuild_cache(&vault);

    // ── Session 1: confirmed vault.set mutation → writes event file to disk ───
    //
    // Uses two separate server sessions to guarantee strict ordering: vault.set
    // fully completes (event file flushed and closed) before the second session
    // reads it back. A single-session approach is flaky because rmcp dispatches
    // requests to a tokio executor; vault.audit (a fast read-only call) can
    // acquire the call_lock before vault.set (which flushes an event file), so
    // the audit returns empty even when a mutation was queued ahead of it.
    {
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
            .expect("failed to spawn norn mcp (session 1)");

        {
            let stdin = child.stdin.as_mut().expect("stdin not captured");
            stdin
                .write_all(&line(serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": { "name": "audit-rt-set", "version": "0" } }
                })))
                .unwrap();
            // vault.set uses `set: { key: value }` map — matches mcp_set.rs shape exactly
            stdin
                .write_all(&line(serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {
                        "name": "vault.set",
                        "arguments": { "target": "note", "set": { "status": "done" }, "confirm": true }
                    }
                })))
                .unwrap();
        }
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .expect("wait on norn mcp (session 1)");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "norn mcp session 1 exited non-zero ({})\nstdout: {}\nstderr: {}",
            output.status,
            stdout,
            stderr
        );

        let responses: Vec<serde_json::Value> = stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let set_resp = responses.iter().find(|r| r["id"] == 2).unwrap_or_else(|| {
            panic!("no vault.set response in session 1\nstdout: {stdout}\nstderr: {stderr}")
        });
        assert!(
            set_resp.get("error").is_none(),
            "vault.set confirm:true must not error, got: {set_resp}"
        );
        let set_report = &set_resp["result"]["structuredContent"]["report"];
        assert_eq!(
            set_report["applied"].as_bool(),
            Some(true),
            "vault.set confirm:true must report applied=true, got: {set_resp}"
        );
    }

    // ── Session 2: vault.audit must surface the mutation from session 1 ──────
    {
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
            .expect("failed to spawn norn mcp (session 2)");

        {
            let stdin = child.stdin.as_mut().expect("stdin not captured");
            stdin
                .write_all(&line(serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": { "name": "audit-rt-read", "version": "0" } }
                })))
                .unwrap();
            stdin
                .write_all(&line(serde_json::json!({
                    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "vault.audit", "arguments": {} }
                })))
                .unwrap();
        }
        drop(child.stdin.take());

        let output = child
            .wait_with_output()
            .expect("wait on norn mcp (session 2)");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "norn mcp session 2 exited non-zero ({})\nstdout: {}\nstderr: {}",
            output.status,
            stdout,
            stderr
        );

        let responses: Vec<serde_json::Value> = stdout
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        // vault.audit result: structuredContent.events (matches mcp_get.rs envelope shape)
        let audit_resp = responses.iter().find(|r| r["id"] == 2).unwrap_or_else(|| {
            panic!("no vault.audit response in session 2\nstdout: {stdout}\nstderr: {stderr}")
        });
        assert!(
            audit_resp.get("error").is_none(),
            "vault.audit must not error, got: {audit_resp}"
        );

        let events = audit_resp["result"]["structuredContent"]["events"]
            .as_array()
            .unwrap_or_else(|| {
                panic!(
                    "vault.audit result must carry a structuredContent.events array, got: {audit_resp}"
                )
            });
        assert!(
            !events.is_empty(),
            "vault.audit should surface the set mutation from session 1: {audit_resp}"
        );
    }
}
