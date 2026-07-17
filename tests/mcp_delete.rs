//! Integration round-trip for the `vault.delete` MCP tool (NRN-33, Task 11b).
//!
//! `vault.delete` is DESTRUCTIVE, so the DRY-RUN-by-default property is the
//! headline safety guarantee this test pins end-to-end over JSON-RPC against a
//! real `norn mcp` child process:
//!
//! 1. `tools/call` `vault.delete` with `confirm: false` (the default) → the
//!    response reports `dry_run = true` AND `doc.md` STILL EXISTS on disk.
//! 2. `tools/call` `vault.delete` with `confirm: true` + `rewrite_to` → the
//!    response reports `dry_run = false`, `doc.md` is GONE, and the incoming link
//!    in `linker.md` is redirected to the alternate target.
//!
//! Plus the audit invariant (confirm writes the event stream; dry-run writes
//! none). Each `vault.delete` call runs in its OWN server process: JSON-RPC does
//! NOT guarantee pipelined `tools/call`s dispatch in id order (the server's
//! `call_lock` is mutual-exclusion, not FIFO), so a destructive non-idempotent
//! delete cannot share a connection with a follow-up call. State is isolated via
//! an `XDG_STATE_HOME` subdir of the vault tempdir (no in-process `set_var`).

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

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

/// Seed `doc.md`, `alt.md` (redirect target), and `linker.md` (links `[[doc]]`).
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-delete-rt-")
        .tempdir()
        .unwrap();
    std::fs::write(
        tmp.path().join("doc.md"),
        "---\ntype: note\n---\nDoc body\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("alt.md"),
        "---\ntype: note\n---\nAlt body\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("linker.md"),
        "---\ntype: note\n---\nLinks to [[doc]] here.\n",
    )
    .unwrap();
    tmp
}

fn prebuild_cache(vault: &TempDir) {
    prewrite_prune_marker(&vault.path().join(".xdg-cache"));
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

/// Drive `norn mcp` for ONE delete call in its own process and return the parsed
/// JSON-RPC responses. `state_dir` sets `XDG_STATE_HOME` so the audit stream is
/// isolated. `with_tools_list` adds the id=2 `tools/list` probe.
fn run_delete_responses(
    vault: &TempDir,
    state_dir: &Path,
    args: serde_json::Value,
    with_tools_list: bool,
) -> Vec<serde_json::Value> {
    prewrite_prune_marker(&vault.path().join(".xdg-cache"));
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
                    "clientInfo": { "name": "norn-delete-client", "version": "0.0.1" }
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
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": { "name": "vault.delete", "arguments": args }
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

#[test]
fn dry_run_then_confirm_roundtrip() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    // ── Process 1: DRY-RUN (default) + tools/list schema check ────────────────
    let dry_responses = run_delete_responses(
        &vault,
        &state,
        serde_json::json!({ "target": "doc.md", "allow_broken_links": true }),
        true,
    );

    let tools = dry_responses
        .iter()
        .find(|r| r["id"] == 2)
        .and_then(|r| r["result"]["tools"].as_array())
        .expect("tools/list must return a tools array");
    let delete_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.delete")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.delete, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });
    assert!(
        delete_tool["inputSchema"]["properties"]
            .get("confirm")
            .is_some(),
        "vault.delete inputSchema must expose a `confirm` property"
    );

    let dry = dry_responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no dry-run response");
    assert!(
        dry.get("error").is_none(),
        "dry-run vault.delete must not error, got: {dry}"
    );
    assert_eq!(
        dry["result"]["structuredContent"]["report"]["dry_run"].as_bool(),
        Some(true),
        "dry-run report must have dry_run == true, got: {dry}"
    );

    // CRITICAL (destructive safety): after the dry-run process exited, doc.md
    // STILL EXISTS and the linker is untouched.
    assert!(
        vault.path().join("doc.md").exists(),
        "dry-run must NOT delete doc.md"
    );
    assert!(
        std::fs::read_to_string(vault.path().join("linker.md"))
            .unwrap()
            .contains("[[doc]]"),
        "dry-run must leave the incoming link unchanged"
    );

    // ── Process 2: CONFIRM + rewrite_to — deletes and redirects ───────────────
    let confirm_responses = run_delete_responses(
        &vault,
        &state,
        serde_json::json!({ "target": "doc.md", "rewrite_to": "alt.md", "confirm": true }),
        false,
    );
    let confirm = confirm_responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no confirm response");
    assert!(
        confirm.get("error").is_none(),
        "confirm vault.delete must not error, got: {confirm}"
    );
    assert_eq!(
        confirm["result"]["structuredContent"]["report"]["dry_run"].as_bool(),
        Some(false),
        "confirm report must have dry_run == false, got: {confirm}"
    );

    // ── Final disk state: doc.md gone, incoming link redirected to [[alt]]. ────
    assert!(
        !vault.path().join("doc.md").exists(),
        "after confirm, doc.md must be deleted from disk"
    );
    let linker = std::fs::read_to_string(vault.path().join("linker.md")).unwrap();
    assert!(
        linker.contains("[[alt]]") && !linker.contains("[[doc]]"),
        "after confirm + rewrite_to, the incoming link must be redirected to [[alt]]:\n{linker}"
    );
}

// ── Bare-stem resolution through the tool (NRN-239) ─────────────────────────────

/// NRN-239: `target` given as a bare stem resolves through the tool's own
/// preflight exactly like the CLI — the confirm deletes the RESOLVED `doc.md`,
/// and the report's `summary` prose names the resolved document rather than the
/// raw stem.
#[test]
fn bare_stem_resolves_through_tool() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    let responses = run_delete_responses(
        &vault,
        &state,
        serde_json::json!({ "target": "doc", "allow_broken_links": true, "confirm": true }),
        false,
    );
    let resp = responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no response");
    assert!(
        resp.get("error").is_none(),
        "bare-stem confirm vault.delete must not error, got: {resp}"
    );
    let report = &resp["result"]["structuredContent"]["report"];
    assert_eq!(report["dry_run"].as_bool(), Some(false));
    assert_eq!(report["applied"].as_u64(), Some(1));
    let op = &report["operations"][0];
    assert_eq!(
        op["summary"].as_str().unwrap_or(""),
        "delete doc.md",
        "the RESOLVED target, not the raw stem, must appear in the plan/report: {op}"
    );

    assert!(
        !vault.path().join("doc.md").exists(),
        "bare-stem delete must delete the RESOLVED doc.md"
    );
}

/// NRN-239: an ambiguous bare stem (two docs sharing the stem) is refused with
/// the coded `target-ambiguous` through the tool — a structured `isError:true`
/// refusal, not a silent delete of either candidate.
#[test]
fn ambiguous_stem_refuses_through_tool() {
    let vault = seeded_vault();
    std::fs::create_dir_all(vault.path().join("sub")).unwrap();
    std::fs::write(
        vault.path().join("sub/doc.md"),
        "---\ntype: note\n---\nAnother doc\n",
    )
    .unwrap();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    let responses = run_delete_responses(
        &vault,
        &state,
        serde_json::json!({ "target": "doc", "allow_broken_links": true, "confirm": true }),
        false,
    );
    let resp = responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("no response");
    assert!(
        resp.get("error").is_none(),
        "an ambiguous-stem refusal must be Ok(structured), not a bare JSON-RPC Err: {resp}"
    );
    assert_eq!(
        resp["result"]["isError"],
        serde_json::json!(true),
        "a confirmed ambiguous-stem refusal maps to isError:true; got: {resp}"
    );
    let report = &resp["result"]["structuredContent"]["report"];
    assert_eq!(report["outcome"], "refused");
    assert_eq!(report["operations"][0]["error"]["code"], "target-ambiguous");

    // Nothing deleted: both candidates are untouched.
    assert!(vault.path().join("doc.md").exists());
    assert!(vault.path().join("sub/doc.md").exists());
}

// ── Audit trail: MCP delete writes the event stream like the CLI (NRN-33) ──────

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

    run_delete_responses(
        &vault,
        &state,
        serde_json::json!({ "target": "doc.md", "allow_broken_links": true, "confirm": true }),
        false,
    );

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
        "audited delete must record invocation.started; events: {events:?}"
    );
    assert!(
        events.iter().any(|e| e["EventName"]
            .as_str()
            .is_some_and(|n| n.starts_with("norn.action."))),
        "audited delete must record an op action event; events: {events:?}"
    );
}

#[test]
fn dry_run_writes_no_audit_events() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let state = vault.path().join(".xdg-state");

    run_delete_responses(
        &vault,
        &state,
        serde_json::json!({ "target": "doc.md", "allow_broken_links": true, "confirm": false }),
        false,
    );

    assert!(
        find_event_files(&state).is_empty(),
        "dry-run (confirm:false) must persist no events-*.jsonl file"
    );
    // And the destructive op was a true no-op: doc.md survives.
    assert!(
        vault.path().join("doc.md").exists(),
        "dry-run must NOT delete doc.md"
    );
}
