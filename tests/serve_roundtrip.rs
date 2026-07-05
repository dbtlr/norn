//! End-to-end MCP round-trips over the `norn serve` socket (NRN-93).
//!
//! One daemon, many vaults: a `hello` names the vault per connection, so two
//! vaults served by the same daemon stay isolated, and reconnecting to a vault
//! reuses its warm context. Also covers a write over the socket landing on disk
//! and being visible to the same daemon's next read (freshness after a
//! self-mutation).

#[path = "serve_util/mod.rs"]
mod serve_util;

use serve_util::*;
use tempfile::TempDir;

/// Create a vault tempdir seeded with `(filename, contents)` docs.
fn seed_vault(prefix: &str, docs: &[(&str, &str)]) -> TempDir {
    let tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    for (name, contents) in docs {
        std::fs::write(tmp.path().join(name), contents).unwrap();
    }
    tmp
}

fn note(title: &str) -> String {
    format!("---\ntype: note\ntitle: {title}\n---\n{title} body\n")
}

fn task(title: &str) -> String {
    format!("---\ntype: task\nstatus: backlog\ntitle: {title}\n---\n{title} body\n")
}

/// `find` for `eq:["type:X"]` with a generous limit (find defaults to 10).
fn find_type(conn: &mut Conn, ty: &str) -> Vec<serde_json::Value> {
    let resp = conn.call_tool(
        "vault.find",
        serde_json::json!({ "eq": [format!("type:{ty}")], "limit": 1000 }),
    );
    find_documents(&resp)
}

/// Two vaults on one daemon stay isolated, and reconnecting to a vault serves
/// its own docs (warm-context reuse; correctness asserted, reuse is unit-tested).
#[test]
fn hello_find_and_two_vault_isolation() {
    let daemon = spawn_ready_daemon();

    let vault_a = seed_vault(
        "norn-serve-rt-a-",
        &[("a1.md", &note("A One")), ("a2.md", &note("A Two"))],
    );
    let vault_b = seed_vault(
        "norn-serve-rt-b-",
        &[
            ("b1.md", &task("B One")),
            ("b2.md", &task("B Two")),
            ("b3.md", &task("B Three")),
        ],
    );

    // Vault A: exactly its 2 notes.
    let mut conn_a = connect_and_hello(&daemon.socket_path, vault_a.path());
    conn_a.initialize();
    let a_notes = find_type(&mut conn_a, "note");
    assert_eq!(
        a_notes.len(),
        2,
        "vault A must return exactly its 2 notes, got: {:?}",
        paths(&a_notes)
    );
    assert_eq!(paths(&a_notes), vec!["a1.md", "a2.md"]);
    // Vault A has no tasks — isolation from B.
    assert!(
        find_type(&mut conn_a, "task").is_empty(),
        "vault A must not see vault B's tasks"
    );

    // New connection, vault B: exactly its 3 tasks.
    let mut conn_b = connect_and_hello(&daemon.socket_path, vault_b.path());
    conn_b.initialize();
    let b_tasks = find_type(&mut conn_b, "task");
    assert_eq!(
        b_tasks.len(),
        3,
        "vault B must return exactly its 3 tasks, got: {:?}",
        paths(&b_tasks)
    );
    assert_eq!(paths(&b_tasks), vec!["b1.md", "b2.md", "b3.md"]);

    // Third connection back to A: still A's docs (warm context reused).
    let mut conn_a2 = connect_and_hello(&daemon.socket_path, vault_a.path());
    conn_a2.initialize();
    let a_again = find_type(&mut conn_a2, "note");
    assert_eq!(
        paths(&a_again),
        vec!["a1.md", "a2.md"],
        "reconnecting to vault A must still serve A's docs"
    );
}

/// A `vault.set` over the socket writes to disk AND is visible to the same
/// daemon's next read (freshness after a self-mutation).
#[test]
fn mutation_over_socket() {
    let daemon = spawn_ready_daemon();
    let vault = seed_vault(
        "norn-serve-rt-mut-",
        &[(
            "alpha.md",
            "---\ntype: note\nstatus: draft\ntitle: Alpha\n---\nAlpha body\n",
        )],
    );

    let mut conn = connect_and_hello(&daemon.socket_path, vault.path());
    conn.initialize();

    // Confirmed set: status draft -> active.
    let set = conn.call_tool(
        "vault.set",
        serde_json::json!({
            "target": "alpha",
            "set": { "status": "active" },
            "confirm": true,
        }),
    );
    assert!(
        set.get("error").is_none(),
        "vault.set must not error, got: {set}"
    );
    assert_eq!(
        set["result"]["structuredContent"]["report"]["applied"].as_bool(),
        Some(true),
        "confirmed set must report applied == true, got: {set}"
    );

    // Verify ON DISK: the frontmatter changed.
    let disk = read_to_string(&vault.path().join("alpha.md"));
    assert!(
        disk.contains("status: active"),
        "the confirmed set must land on disk, file was:\n{disk}"
    );
    assert!(
        !disk.contains("status: draft"),
        "the old value must be gone on disk, file was:\n{disk}"
    );

    // Same daemon, same connection: the next read sees the new value.
    let active = find_type_status(&mut conn, "active");
    assert_eq!(
        paths(&active),
        vec!["alpha.md"],
        "the same daemon must see its own write (freshness after self-mutation)"
    );

    let recs =
        get_records(&conn.call_tool("vault.get", serde_json::json!({ "targets": ["alpha"] })));
    assert_eq!(recs.len(), 1, "vault.get must resolve alpha, got: {recs:?}");
    assert_eq!(
        recs[0]["frontmatter"]["status"], "active",
        "vault.get must reflect the mutated status, got: {}",
        recs[0]
    );
}

/// `find` for `eq:["status:X"]` with a generous limit.
fn find_type_status(conn: &mut Conn, status: &str) -> Vec<serde_json::Value> {
    let resp = conn.call_tool(
        "vault.find",
        serde_json::json!({ "eq": [format!("status:{status}")], "limit": 1000 }),
    );
    find_documents(&resp)
}
