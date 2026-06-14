//! Integration round-trip for the `vault.rewrite_wikilink` MCP tool
//! (NRN-33, Task 11c).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (`old-target.md`, `new-target.md`, plus docs linking `[[old-target]]`
//! in body and frontmatter), exercising the mutation-safety contract for a
//! graph-wide cascading rewrite end-to-end over JSON-RPC:
//!
//! 1. `tools/call` `vault.rewrite_wikilink` with `confirm: false` (the default)
//!    → the response reports `dry_run = true` AND no `[[old-target]]` occurrence
//!    changes on disk.
//! 2. `tools/call` `vault.rewrite_wikilink` with `confirm: true` → the response
//!    reports `dry_run = false` AND every `[[old-target]]` occurrence (body +
//!    frontmatter) is retargeted to `[[new-target]]`.
//!
//! Plus the audit invariant (confirm writes the event stream; dry-run writes
//! none). Each call runs in its OWN server process for deterministic ordering
//! (JSON-RPC pipelining under the `call_lock` is not FIFO). State is isolated via
//! an `XDG_STATE_HOME` subdir of the vault tempdir.

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

/// Seed link targets plus two docs referencing `[[old-target]]` (a.md also in
/// frontmatter via `rel`).
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-rewrite-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("old-target.md"),
        "---\ntype: note\n---\nold\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("new-target.md"),
        "---\ntype: note\n---\nnew\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("a.md"),
        "---\nrel: \"[[old-target]]\"\n---\nBody [[old-target]] and [[old-target|disp]].\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b.md"),
        "---\ntype: note\n---\nAlso [[old-target]] here.\n",
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

/// Drive `norn mcp` for ONE rewrite call in its own process; return the parsed
/// responses. `state_dir` sets `XDG_STATE_HOME`. `with_tools_list` adds id=2.
fn run_rewrite_responses(
    vault: &TempDir,
    state_dir: &Path,
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
                    "clientInfo": { "name": "norn-rewrite-client", "version": "0.0.1" }
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
        let mut args = serde_json::json!({ "from": "old-target", "to": "new-target" });
        if confirm {
            args["confirm"] = serde_json::Value::Bool(true);
        }
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "vault.rewrite_wikilink", "arguments": args }
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

fn occurrences(vault: &TempDir, file: &str, needle: &str) -> usize {
    std::fs::read_to_string(vault.path().join(file))
        .unwrap()
        .matches(needle)
        .count()
}

#[test]
fn dry_run_then_confirm_roundtrip() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    // ── Process 1: DRY-RUN (default) + tools/list schema check ────────────────
    let dry_responses = run_rewrite_responses(&vault, &state, false, true);

    let tools = dry_responses
        .iter()
        .find(|r| r["id"] == 2)
        .and_then(|r| r["result"]["tools"].as_array())
        .expect("tools/list must return a tools array");
    let tool = tools
        .iter()
        .find(|t| t["name"] == "vault.rewrite_wikilink")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.rewrite_wikilink, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        tool["inputSchema"]["properties"].get("confirm").is_some(),
        "vault.rewrite_wikilink inputSchema must expose a `confirm` property"
    );

    let dry = dry_responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no dry-run response");
    assert!(
        dry.get("error").is_none(),
        "dry-run vault.rewrite_wikilink must not error, got: {dry}"
    );
    assert_eq!(
        dry["result"]["structuredContent"]["report"]["dry_run"].as_bool(),
        Some(true),
        "dry-run report must have dry_run == true, got: {dry}"
    );

    // CRITICAL: after the dry-run process exited, no link changed on disk.
    assert_eq!(
        occurrences(&vault, "a.md", "[[old-target"),
        3,
        "dry-run must leave a.md's 2 body + 1 frontmatter old-target links intact"
    );
    assert_eq!(
        occurrences(&vault, "b.md", "[[old-target]]"),
        1,
        "dry-run must leave b.md's old-target link intact"
    );
    assert_eq!(
        occurrences(&vault, "a.md", "new-target"),
        0,
        "dry-run must introduce no new-target occurrence"
    );

    // ── Process 2: CONFIRM — applies the rewrite across body + frontmatter ────
    let confirm_responses = run_rewrite_responses(&vault, &state, true, false);
    let confirm = confirm_responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no confirm response");
    assert!(
        confirm.get("error").is_none(),
        "confirm vault.rewrite_wikilink must not error, got: {confirm}"
    );
    assert_eq!(
        confirm["result"]["structuredContent"]["report"]["dry_run"].as_bool(),
        Some(false),
        "confirm report must have dry_run == false, got: {confirm}"
    );

    // ── Final disk state: every old-target retargeted to new-target. ──────────
    let a = std::fs::read_to_string(vault.path().join("a.md")).unwrap();
    assert!(
        !a.contains("old-target")
            && a.contains("[[new-target]]")
            && a.contains("[[new-target|disp]]")
            && a.contains("rel: \"[[new-target]]\""),
        "confirm must retarget a.md's body AND frontmatter wikilinks:\n{a}"
    );
    let b = std::fs::read_to_string(vault.path().join("b.md")).unwrap();
    assert!(
        b.contains("[[new-target]]") && !b.contains("[[old-target]]"),
        "confirm must retarget b.md's body link:\n{b}"
    );
}

// ── Audit trail: MCP rewrite writes the event stream like the CLI (NRN-33) ─────

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

#[test]
fn confirm_writes_audit_event_stream() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_rewrite_responses(&vault, &state, /*confirm=*/ true, false);

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
        "audited rewrite must record invocation.started; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["EventName"]
            .as_str()
            .is_some_and(|n| n.starts_with("norn.action."))),
        "audited rewrite must record an op action event; events: {events:?}"
    );
}

#[test]
fn dry_run_writes_no_audit_events() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_rewrite_responses(&vault, &state, /*confirm=*/ false, false);

    assert!(
        find_event_files(&state).is_empty(),
        "dry-run (confirm:false) must persist no events-*.jsonl file"
    );
    assert_eq!(
        occurrences(&vault, "a.md", "[[old-target"),
        3,
        "dry-run must leave every old-target occurrence intact"
    );
}
