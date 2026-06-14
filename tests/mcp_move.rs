//! Integration round-trip for the `vault.move` MCP tool (NRN-33, Task 11a).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (`a.md`, plus `b.md` linking `[[a]]`), exercising the **mutation-safety
//! contract** for a CASCADING mutation end-to-end over JSON-RPC:
//!
//! 1. `tools/call` `vault.move` with `confirm: false` (the default) → the
//!    response reports `dry_run = true` AND on disk `a.md` is STILL at its path
//!    and `b.md`'s `[[a]]` backlink is unchanged.
//! 2. `tools/call` `vault.move` with `confirm: true` → the response reports
//!    `dry_run = false` AND `a.md` moved to the new path with `b.md`'s backlink
//!    rewritten to the new target.
//!
//! Plus the audit invariant: a `confirm:true` move writes the event stream
//! (`invocation.started` + an `action` event), a `confirm:false` move writes
//! none — exactly like the CLI. State is isolated via an `XDG_STATE_HOME` subdir
//! of the vault tempdir per the `mcp_set.rs` pattern (no in-process `set_var`).

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn norn_bin() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push(format!("norn{}", std::env::consts::EXE_SUFFIX));
    p
}

/// Seed `a.md` and `b.md` (b links to `[[a]]`).
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-move-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(tmp.path().join("a.md"), "---\ntype: note\n---\nA body\n").unwrap();
    std::fs::write(
        tmp.path().join("b.md"),
        "---\ntype: note\n---\nLinks to [[a]] here.\n",
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

/// Drive `norn mcp` for ONE move call and return the parsed JSON-RPC responses.
///
/// Each call gets its own server process: `initialize` (id=1) + optional
/// `tools/list` (id=2) + the `vault.move` call (id=3). Using a fresh process per
/// call makes request ordering deterministic — JSON-RPC does NOT guarantee that
/// two pipelined `tools/call`s on one connection are dispatched in id order (the
/// server's `call_lock` enforces mutual exclusion, not FIFO), so a non-idempotent
/// mutation like move cannot share a connection with a follow-up call in a test.
fn run_move_responses(
    vault: &TempDir,
    from: &str,
    to: &str,
    confirm: bool,
    with_tools_list: bool,
) -> Vec<serde_json::Value> {
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
                    "clientInfo": { "name": "norn-move-client", "version": "0.0.1" }
                }
            })))
            .unwrap();
        if with_tools_list {
            stdin
                .write_all(&line(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                    "params": {}
                })))
                .unwrap();
        }
        let mut args = serde_json::json!({ "from": from, "to": to });
        if confirm {
            args["confirm"] = serde_json::Value::Bool(true);
        }
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "vault.move", "arguments": args }
            })))
            .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait on norn mcp");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "norn mcp exited non-zero ({})\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Full contract round-trip: a `confirm:false` move (its own process) reports
/// `dry_run=true` and leaves disk untouched; a subsequent `confirm:true` move
/// (a second process) reports `dry_run=false`, moves `a.md`, and rewrites
/// `b.md`'s backlink. Sequential processes guarantee the dry-run is observed
/// before the confirm mutates anything.
#[test]
fn dry_run_then_confirm_roundtrip() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

    // ── Process 1: DRY-RUN (default) + tools/list schema check ────────────────
    let dry_responses = run_move_responses(&vault, "a.md", "renamed.md", false, true);

    let tools = dry_responses
        .iter()
        .find(|r| r["id"] == 2)
        .and_then(|r| r["result"]["tools"].as_array())
        .expect("tools/list must return a tools array");
    let move_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.move")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.move, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        move_tool["inputSchema"]["properties"]
            .get("confirm")
            .is_some(),
        "vault.move inputSchema must expose a `confirm` property"
    );

    let dry = dry_responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no dry-run response");
    assert!(
        dry.get("error").is_none(),
        "dry-run vault.move must not error, got: {dry}"
    );
    let dry_report = &dry["result"]["structuredContent"]["report"];
    assert_eq!(
        dry_report["dry_run"].as_bool(),
        Some(true),
        "dry-run report must have dry_run == true, got: {dry}"
    );
    assert_eq!(
        dry_report["applied"].as_u64(),
        Some(0),
        "dry-run report must have applied == 0, got: {dry}"
    );

    // CRITICAL: after the dry-run process exited, disk is untouched.
    assert!(
        vault.path().join("a.md").exists(),
        "dry-run must NOT move a.md off its path"
    );
    assert!(
        !vault.path().join("renamed.md").exists(),
        "dry-run must NOT create the destination"
    );
    assert!(
        std::fs::read_to_string(vault.path().join("b.md"))
            .unwrap()
            .contains("[[a]]"),
        "dry-run must leave b.md's backlink unchanged"
    );

    // ── Process 2: CONFIRM — applies the move + cascade ───────────────────────
    let confirm_responses = run_move_responses(&vault, "a.md", "renamed.md", true, false);
    let confirm = confirm_responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no confirm response");
    assert!(
        confirm.get("error").is_none(),
        "confirm vault.move must not error, got: {confirm}"
    );
    let confirm_report = &confirm["result"]["structuredContent"]["report"];
    assert_eq!(
        confirm_report["dry_run"].as_bool(),
        Some(false),
        "confirm report must have dry_run == false, got: {confirm}"
    );

    // ── Final disk state: a.md moved, backlink rewritten. ─────────────────────
    assert!(
        !vault.path().join("a.md").exists(),
        "after confirm, a.md must be gone from its original path"
    );
    assert!(
        vault.path().join("renamed.md").exists(),
        "after confirm, renamed.md must exist"
    );
    let b_after = std::fs::read_to_string(vault.path().join("b.md")).unwrap();
    assert!(
        b_after.contains("[[renamed]]") && !b_after.contains("[[a]]"),
        "after confirm, b.md's backlink must be rewritten to [[renamed]]:\n{b_after}"
    );
}

/// Dedicated dry-run safety check: a `confirm:false` move against a fresh vault
/// moves nothing and leaves every linker byte-identical. Isolates the "writes
/// nothing" property into its own process so no later confirm write can mask a
/// regression that moved on dry-run.
#[test]
fn dry_run_alone_moves_nothing() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let a_before = std::fs::read_to_string(vault.path().join("a.md")).unwrap();
    let b_before = std::fs::read_to_string(vault.path().join("b.md")).unwrap();

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
                    "clientInfo": { "name": "norn-move-client", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.move",
                    "arguments": { "from": "a.md", "to": "renamed.md", "confirm": false }
                }
            })))
            .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait on norn mcp");
    assert!(output.status.success(), "norn mcp exited non-zero");

    assert!(
        vault.path().join("a.md").exists() && !vault.path().join("renamed.md").exists(),
        "dry-run must NOT move a.md"
    );
    assert_eq!(
        a_before,
        std::fs::read_to_string(vault.path().join("a.md")).unwrap(),
        "dry-run must leave a.md byte-identical"
    );
    assert_eq!(
        b_before,
        std::fs::read_to_string(vault.path().join("b.md")).unwrap(),
        "dry-run must leave b.md (the backlink) byte-identical"
    );
}

// ── Audit trail: MCP move writes the event stream like the CLI (NRN-33) ────────

fn find_event_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(find_event_files(&path));
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                found.push(path);
            }
        }
    }
    found
}

fn read_all_event_lines(state_root: &Path) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    for f in find_event_files(state_root) {
        let body = std::fs::read_to_string(&f).unwrap();
        for l in body.lines() {
            if l.trim().is_empty() {
                continue;
            }
            events.push(serde_json::from_str(l).expect("each event line must parse as JSON"));
        }
    }
    events
}

/// Spawn `norn mcp`, send `initialize` + one `vault.move` call (caller supplies
/// `confirm`), close stdin, wait.
fn run_move_call(vault: &TempDir, state_dir: &Path, confirm: bool) {
    let mut child = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", state_dir)
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
                    "clientInfo": { "name": "norn-move-audit", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.move",
                    "arguments": { "from": "a.md", "to": "renamed.md", "confirm": confirm }
                }
            })))
            .unwrap();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait on norn mcp");
    assert!(
        output.status.success(),
        "norn mcp exited non-zero\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn confirm_writes_audit_event_stream() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_move_call(&vault, &state, /*confirm=*/ true);

    assert!(
        !find_event_files(&state).is_empty(),
        "confirm:true must write at least one events-*.jsonl file under {}",
        state.display()
    );

    let events = read_all_event_lines(&state);
    assert!(
        events
            .iter()
            .any(|e| e["EventName"] == "norn.invocation.started"),
        "audited move must record invocation.started; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["EventName"]
            .as_str()
            .is_some_and(|n| n.starts_with("norn.action."))),
        "audited move must record an op action event; events: {events:?}"
    );
}

#[test]
fn dry_run_writes_no_audit_events() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_move_call(&vault, &state, /*confirm=*/ false);

    assert!(
        find_event_files(&state).is_empty(),
        "dry-run (confirm:false) must persist no events-*.jsonl file"
    );
}
