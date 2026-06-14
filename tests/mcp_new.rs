//! Integration round-trip for the `vault.new` MCP tool (NRN-33, Task 10).
//!
//! Drives the real `norn mcp` binary as a child process against a temp vault,
//! exercising the **mutation-safety contract** end-to-end over JSON-RPC:
//!
//! 1. `tools/call` `vault.new` with `confirm: false` (the default dry-run) →
//!    the response reports `applied = false` AND no file exists on disk.
//! 2. `tools/call` `vault.new` with `confirm: true` → the response reports
//!    `applied = true` AND the file now exists on disk with schema-scaffolded
//!    frontmatter.
//! 3. On `confirm: true`, an `events-*.jsonl` audit file is written under
//!    `XDG_STATE_HOME` — mirroring the event stream `norn new --yes` leaves.
//!
//! The pure handler is unit-tested inside `src/mcp/tools/new.rs`; this test
//! covers the rmcp wiring (router registration, schema, call dispatch) and the
//! disk effect of the contract through a real process.
//!
//! Non-flaky by construction: requests are written in order, then stdin is
//! closed so the server exits cleanly — no sleeps, no timeouts.

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

/// Temp vault with a schema that scaffolds `type: note` on every `.md` doc.
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-new-rt-")
        .tempdir()
        .unwrap();
    let norn_dir = tmp.path().join(".norn");
    std::fs::create_dir_all(&norn_dir).unwrap();
    std::fs::write(
        norn_dir.join("config.yaml"),
        r#"
validate:
  rules:
    - name: note-rule
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#,
    )
    .unwrap();
    tmp
}

/// Pre-build the cache so the first MCP call hits the warm path.
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

/// Core contract test: dry-run then confirm in one server session.
///
/// The dry-run and confirm target DIFFERENT paths so the dry-run path's
/// non-existence after the session proves the dry-run wrote nothing, while
/// the confirm path's existence proves the confirm wrote the file.
///
/// - dry-run path: `applied == false`, file does NOT exist after server exits
/// - confirm path: `applied == true`, file EXISTS with schema-scaffolded frontmatter
#[test]
fn dry_run_then_confirm_roundtrip() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

    // Different paths so the dry-run's non-existence is not masked by the confirm.
    let dry_path = "dry-only-path.md";
    let confirm_path = "confirm-path.md";

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
                    "clientInfo": { "name": "norn-new-client", "version": "0.0.1" }
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
                    "name": "vault.new",
                    "arguments": {
                        "path": dry_path
                    }
                }
            })))
            .unwrap();
        // Confirm: create a DIFFERENT file on disk.
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "vault.new",
                    "arguments": {
                        "path": confirm_path,
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

    // ── tools/list (id=2): vault.new present with `confirm` + `path` properties ─
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));
    let new_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.new")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.new, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });
    let schema = &new_tool["inputSchema"];
    assert!(
        schema["properties"].get("confirm").is_some(),
        "vault.new inputSchema must expose a `confirm` property, got: {schema}"
    );
    assert!(
        schema["properties"].get("path").is_some(),
        "vault.new inputSchema must expose a `path` property, got: {schema}"
    );

    // ── tools/call (id=3): DRY-RUN — applied=false ────────────────────────────
    let dry = responses
        .iter()
        .find(|r| r["id"] == 3)
        .unwrap_or_else(|| panic!("no dry-run response\nstdout: {stdout}\nstderr: {stderr}"));
    assert!(
        dry.get("error").is_none(),
        "dry-run vault.new must not error, got: {dry}\nstderr: {stderr}"
    );
    let dry_report = &dry["result"]["structuredContent"]["report"];
    assert_eq!(
        dry_report["applied"].as_bool(),
        Some(false),
        "dry-run report must have applied == false, got: {dry}"
    );
    assert_eq!(
        dry_report["operation"].as_str(),
        Some("new"),
        "dry-run report operation must be 'new', got: {dry}"
    );

    // ── tools/call (id=4): CONFIRM — applied=true ─────────────────────────────
    let confirm = responses
        .iter()
        .find(|r| r["id"] == 4)
        .unwrap_or_else(|| panic!("no confirm response\nstdout: {stdout}\nstderr: {stderr}"));
    assert!(
        confirm.get("error").is_none(),
        "confirm vault.new must not error, got: {confirm}\nstderr: {stderr}"
    );
    let confirm_report = &confirm["result"]["structuredContent"]["report"];
    assert_eq!(
        confirm_report["applied"].as_bool(),
        Some(true),
        "confirm report must have applied == true, got: {confirm}"
    );

    // ── Final disk state ───────────────────────────────────────────────────────
    // CRITICAL: dry-run path must NOT exist (only the dry-run targeted it).
    // The confirm targeted a different path, so this is an unambiguous proof
    // that the dry-run wrote nothing.
    assert!(
        !vault.path().join(dry_path).exists(),
        "dry-run path must NOT exist on disk after session: {:?}",
        vault.path().join(dry_path)
    );

    // Confirm path must now exist with schema-scaffolded frontmatter.
    let confirm_disk = vault.path().join(confirm_path);
    assert!(
        confirm_disk.exists(),
        "after confirm:true, the file must exist at {confirm_disk:?}"
    );
    let content = std::fs::read_to_string(&confirm_disk).unwrap();
    assert!(
        content.contains("type: note"),
        "created file must have schema-scaffolded `type: note` frontmatter:\n{content}"
    );
}

/// Dedicated dry-run safety check in isolation: a `confirm:false` call against a
/// fresh vault creates no file. This isolates the "writes nothing" property from
/// the confirm call so a regression that wrote on dry-run can't hide behind a
/// later confirm write.
#[test]
fn dry_run_alone_writes_nothing() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

    let new_doc_path = "dry-only.md";

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
                    "clientInfo": { "name": "norn-new-dry", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.new",
                    "arguments": {
                        "path": new_doc_path,
                        "confirm": false
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

    assert!(
        !vault.path().join(new_doc_path).exists(),
        "dry-run (confirm:false) must leave NO file on disk"
    );
}

// ── Audit trail: MCP new mutations write the event stream like the CLI ────────
//
// `vault.new` with `confirm:true` must write the same append-only event stream
// `norn new --yes` writes. These tests pin the audit regression: before the
// fix the confirm path used `EventSink::discard`, so NO event file was written.
// State isolation: isolated `XDG_STATE_HOME` in the vault tempdir — never
// touches the developer's real `~/.local/state`, no in-process `set_var` race.

/// Spawn `norn mcp` with the given state dir, send `initialize` + one
/// `vault.new` call (the caller supplies `confirm`), then close stdin and wait.
fn run_new_call(vault: &TempDir, state_dir: &Path, path: &str, confirm: bool) {
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
                    "clientInfo": { "name": "norn-new-audit", "version": "0.0.1" }
                }
            })))
            .unwrap();
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.new",
                    "arguments": {
                        "path": path,
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

/// `confirm:true` MCP new writes the audit event stream.
/// At least one `events-*.jsonl` file exists with `invocation.started` and
/// an `action` event — byte-for-byte the audit trail `norn new --yes` leaves.
#[test]
fn confirm_writes_audit_event_stream() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_new_call(&vault, &state, "audited-new.md", /*confirm=*/ true);

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
        "audited new must record invocation.started; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["EventName"]
            .as_str()
            .is_some_and(|n| n.starts_with("norn.action."))),
        "audited new must record an op action event; events: {events:?}"
    );

    // All events share one trace id.
    let trace = events[0]["TraceId"].as_str().unwrap();
    assert!(
        events.iter().all(|e| e["TraceId"] == trace),
        "all audit events must share one trace id"
    );
    assert_eq!(trace.len(), 32, "trace id must be a 32-hex string");
}

/// `confirm:false` MCP call (the default dry-run) must persist NO event file —
/// dry-runs stay silent, exactly like the CLI's non-`--yes` branch.
#[test]
fn dry_run_writes_no_audit_events() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_new_call(&vault, &state, "silent-dry.md", /*confirm=*/ false);

    assert!(
        find_event_files(&state).is_empty(),
        "dry-run (confirm:false) must persist no events-*.jsonl file"
    );
}
