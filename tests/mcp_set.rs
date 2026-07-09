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

// ── Audit trail: MCP mutations write the event stream like the CLI (NRN-33) ────
//
// The defect these tests pin: a `vault.set confirm:true` MUST write the same
// append-only event stream `norn set --yes` writes — that audit trail is how an
// off-filesystem MCP client is "audited for free." Before the fix the confirm
// path used `EventSink::discard`, so NO event file was written. These tests fail
// with the old discard sink and pass once the confirm path opens a real sink.
//
// State isolation: the server is driven as a child process with an isolated
// `XDG_STATE_HOME` (a subdir of the vault tempdir), so the event stream lands
// under that tempdir and never touches the developer's real `~/.local/state`,
// and there is no in-process `set_var` race with sibling tests.

/// Recursively collect every `events-*.jsonl` file under `dir`.
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

/// Parse every JSON line of every event file under `state_root`.
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

/// Spawn `norn mcp` with the given state dir, send `initialize` + one
/// `vault.set` call (the caller supplies `confirm`), then close stdin and wait.
fn run_set_call(vault: &TempDir, state_dir: &Path, confirm: bool) {
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
                    "clientInfo": { "name": "norn-set-audit", "version": "0.0.1" }
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
                        "confirm": confirm
                    }
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

/// The audit regression: a `confirm:true` MCP mutation writes the event stream.
/// At least one `events-*.jsonl` file exists and carries `invocation.started`
/// plus an `action` event — byte-for-byte the audit trail `norn set --yes`
/// leaves. (Fails with the pre-fix `EventSink::discard` confirm path.)
#[test]
fn confirm_writes_audit_event_stream() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_set_call(&vault, &state, /*confirm=*/ true);

    let files = find_event_files(&state);
    assert!(
        !files.is_empty(),
        "confirm:true must write at least one events-*.jsonl file under {}",
        state.display()
    );

    let events = read_all_event_lines(&state);
    assert!(
        events
            .iter()
            .any(|e| e["EventName"] == "norn.invocation.started"),
        "audited mutation must record invocation.started; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["EventName"]
            .as_str()
            .is_some_and(|n| n.starts_with("norn.action."))),
        "audited mutation must record an op action event; events: {events:?}"
    );

    // Every event shares one trace id (the same id the report carries).
    let trace = events[0]["TraceId"].as_str().unwrap();
    assert!(
        events.iter().all(|e| e["TraceId"] == trace),
        "all audit events must share one trace id"
    );
    assert_eq!(trace.len(), 32, "trace id must be a 32-hex trace id");
}

/// The dry-run audit invariant: a `confirm:false` MCP call (the default) must
/// persist NO event file — dry-runs stay silent, exactly like the CLI.
#[test]
fn dry_run_writes_no_audit_events() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_set_call(&vault, &state, /*confirm=*/ false);

    assert!(
        find_event_files(&state).is_empty(),
        "dry-run (confirm:false) must persist no events-*.jsonl file"
    );
}

// ── NRN-221: `set`'s schema-refusal prose is now a coded structured refusal ────

/// Seed a vault whose `.norn/config.yaml` declares `status` as a
/// `required_frontmatter` field for `type: task` documents — the minimal schema
/// needed to trigger `SetError::RequiredFieldRemoved` (code
/// `required-field-removed`) from a `vault.set` call.
fn seeded_vault_with_required_status() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-set-refusal-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("task.md"),
        "---\ntype: task\nstatus: backlog\n---\nTask body\n",
    )
    .unwrap();
    let config_dir = tmp.path().join(".norn");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yaml"),
        r#"
validate:
  rules:
    - name: task-rule
      match:
        frontmatter:
          type: task
      required_frontmatter: [status]
"#,
    )
    .unwrap();
    tmp
}

/// NRN-221: a `vault.set` call that hits `set`'s schema-validation refusal
/// (removing a `required_frontmatter` field without `force`) now returns the
/// SAME structured coded envelope the `stale-document-hash` / CAS refusals do
/// (NRN-220) — `isError:true` + `report.error.code` — instead of a bare MCP
/// `Err` with the code laundered to prose. Mirrors
/// `tests/mcp_edit.rs::vault_edit_expected_hash_cas`'s refusal assertions.
#[test]
fn confirm_schema_refusal_returns_coded_structured_error() {
    let vault = seeded_vault_with_required_status();
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
                    "clientInfo": { "name": "norn-set-refusal-client", "version": "0.0.1" }
                }
            })))
            .unwrap();
        // Removing a required field without `force` is a schema refusal
        // (`SetError::RequiredFieldRemoved`).
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.set",
                    "arguments": {
                        "target": "task",
                        "remove": ["status"],
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
    let resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/call response\nstdout: {stdout}\nstderr: {stderr}"));

    assert_eq!(
        resp["result"]["isError"],
        serde_json::json!(true),
        "a required-field removal must map to isError:true; got: {resp}"
    );
    let report = &resp["result"]["structuredContent"]["report"];
    assert_eq!(
        report["outcome"], "refused",
        "schema refusal report outcome must be refused; got: {resp}"
    );
    assert_eq!(
        report["error"]["code"], "required-field-removed",
        "a consumer branches on the stable code, not the prose; got: {resp}"
    );

    assert_eq!(
        disk_status(&vault),
        "backlog",
        "a refused vault.set call must leave the file on disk UNCHANGED"
    );
}
