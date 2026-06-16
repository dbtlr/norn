//! `norn mcp --read-only` gating round-trip (NRN-33, Task 13).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault, exercising the read-only contract end-to-end over JSON-RPC:
//!
//! 1. With `--read-only`: `tools/list` advertises EXACTLY the 6 read tools and
//!    NONE of the 7 mutation tools (drop-from-list, requirement 1).
//! 2. With `--read-only`: a `tools/call` for a mutation tool (`vault.set`) ERRORS
//!    AND the file on disk is byte-for-byte UNCHANGED (runtime refusal + writes
//!    nothing, requirement 2).
//! 3. Without the flag (default): `tools/list` advertises ALL 13 tools — the
//!    non-read-only path is unchanged (requirement 3, the regression guard).
//!
//! Child-process driven, cache pre-built, XDG isolated — same shape as the other
//! `tests/mcp_*.rs` round-trips. Non-flaky by construction: write all requests in
//! order, then close stdin so the server shuts down cleanly.

use std::io::Write as _;
use std::process::{Command, Stdio};
use tempfile::TempDir;

/// The 6 read tools — always advertised, even under `--read-only`.
const READ_TOOLS: &[&str] = &[
    "vault.find",
    "vault.count",
    "vault.get",
    "vault.validate",
    "vault.repair_plan",
    "vault.describe",
];

/// The 7 mutation tools — dropped from `tools/list` under `--read-only`.
const MUTATION_TOOLS: &[&str] = &[
    "vault.new",
    "vault.set",
    "vault.edit",
    "vault.move",
    "vault.delete",
    "vault.rewrite_wikilink",
    "vault.apply_plan",
];

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-read-only-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("task.md"),
        "---\ntype: task\nstatus: backlog\n---\nTask body\n",
    )
    .unwrap();
    tmp
}

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

/// Spawn `norn mcp` (optionally `--read-only`) against `vault`, feed the given
/// request lines, close stdin, and return the parsed JSON-RPC responses.
fn drive(
    vault: &TempDir,
    read_only: bool,
    requests: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut cmd = Command::new(norn_bin());
    cmd.arg("--cwd").arg(vault.path()).arg("mcp");
    if read_only {
        cmd.arg("--read-only");
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"))
        .spawn()
        .expect("failed to spawn norn mcp");

    {
        let stdin = child.stdin.as_mut().expect("stdin not captured");
        for req in requests {
            stdin.write_all(&line(req.clone())).unwrap();
        }
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

    stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn initialize() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "norn-read-only-client", "version": "0.0.1" }
        }
    })
}

fn tools_list(id: u32) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": {}
    })
}

fn tool_names(responses: &[serde_json::Value], id: u32) -> Vec<String> {
    let resp = responses
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no tools/list response (id={id}) in {responses:?}"));
    resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {resp}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// `--read-only`: `tools/list` lists EXACTLY the 6 read tools, NO mutation tools.
#[test]
fn read_only_lists_only_read_tools() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

    let responses = drive(
        &vault,
        /*read_only=*/ true,
        &[initialize(), tools_list(2)],
    );
    let names = tool_names(&responses, 2);

    for read in READ_TOOLS {
        assert!(
            names.iter().any(|n| n == read),
            "read-only tools/list must include {read}, got: {names:?}"
        );
    }
    for mutate in MUTATION_TOOLS {
        assert!(
            !names.iter().any(|n| n == mutate),
            "read-only tools/list must NOT include mutation tool {mutate}, got: {names:?}"
        );
    }
    assert_eq!(
        names.len(),
        READ_TOOLS.len(),
        "read-only tools/list must advertise exactly {} tools, got: {names:?}",
        READ_TOOLS.len()
    );
}

/// `--read-only`: calling a mutation tool ERRORS and writes nothing to disk.
#[test]
fn read_only_refuses_mutation_call_and_writes_nothing() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let before = std::fs::read_to_string(vault.path().join("task.md")).unwrap();

    // A confirm:true set would write if it were allowed — the refusal must beat it.
    let call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "vault.set",
            "arguments": {
                "target": "task",
                "set": { "status": "active" },
                "confirm": true
            }
        }
    });

    let responses = drive(&vault, /*read_only=*/ true, &[initialize(), call]);

    let resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no vault.set response in {responses:?}"));

    // Either a JSON-RPC error, or a CallToolResult flagged isError — both count as
    // "refused." rmcp surfaces a tool handler's ErrorData as a JSON-RPC error.
    let refused = resp.get("error").is_some() || resp["result"]["isError"].as_bool() == Some(true);
    assert!(
        refused,
        "read-only vault.set call must be refused (error), got: {resp}"
    );

    let after = std::fs::read_to_string(vault.path().join("task.md")).unwrap();
    assert_eq!(
        before, after,
        "read-only mutation refusal must leave the file byte-for-byte unchanged"
    );
}

/// Default (no flag): `tools/list` advertises ALL 13 tools — path unchanged.
#[test]
fn default_lists_all_thirteen_tools() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

    let responses = drive(
        &vault,
        /*read_only=*/ false,
        &[initialize(), tools_list(2)],
    );
    let names = tool_names(&responses, 2);

    for tool in READ_TOOLS.iter().chain(MUTATION_TOOLS.iter()) {
        assert!(
            names.iter().any(|n| n == tool),
            "default tools/list must include {tool}, got: {names:?}"
        );
    }
    assert_eq!(
        names.len(),
        READ_TOOLS.len() + MUTATION_TOOLS.len(),
        "default tools/list must advertise all 13 tools, got: {names:?}"
    );
}
