//! Integration round-trip for the `vault.repair` MCP tool (NRN-33, Task 7).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault with a FIXABLE broken wikilink. Asserts:
//!
//! 1. `tools/list` advertises `vault.repair` with a non-empty `inputSchema`.
//! 2. `tools/call` for `vault.repair` against the fixable vault returns a plan
//!    whose `operations` array has ≥1 entry with `kind = "rewrite_link"`.
//! 3. The returned `plan` JSON is structurally a `MigrationPlan` — carries
//!    `schema_version`, `vault_root`, `operations` — so Task 12 (`vault.apply`)
//!    can consume it unchanged.
//!
//! The pure handler is unit-tested inside `src/mcp/tools/repair.rs`; this
//! test covers the rmcp wiring (router registration, schema, call dispatch, envelope
//! shape).
//!
//! Non-flaky by construction: write all requests then close stdin so the server
//! shuts down cleanly — no sleeps, no timeouts. Cache is pre-built before the MCP
//! child starts to avoid concurrent cold-start races (NRN-55).

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

/// Vault with a FIXABLE broken wikilink:
/// - `target-note.md` exists (stem: `target-note`)
/// - `source.md` links to `[[target-not]]` (one-char edit → closest-match proposal)
///
/// Cache is pre-built so the MCP server's first tool call doesn't race a
/// concurrent cold-start rebuild (NRN-55).
fn vault_with_fixable_link() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-repair-plan-rt-")
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

#[test]
fn lists_and_calls_vault_repair() {
    let vault = vault_with_fixable_link();

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

        // 1. initialize
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "norn-repair-plan-client", "version": "0.0.1" }
                }
            })))
            .unwrap();

        // 2. tools/list
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            })))
            .unwrap();

        // 3. vault.repair — no filters → should return ≥1 operation for the fixable link
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.repair",
                    "arguments": {}
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

    // ── tools/list (id=2): vault.repair present with a non-empty inputSchema ──
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));

    let repair_tool = tools
        .iter()
        .find(|t| t["name"] == "vault.repair")
        .unwrap_or_else(|| {
            panic!(
                "tools/list must include vault.repair, got: {:?}",
                tools
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    let schema = &repair_tool["inputSchema"];
    assert!(
        schema.is_object(),
        "vault.repair inputSchema must be an object, got: {schema}"
    );
    assert!(
        schema["properties"]
            .as_object()
            .map(|p| !p.is_empty())
            .unwrap_or(false),
        "vault.repair inputSchema must have non-empty properties, got: {schema}"
    );

    // ── tools/call (id=3): fixable-link vault → ≥1 rewrite_link operation ──
    let repair_resp = responses.iter().find(|r| r["id"] == 3).unwrap_or_else(|| {
        panic!("no tools/call (id=3) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        repair_resp.get("error").is_none(),
        "vault.repair call must not error, got: {repair_resp}"
    );

    // The plan lives under result.structuredContent.plan
    let plan = &repair_resp["result"]["structuredContent"]["plan"];
    assert!(
        plan.is_object(),
        "vault.repair result must carry a plan object, got: {repair_resp}"
    );

    // NRN-231: `has_diagnostic_errors` rides alongside `plan` — the exit-code
    // signal a routed `repair --plan` reconstructs.
    let has_diagnostic_errors =
        &repair_resp["result"]["structuredContent"]["has_diagnostic_errors"];
    assert!(
        has_diagnostic_errors.is_boolean(),
        "vault.repair result must carry a has_diagnostic_errors bool, got: {repair_resp}"
    );
    assert_eq!(
        has_diagnostic_errors, false,
        "the fixable-link fixture has no error-severity diagnostic"
    );

    // ── Structural MigrationPlan checks (Task 12 readiness) ──
    assert_eq!(
        plan["schema_version"], 1,
        "plan must have schema_version=1, got: {:?}",
        plan["schema_version"]
    );
    assert!(
        plan["vault_root"].is_string(),
        "plan must carry vault_root string, got: {:?}",
        plan["vault_root"]
    );

    let ops = plan["operations"]
        .as_array()
        .unwrap_or_else(|| panic!("plan.operations must be an array, got: {plan}"));

    assert!(
        !ops.is_empty(),
        "fixable-link vault must produce ≥1 operation, got 0\nfull plan: {plan}"
    );

    // ── Verify the operation is a rewrite_link (not some other kind) ──
    let rewrite_op = ops
        .iter()
        .find(|op| op["kind"] == "rewrite_link")
        .unwrap_or_else(|| {
            panic!(
                "must include a rewrite_link operation, got kinds: {:?}",
                ops.iter()
                    .map(|op| op["kind"].as_str().unwrap_or("?"))
                    .collect::<Vec<_>>()
            )
        });

    // The operation fields must carry the broken target and proposed fix.
    let fields = &rewrite_op["fields"];
    assert!(
        fields.get("expected_old_value").is_some(),
        "rewrite_link op must carry expected_old_value, got: {fields}"
    );
    assert!(
        fields.get("new_value").is_some(),
        "rewrite_link op must carry new_value, got: {fields}"
    );
    assert_eq!(
        fields["expected_old_value"], "target-not",
        "expected_old_value must be 'target-not' (the broken link), got: {:?}",
        fields["expected_old_value"]
    );
    assert_eq!(
        fields["new_value"], "target-note",
        "new_value must be 'target-note' (the correct stem), got: {:?}",
        fields["new_value"]
    );
}
