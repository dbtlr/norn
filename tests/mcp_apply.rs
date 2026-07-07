//! Integration round-trip for the `vault.apply` MCP tool (NRN-33, Task 12).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault with a FIXABLE broken wikilink. Exercises the full repair →
//! apply composition over the wire, plus mutation-safety and audit invariants.
//!
//! Tests:
//! 1. `tools/list` advertises `vault.apply` with a non-empty `inputSchema`
//!    and a `confirm` property.
//! 2. **Dry-run:** `vault.apply` with `confirm: false` (the default) returns
//!    `dry_run: true` AND the broken link is STILL broken on disk (disk unchanged).
//! 3. **Confirm:** `vault.apply` with `confirm: true` applies the fix on disk
//!    (the broken link is rewritten) AND the report has `dry_run: false` +
//!    `applied >= 1`.
//! 4. **Compose:** call `vault.repair` then feed its `plan` value directly
//!    into `vault.apply confirm:true` — proves end-to-end composition over
//!    the wire. Uses two separate processes (non-idempotent mutation cannot share a
//!    connection with a follow-up call in a test).
//! 5. **Malformed plan:** a plan with wrong/missing `schema_version` returns an
//!    MCP error and applies nothing.
//! 6. **Audit:** a `confirm:true` apply writes the event stream
//!    (`invocation.started` + an `action` event); a `confirm:false` call writes
//!    none.
//!
//! Non-flaky by construction: write all requests then close stdin so the server
//! shuts down cleanly — no sleeps, no timeouts. Cache is pre-built before the
//! MCP child starts to avoid concurrent cold-start races (NRN-55).

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

/// Vault with a FIXABLE broken wikilink:
/// - `target-note.md` exists (stem: `target-note`)
/// - `source.md` links to `[[target-not]]` (one-char edit → closest-match proposal)
///
/// Cache is pre-built so the MCP server's first tool call doesn't race a
/// concurrent cold-start rebuild (NRN-55).
fn vault_with_fixable_link() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-apply-plan-rt-")
        .tempdir()
        .unwrap();

    std::fs::write(
        tmp.path().join("target-note.md"),
        "---\ntype: note\ntitle: Target Note\n---\n\nI am the target.\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("source.md"),
        "---\ntype: note\ntitle: Source\n---\n\nSee [[target-not]] for details.\n",
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

/// Spawn `norn mcp` and run a sequence of JSON-RPC messages, returning the
/// parsed response lines.  Panics if the server exits non-zero.
fn run_mcp_sequence(vault: &TempDir, messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
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
        for msg in messages {
            stdin.write_all(&line(msg)).unwrap();
        }
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

/// Same as `run_mcp_sequence` but with an explicit `XDG_STATE_HOME` override
/// (needed for audit trail tests where we want to inspect the state dir from
/// outside the vault tempdir).
fn run_mcp_sequence_with_state(
    vault: &TempDir,
    state_dir: &Path,
    messages: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
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
        for msg in messages {
            stdin.write_all(&line(msg)).unwrap();
        }
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

fn initialize_msg(id: u64, client_name: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": "0.0.1" }
        }
    })
}

/// Build a plan value by calling `vault.repair` in one MCP session.
/// Returns the plan `serde_json::Value` from `result.structuredContent.plan`.
fn get_repair_output(vault: &TempDir) -> serde_json::Value {
    let responses = run_mcp_sequence(
        vault,
        vec![
            initialize_msg(1, "norn-apply-plan-setup"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "vault.repair", "arguments": {} }
            }),
        ],
    );
    let resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("no vault.repair response");
    assert!(
        resp.get("error").is_none(),
        "vault.repair must not error: {resp}"
    );
    resp["result"]["structuredContent"]["plan"].clone()
}

// ── Test 1: tools/list + schema check ───────────────────────────────────────────

#[test]
fn lists_vault_apply_with_confirm_property() {
    let vault = vault_with_fixable_link();

    let responses = run_mcp_sequence(
        &vault,
        vec![
            initialize_msg(1, "norn-apply-plan-list"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            }),
        ],
    );

    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("no tools/list response");
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools/list result.tools must be an array");

    let tool = tools
        .iter()
        .find(|t| t["name"] == "vault.apply")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.apply, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    let schema = &tool["inputSchema"];
    assert!(
        schema.is_object(),
        "vault.apply inputSchema must be an object, got: {schema}"
    );
    assert!(
        schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "vault.apply inputSchema must have non-empty properties, got: {schema}"
    );
    assert!(
        schema["properties"].get("confirm").is_some(),
        "vault.apply inputSchema must expose a `confirm` property, got: {schema}"
    );
    assert!(
        schema["properties"].get("plan").is_some(),
        "vault.apply inputSchema must expose a `plan` property, got: {schema}"
    );
}

// ── Test 2 & 3: dry-run then confirm round-trip ─────────────────────────────────

/// A `confirm:false` apply (dry-run) reports `dry_run=true` and leaves disk
/// unchanged.  A subsequent `confirm:true` apply (in a separate process) fixes
/// the broken link on disk and reports `applied >= 1`.
#[test]
fn dry_run_then_confirm_roundtrip() {
    let vault = vault_with_fixable_link();

    // ── Step 1: get the repair plan ──────────────────────────────────────────────
    let plan = get_repair_output(&vault);
    assert!(
        plan["operations"]
            .as_array()
            .is_some_and(|ops| !ops.is_empty()),
        "vault.repair must return ≥1 operation for the fixable vault: {plan}"
    );

    // ── Step 2: dry-run apply (confirm:false, the default) ───────────────────────
    let source_before = std::fs::read_to_string(vault.path().join("source.md")).unwrap();

    let dry_responses = run_mcp_sequence(
        &vault,
        vec![
            initialize_msg(1, "norn-apply-plan-dryrun"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.apply",
                    "arguments": { "plan": plan.clone() }
                    // confirm absent → false → dry-run
                }
            }),
        ],
    );

    let dry_resp = dry_responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("no dry-run response");
    assert!(
        dry_resp.get("error").is_none(),
        "dry-run vault.apply must not error, got: {dry_resp}"
    );
    let dry_report = &dry_resp["result"]["structuredContent"]["report"];
    assert_eq!(
        dry_report["dry_run"].as_bool(),
        Some(true),
        "dry-run report must have dry_run == true, got: {dry_resp}"
    );
    assert_eq!(
        dry_report["applied"].as_u64(),
        Some(0),
        "dry-run report must have applied == 0, got: {dry_resp}"
    );

    // CRITICAL: after the dry-run process exited, disk is untouched.
    let source_after_dry = std::fs::read_to_string(vault.path().join("source.md")).unwrap();
    assert_eq!(
        source_before, source_after_dry,
        "dry-run must leave source.md byte-identical"
    );
    assert!(
        source_after_dry.contains("[[target-not]]"),
        "dry-run must NOT rewrite the broken link:\n{source_after_dry}"
    );

    // ── Step 3: confirm apply (confirm:true) in a SEPARATE process ───────────────
    // Non-idempotent mutation — use a fresh server process so request ordering
    // is deterministic and the mutation is cleanly isolated.
    let confirm_responses = run_mcp_sequence(
        &vault,
        vec![
            initialize_msg(1, "norn-apply-plan-confirm"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.apply",
                    "arguments": { "plan": plan, "confirm": true }
                }
            }),
        ],
    );

    let confirm_resp = confirm_responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("no confirm response");
    assert!(
        confirm_resp.get("error").is_none(),
        "confirm vault.apply must not error, got: {confirm_resp}"
    );
    let confirm_report = &confirm_resp["result"]["structuredContent"]["report"];
    assert_eq!(
        confirm_report["dry_run"].as_bool(),
        Some(false),
        "confirm report must have dry_run == false, got: {confirm_resp}"
    );
    assert!(
        confirm_report["applied"].as_u64().unwrap_or(0) >= 1,
        "confirm report must have applied >= 1, got: {confirm_resp}"
    );

    // The broken link must be fixed on disk.
    let source_fixed = std::fs::read_to_string(vault.path().join("source.md")).unwrap();
    assert!(
        source_fixed.contains("[[target-note]]"),
        "confirm must rewrite [[target-not]] → [[target-note]] in source.md:\n{source_fixed}"
    );
    assert!(
        !source_fixed.contains("[[target-not]]"),
        "confirm must not leave the old broken link:\n{source_fixed}"
    );
}

// ── Test 4: malformed plan rejection ────────────────────────────────────────────

/// A plan with an invalid `schema_version` is rejected with an MCP error;
/// applies nothing.
#[test]
fn malformed_plan_returns_mcp_error() {
    let vault = vault_with_fixable_link();
    let source_before = std::fs::read_to_string(vault.path().join("source.md")).unwrap();

    let bad_plan = serde_json::json!({
        "schema_version": 99,
        "vault_root": vault.path().to_str().unwrap(),
        "operations": []
    });

    let responses = run_mcp_sequence(
        &vault,
        vec![
            initialize_msg(1, "norn-apply-plan-malformed"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.apply",
                    "arguments": { "plan": bad_plan, "confirm": false }
                }
            }),
        ],
    );

    let resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("no response for malformed plan call");
    assert!(
        resp.get("error").is_some()
            || resp["result"]["isError"].as_bool().unwrap_or(false)
            || resp["result"]["content"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|c| c["text"].as_str())
                .map(|t| t.contains("schema_version"))
                .unwrap_or(false),
        "malformed plan (wrong schema_version) must produce an error response, got: {resp}"
    );

    // Disk is still untouched.
    let source_after = std::fs::read_to_string(vault.path().join("source.md")).unwrap();
    assert_eq!(
        source_before, source_after,
        "malformed plan rejection must leave disk unchanged"
    );
}

// ── Test 5 & 6: audit trail ─────────────────────────────────────────────────────

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

/// `confirm:true` writes the event stream; `confirm:false` (dry-run) writes none.
#[test]
fn confirm_writes_audit_event_stream_dry_run_does_not() {
    let vault = vault_with_fixable_link();
    let plan = get_repair_output(&vault);

    // ── Dry-run: no events ────────────────────────────────────────────────────
    let state_dry = vault.path().join(".xdg-state-dryrun");
    run_mcp_sequence_with_state(
        &vault,
        &state_dry,
        vec![
            initialize_msg(1, "norn-apply-plan-audit-dry"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.apply",
                    "arguments": { "plan": plan.clone() }
                    // confirm absent → false → dry-run
                }
            }),
        ],
    );

    assert!(
        find_event_files(&state_dry).is_empty(),
        "dry-run (confirm:false) must persist no events-*.jsonl file under {}",
        state_dry.display()
    );

    // ── Confirm: events present ───────────────────────────────────────────────
    let state_confirm = vault.path().join(".xdg-state-confirm");
    run_mcp_sequence_with_state(
        &vault,
        &state_confirm,
        vec![
            initialize_msg(1, "norn-apply-plan-audit-confirm"),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.apply",
                    "arguments": { "plan": plan, "confirm": true }
                }
            }),
        ],
    );

    assert!(
        !find_event_files(&state_confirm).is_empty(),
        "confirm:true must write at least one events-*.jsonl file under {}",
        state_confirm.display()
    );

    let events = read_all_event_lines(&state_confirm);
    assert!(
        events
            .iter()
            .any(|e| e["EventName"] == "norn.invocation.started"),
        "audited apply must record invocation.started; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["EventName"]
            .as_str()
            .is_some_and(|n| n.starts_with("norn.action."))),
        "audited apply must record an op action event; events: {events:?}"
    );
}
