//! Integration round-trip for the `vault.edit` MCP tool (NRN-19).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault, exercising the **mutation-safety contract** end-to-end over JSON-RPC:
//!
//! 1. `tools/call` `vault.edit` with `confirm` omitted (default false) → the
//!    response reports `applied = false`.
//! 2. `tools/call` `vault.edit` with `confirm: true` → the response reports
//!    `applied = true` AND the file on disk now reflects the edit.
//!
//! Mirrors `tests/mcp_set.rs`: helpers are copied verbatim (a separate test
//! binary), and the `applied` flag is extracted at the same JSON path the
//! working `vault.set` round-trip uses (`result.structuredContent.report`).

use std::io::{BufRead as _, BufReader, Write as _};
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
        .prefix("norn-mcp-edit-rt-")
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
/// a fresh index without a cold-start build inside the apply path.
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

/// blake3 hex of a file's full bytes — the canonical `document_hash` convention.
fn blake3_of_file(path: &std::path::Path) -> String {
    blake3::hash(&std::fs::read(path).unwrap())
        .to_hex()
        .to_string()
}

/// NRN-99 (H1): `vault.edit`'s optional `expected_hash` CAS mirrors
/// `norn edit --expected-hash` end-to-end over JSON-RPC. A stale hash refuses
/// (error, no write); the correct hash applies — proving the precondition is
/// reachable for an MCP-only, off-filesystem client, at CLI ↔ MCP parity.
#[test]
fn vault_edit_expected_hash_cas() {
    let vault = seeded_vault();
    prebuild_cache(&vault);
    let doc = vault.path().join("task.md");
    let good_hash = blake3_of_file(&doc);

    let initialize = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}
    });
    let stale = serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"vault.edit","arguments":{
            "target":"task.md",
            "edits":[{"op":"str_replace","old":"Task body","new":"Edited body"}],
            "expected_hash":"deadbeefstalehash",
            "confirm":true
        }}
    });
    let good = serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"vault.edit","arguments":{
            "target":"task.md",
            "edits":[{"op":"str_replace","old":"Task body","new":"Edited body"}],
            "expected_hash":good_hash,
            "confirm":true
        }}
    });

    let mut child = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("mcp")
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut read_response = |want: i64| -> serde_json::Value {
        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).expect("read response line");
            assert!(n > 0, "stream closed before response id={want}");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&buf) {
                if v["id"] == want {
                    return v;
                }
            }
        }
    };
    let applied_in = |resp: &serde_json::Value| -> Option<bool> {
        resp.get("result")
            .and_then(|r| r.get("structuredContent"))
            .and_then(|s| s.get("report"))
            .and_then(|rep| rep.get("applied"))
            .and_then(|a| a.as_bool())
    };

    stdin.write_all(&line(initialize)).unwrap();
    let _init = read_response(1);

    // ── Stale expected_hash: refuse, write nothing. ──
    stdin.write_all(&line(stale)).unwrap();
    let stale_resp = read_response(2);
    assert_ne!(
        applied_in(&stale_resp),
        Some(true),
        "stale expected_hash must not apply; got: {stale_resp}"
    );
    // NRN-220: the refusal is STRUCTURED, not a bare JSON-RPC error. The result
    // carries `isError:true` + a preserved report whose `outcome:"refused"` and
    // machine-branchable `error.code` a consumer branches on — the code is no
    // longer laundered to prose.
    assert_eq!(
        stale_resp["result"]["isError"],
        serde_json::json!(true),
        "stale expected_hash must map to isError:true; got: {stale_resp}"
    );
    let stale_report = &stale_resp["result"]["structuredContent"]["report"];
    assert_eq!(
        stale_report["outcome"], "refused",
        "stale expected_hash report outcome must be refused; got: {stale_resp}"
    );
    assert_eq!(
        stale_report["error"]["code"], "stale-document-hash",
        "a consumer branches on the stable code, not the prose; got: {stale_resp}"
    );
    // The refusal identifies the drifted document consistently: resolved path in
    // both `report.target` and `error.path`, and named in the message (CLI parity).
    assert_eq!(
        stale_report["target"], "task.md",
        "refusal target must be the resolved path; got: {stale_resp}"
    );
    assert_eq!(
        stale_report["error"]["path"], "task.md",
        "refusal error.path must name the drifted document; got: {stale_resp}"
    );
    let mid = std::fs::read_to_string(&doc).unwrap();
    assert!(
        mid.contains("Task body") && !mid.contains("Edited body"),
        "stale expected_hash must leave the body unchanged on disk: {mid}"
    );

    // ── Correct expected_hash: apply, edit lands. ──
    stdin.write_all(&line(good)).unwrap();
    let good_resp = read_response(3);
    assert_eq!(
        applied_in(&good_resp),
        Some(true),
        "correct expected_hash must apply; got: {good_resp}"
    );

    drop(stdin);
    assert!(child.wait().unwrap().success());
    let final_doc = std::fs::read_to_string(&doc).unwrap();
    assert!(
        final_doc.contains("Edited body") && final_doc.contains("status: backlog"),
        "correct-hash confirm should write and preserve frontmatter: {final_doc}"
    );
}

#[test]
fn vault_edit_dry_run_then_confirm() {
    let vault = seeded_vault();
    prebuild_cache(&vault);

    let initialize = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}
    });
    let dry = serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"vault.edit","arguments":{
            "target":"task.md",
            "edits":[{"op":"str_replace","old":"Task body","new":"Edited body"}]
        }}
    });
    let confirm = serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"vault.edit","arguments":{
            "target":"task.md",
            "edits":[{"op":"str_replace","old":"Task body","new":"Edited body"}],
            "confirm":true
        }}
    });

    let mut child = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("mcp")
        .env("XDG_CACHE_HOME", vault.path().join(".xdg-cache"))
        .env("XDG_STATE_HOME", vault.path().join(".xdg-state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    // Read the JSON-RPC response whose `id` matches `want`, skipping any others.
    // `str_replace` is NOT idempotent (once "Task body" → "Edited body" the
    // anchor is gone), and the server answers each tool call on an independent
    // task — so the dry-run and confirm responses can come back in EITHER order
    // if both are in flight. We therefore drive them strictly sequentially:
    // send the dry-run, drain its response, THEN send the confirm. This pins the
    // ordering deterministically without weakening any assertion.
    let mut read_response = |want: i64| -> serde_json::Value {
        loop {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).expect("read response line");
            assert!(n > 0, "stream closed before response id={want}");
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&buf) {
                if v["id"] == want {
                    return v;
                }
            }
        }
    };

    let applied_in = |resp: &serde_json::Value| -> Option<bool> {
        resp.get("result")
            .and_then(|r| r.get("structuredContent"))
            .and_then(|s| s.get("report"))
            .and_then(|rep| rep.get("applied"))
            .and_then(|a| a.as_bool())
    };

    stdin.write_all(&line(initialize)).unwrap();
    let _init = read_response(1);

    // ── Dry-run (confirm omitted → default false): applied=false, no write. ──
    stdin.write_all(&line(dry)).unwrap();
    let dry_resp = read_response(2);
    assert_eq!(
        applied_in(&dry_resp),
        Some(false),
        "dry-run (id=2) report must have applied == false; got: {dry_resp}"
    );
    // The dry-run wrote nothing: the anchor is still on disk for the confirm.
    let mid_doc = std::fs::read_to_string(vault.path().join("task.md")).unwrap();
    assert!(
        mid_doc.contains("Task body") && !mid_doc.contains("Edited body"),
        "dry-run must leave the body unchanged on disk: {mid_doc}"
    );

    // ── Confirm: applied=true, the edit lands on disk. ──
    stdin.write_all(&line(confirm)).unwrap();
    let confirm_resp = read_response(3);
    assert_eq!(
        applied_in(&confirm_resp),
        Some(true),
        "confirm (id=3) report must have applied == true; got: {confirm_resp}"
    );

    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());

    let final_doc = std::fs::read_to_string(vault.path().join("task.md")).unwrap();
    assert!(
        final_doc.contains("Edited body"),
        "confirm should write: {final_doc}"
    );
    assert!(
        final_doc.contains("status: backlog"),
        "frontmatter must be preserved: {final_doc}"
    );
}
