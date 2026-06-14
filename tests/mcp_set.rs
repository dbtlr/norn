//! Integration round-trip for the `vault.set` MCP tool (NRN-33, Task 9).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault, exercising the **mutation-safety contract** end-to-end over JSON-RPC:
//!
//! 1. `tools/call` `vault.set` with `confirm: false` (the default) → the
//!    response reports `applied = false` AND the file on disk is UNCHANGED.
//! 2. `tools/call` `vault.set` with `confirm: true` → the response reports
//!    `applied = true` AND the file on disk now reflects the change.
//!
//! The dry-run-writes-nothing assertion is the critical safety property: it reads
//! the file back from disk after the dry-run call and asserts the original value
//! is intact.
//!
//! The pure handler is unit-tested inside `src/mcp/tools/set.rs`; this test covers
//! the rmcp wiring (router registration, schema, call dispatch) and the disk
//! effect of the contract through a real process.
//!
//! Non-flaky by construction: write all requests in order, then close stdin so
//! the server shuts down cleanly — no sleeps, no timeouts. Because every tool
//! call is serialized through the server's in-process `call_lock`, the two
//! sequential calls apply in request order, so the disk reads are deterministic.

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
        .prefix("norn-mcp-set-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("task.md"),
        "---\ntype: task\nstatus: backlog\n---\nTask body\n",
    )
    .unwrap();
    tmp
}

/// Pre-build the cache against the vault so the server's first mutating call sees
/// a fresh index without doing a cold-start build inside the apply path. The
/// per-call freshness check inside the handler keeps it current regardless; this
/// just exercises the warm path the real client hits.
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

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn disk_status(vault: &TempDir) -> String {
    let content = std::fs::read_to_string(vault.path().join("task.md")).unwrap();
    for l in content.lines() {
        if let Some(rest) = l.strip_prefix("status:") {
            return rest.trim().to_string();
        }
    }
    panic!("no status field in task.md:\n{content}");
}

#[test]
fn dry_run_then_confirm_roundtrip() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

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
                    "clientInfo": { "name": "norn-set-client", "version": "0.0.1" }
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
        // Dry-run (confirm omitted → default false): plan only, write nothing.
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.set",
                    "arguments": {
                        "target": "task",
                        "set": { "status": "active" }
                    }
                }
            })))
            .unwrap();
        // Confirm: apply the change to disk.
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "vault.set",
                    "arguments": {
                        "target": "task",
                        "set": { "status": "active" },
                        "confirm": true
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

    // ── tools/list (id=2): vault.set present with a `confirm` property ────────
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));
    let set_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.set")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.set, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });
    let schema = &set_tool["inputSchema"];
    assert!(
        schema["properties"].get("confirm").is_some(),
        "vault.set inputSchema must expose a `confirm` property, got: {schema}"
    );

    // ── tools/call (id=3): DRY-RUN — applied=false, file UNCHANGED ────────────
    let dry = responses
        .iter()
        .find(|r| r["id"] == 3)
        .unwrap_or_else(|| panic!("no dry-run response\nstdout: {stdout}\nstderr: {stderr}"));
    assert!(
        dry.get("error").is_none(),
        "dry-run vault.set must not error, got: {dry}"
    );
    let dry_report = &dry["result"]["structuredContent"]["report"];
    assert_eq!(
        dry_report["applied"].as_bool(),
        Some(false),
        "dry-run report must have applied == false, got: {dry}"
    );

    // ── tools/call (id=4): CONFIRM — applied=true ─────────────────────────────
    let confirm = responses
        .iter()
        .find(|r| r["id"] == 4)
        .unwrap_or_else(|| panic!("no confirm response\nstdout: {stdout}\nstderr: {stderr}"));
    assert!(
        confirm.get("error").is_none(),
        "confirm vault.set must not error, got: {confirm}"
    );
    let confirm_report = &confirm["result"]["structuredContent"]["report"];
    assert_eq!(
        confirm_report["applied"].as_bool(),
        Some(true),
        "confirm report must have applied == true, got: {confirm}"
    );

    // ── Final disk state: the confirm write landed; status is now `active`. ───
    // (After both calls completed, the server has exited.) The dry-run's
    // applied=false combined with the confirm's applied=true and the final
    // on-disk `active` together prove the contract: the only write came from the
    // confirm call.
    assert_eq!(
        disk_status(&vault),
        "active",
        "after the confirm call, disk status must be `active`"
    );
}

/// Dedicated dry-run safety check: a `confirm:false` call against a fresh vault
/// leaves the file byte-identical. This isolates the "writes nothing" property
/// from the confirm call so a regression that wrote on dry-run can't hide behind
/// a later confirm write.
#[test]
fn dry_run_alone_writes_nothing() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let before = std::fs::read_to_string(vault.path().join("task.md")).unwrap();

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
                    "clientInfo": { "name": "norn-set-client", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.set",
                    "arguments": {
                        "target": "task",
                        "set": { "status": "active" },
                        "confirm": false
                    }
                }
            })))
            .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait on norn mcp");
    assert!(output.status.success(), "norn mcp exited non-zero");

    let after = std::fs::read_to_string(vault.path().join("task.md")).unwrap();
    assert_eq!(
        before, after,
        "dry-run (confirm:false) must leave the file byte-for-byte unchanged"
    );
}
