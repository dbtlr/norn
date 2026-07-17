//! Integration round-trip for the `vault.describe` MCP tool (NRN-33, Task 8).
//!
//! Drives the real `norn mcp` binary as a child process against a seeded temp
//! vault (same process-level shape as `mcp_validate.rs` — `McpServer` depends on
//! `pub(crate)` types that can't be named from an external test binary). Asserts:
//!
//! 1. `tools/list` advertises `vault.describe`.
//! 2. `tools/call` for `vault.describe` returns `folders`, `path_rules`, and
//!    `schema`, with the seeded notes glob → `type: note` default present and the
//!    notes folder present in the tree.
//! 3. `tools/call` for `vault.describe` against a vault with a creatable rule and
//!    an inbox returns `creatable_rules` with the expected name, target,
//!    `required_vars`, `frontmatter_defaults`, and `body`; and `inbox` equals the
//!    configured path. (NRN-51, Task 7 wire-level coverage.)
//!
//! The pure handler is unit-tested separately inside
//! `src/mcp/tools/describe.rs`; this test covers the rmcp wiring (router
//! registration, schema, call dispatch, envelope shape).
//!
//! Non-flaky by construction: write all requests, then close stdin so the server
//! shuts down cleanly — no sleeps, no timeouts. The cache is pre-built before
//! the MCP child starts to avoid concurrent cold-start races (NRN-55).

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

/// Pre-write a FRESH lazy-sweep throttle marker (`<cache_home>/norn/.last-prune`)
/// so norn invocations under this cache home never spawn a detached GC sweep
/// child (NRN-287) that could race this test. Mirrors src/cache/prune.rs
/// `PRUNE_MARKER`.
fn prewrite_prune_marker(cache_home: &std::path::Path) {
    let tree = cache_home.join("norn");
    let _ = std::fs::create_dir_all(&tree);
    let _ = std::fs::write(tree.join(".last-prune"), b"");
}

/// Vault with docs under `Workspaces/norn/notes/` + a config declaring a notes
/// path rule (`type: note`) and a status schema. Cache is pre-built so the MCP
/// server's first tool call doesn't race a concurrent cold-start rebuild.
fn seeded_vault() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-describe-rt-")
        .tempdir()
        .unwrap();

    let notes = tmp.path().join("Workspaces/norn/notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(
        notes.join("note1.md"),
        "---\ntype: note\ntitle: Note One\n---\nbody\n",
    )
    .unwrap();

    let config_dir = tmp.path().join(".norn");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.yaml"),
        r#"validate:
  rules:
    - name: notes
      match:
        path: "Workspaces/{{workspace}}/notes/*.md"
      allowed_values:
        status:
          - backlog
          - done
      frontmatter_defaults:
        type: note
"#,
    )
    .unwrap();

    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
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
fn lists_and_calls_vault_describe() {
    let vault = seeded_vault();

    prewrite_prune_marker(&vault.path().join(".xdg-cache"));
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
                    "clientInfo": { "name": "norn-describe-client", "version": "0.0.1" }
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

        // 3. vault.describe — no args
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "vault.describe",
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

    // ── tools/list (id=2): vault.describe present ───────────────────────────
    let tools_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list response\nstdout: {stdout}\nstderr: {stderr}"));
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list result.tools must be an array: {tools_resp}"));

    assert!(
        tools.iter().any(|t| t["name"] == "vault.describe"),
        "tools/list must include vault.describe, got: {:?}",
        tools
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );

    // ── tools/call (id=3): folders + path_rules + schema ────────────────────
    let describe_resp = responses.iter().find(|r| r["id"] == 3).unwrap_or_else(|| {
        panic!("no tools/call (id=3) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        describe_resp.get("error").is_none(),
        "vault.describe call must not error, got: {describe_resp}"
    );

    let result = &describe_resp["result"]["structuredContent"];

    // folders contains the notes directory.
    let folders = result["folders"]
        .as_array()
        .unwrap_or_else(|| panic!("describe result must carry a folders array, got: {result}"));
    assert!(
        folders.iter().any(|f| f == "Workspaces/norn/notes"),
        "folders must include Workspaces/norn/notes, got: {folders:?}"
    );

    // path_rules includes the notes glob → type: note default.
    let path_rules = result["path_rules"]
        .as_array()
        .unwrap_or_else(|| panic!("describe result must carry a path_rules array, got: {result}"));
    let notes_rule = path_rules
        .iter()
        .find(|r| r["glob"] == "Workspaces/{{workspace}}/notes/*.md")
        .unwrap_or_else(|| panic!("path_rules must include the notes glob, got: {path_rules:?}"));
    assert_eq!(
        notes_rule["frontmatter_defaults"]["type"], "note",
        "notes rule must give type: note, got: {notes_rule}"
    );

    // schema carries the configured allowed_values.
    let schema_str = serde_json::to_string(&result["schema"]).unwrap();
    assert!(
        schema_str.contains("allowed_values") && schema_str.contains("backlog"),
        "schema must carry the status allowed_values, got: {schema_str}"
    );
}

/// Vault with a creatable rule (`name` + `target` + `body` + `frontmatter_defaults`)
/// and an `inbox.path`. The `target` references `{{var.workspace}}` and
/// `{{title|slugify}}` so that `required_vars` extraction is exercised at the
/// wire level. Cache is pre-built before spawning the MCP child.
fn seeded_vault_with_creatable_rule() -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix("norn-mcp-describe-creatable-rt-")
        .tempdir()
        .unwrap();

    // Seed a doc so `collect_folders` has something to scan.
    let tasks_dir = tmp.path().join("Workspaces/norn/tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("task1.md"),
        "---\ntype: task\ntitle: Task One\n---\nbody\n",
    )
    .unwrap();

    let config_dir = tmp.path().join(".norn");
    std::fs::create_dir_all(&config_dir).unwrap();
    // One creatable rule ("task": has `name` + `target` + `body` +
    // `frontmatter_defaults`) and an inbox. The `notes` rule is a path-only rule
    // (match.path, no target) to confirm it does NOT bleed into `creatable_rules`.
    std::fs::write(
        config_dir.join("config.yaml"),
        "inbox:\n  path: Inbox\nvalidate:\n  rules:\n    - name: task\n      target: \"Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md\"\n      body: \"## Context\\n\"\n      frontmatter_defaults:\n        type: task\n    - name: notes\n      match:\n        path: \"Workspaces/norn/notes/*.md\"\n      frontmatter_defaults:\n        source: notes\n",
    )
    .unwrap();

    prewrite_prune_marker(&tmp.path().join(".xdg-cache"));
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

/// Wire-level contract for `vault.describe`'s `creatable_rules` and `inbox`
/// fields (NRN-51, Task 7).
///
/// Drives the real MCP binary against a vault with:
///   - a creatable rule named "task" with `target`, `body`, and
///     `frontmatter_defaults` (type: task),
///   - a path-only rule "notes" (no target, not creatable),
///   - `inbox.path: Inbox`.
///
/// Asserts at the JSON-RPC wire level:
///   - `creatable_rules` contains exactly one entry with `name = "task"`,
///     `target = "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"`,
///     `required_vars = ["workspace"]`, `frontmatter_defaults.type = "task"`,
///     and a non-empty `body`.
///   - `inbox = "Inbox"`.
#[test]
fn vault_describe_returns_creatable_rules_and_inbox() {
    let vault = seeded_vault_with_creatable_rule();

    prewrite_prune_marker(&vault.path().join(".xdg-cache"));
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
                    "clientInfo": { "name": "norn-creatable-client", "version": "0.0.1" }
                }
            })))
            .unwrap();

        // 2. vault.describe
        stdin
            .write_all(&line(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "vault.describe",
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

    // ── tools/call (id=2): vault.describe result ─────────────────────────────
    let describe_resp = responses.iter().find(|r| r["id"] == 2).unwrap_or_else(|| {
        panic!("no tools/call (id=2) response\nstdout: {stdout}\nstderr: {stderr}")
    });

    assert!(
        describe_resp.get("error").is_none(),
        "vault.describe call must not error, got: {describe_resp}"
    );

    let result = &describe_resp["result"]["structuredContent"];

    // ── creatable_rules: exactly one entry ("task") ──────────────────────────
    let creatable_rules = result["creatable_rules"].as_array().unwrap_or_else(|| {
        panic!("describe result must carry a creatable_rules array, got: {result}")
    });
    assert_eq!(
        creatable_rules.len(),
        1,
        "expected exactly one creatable rule, got: {creatable_rules:?}"
    );
    let task_rule = &creatable_rules[0];

    // name
    assert_eq!(
        task_rule["name"].as_str(),
        Some("task"),
        "creatable rule name must be \"task\", got: {task_rule}"
    );

    // target template (verbatim)
    assert_eq!(
        task_rule["target"].as_str(),
        Some("Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"),
        "creatable rule target mismatch, got: {task_rule}"
    );

    // required_vars: ["workspace"] extracted from {{var.workspace}}
    let required_vars = task_rule["required_vars"]
        .as_array()
        .unwrap_or_else(|| panic!("required_vars must be an array, got: {task_rule}"));
    assert_eq!(
        required_vars.len(),
        1,
        "expected one required var (workspace), got: {required_vars:?}"
    );
    assert_eq!(
        required_vars[0].as_str(),
        Some("workspace"),
        "required_vars[0] must be \"workspace\", got: {required_vars:?}"
    );

    // frontmatter_defaults: type = "task"
    assert_eq!(
        task_rule["frontmatter_defaults"]["type"].as_str(),
        Some("task"),
        "creatable rule frontmatter_defaults.type must be \"task\", got: {task_rule}"
    );

    // body scaffold present and non-empty
    let body = task_rule["body"]
        .as_str()
        .unwrap_or_else(|| panic!("creatable rule body must be a string, got: {task_rule}"));
    assert!(
        !body.is_empty() && body.contains("Context"),
        "creatable rule body must be non-empty and contain \"Context\", got: {body:?}"
    );

    // ── inbox: "Inbox" ───────────────────────────────────────────────────────
    assert_eq!(
        result["inbox"].as_str(),
        Some("Inbox"),
        "inbox must be \"Inbox\", got: {:?}",
        result["inbox"]
    );
}
