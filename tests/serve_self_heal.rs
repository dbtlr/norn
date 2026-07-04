//! Self-heal / freshness coverage for the `norn serve` daemon (NRN-93).
//!
//! A warm context checks integrity once, then keeps itself honest per request:
//! it must see external edits, survive its cache database being cleared out from
//! under it (the POSIX-ghost self-heal), and apply an index-relevant config
//! change — all without a daemon restart.

#[path = "serve_util/mod.rs"]
mod serve_util;

use std::process::Command;

use serve_util::*;
use tempfile::TempDir;

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

fn find(conn: &mut Conn, args: serde_json::Value) -> Vec<serde_json::Value> {
    find_documents(&conn.call_tool("vault.find", args))
}

fn find_notes(conn: &mut Conn) -> Vec<serde_json::Value> {
    find(
        conn,
        serde_json::json!({ "eq": ["type:note"], "limit": 1000 }),
    )
}

/// A warm context sees a doc created after the vault was opened.
#[test]
fn freshness_sees_external_edit() {
    let daemon = spawn_ready_daemon();
    let vault = seed_vault(
        "norn-serve-heal-edit-",
        &[("n1.md", &note("One")), ("n2.md", &note("Two"))],
    );

    let mut conn = connect_and_hello(&daemon.socket_path, vault.path());
    conn.initialize();

    let before = find_notes(&mut conn);
    assert_eq!(
        before.len(),
        2,
        "baseline: 2 notes, got {:?}",
        paths(&before)
    );

    // External editor writes a new doc directly to the vault.
    std::fs::write(vault.path().join("n3.md"), note("Three")).unwrap();

    let after = find_notes(&mut conn);
    assert_eq!(
        paths(&after),
        vec!["n1.md", "n2.md", "n3.md"],
        "the same connection must see the externally-created doc"
    );
}

/// `norn cache clear` nukes the daemon's live database; the next request must
/// still succeed with correct results (the daemon rebuilds — POSIX-ghost heal).
#[test]
fn ground_shift_cache_clear() {
    let daemon = spawn_ready_daemon();
    let vault = seed_vault(
        "norn-serve-heal-clear-",
        &[
            ("n1.md", &note("One")),
            ("n2.md", &note("Two")),
            ("n3.md", &note("Three")),
        ],
    );

    let mut conn = connect_and_hello(&daemon.socket_path, vault.path());
    conn.initialize();
    assert_eq!(find_notes(&mut conn).len(), 3, "baseline: 3 notes");

    // Clear the cache database under the live daemon, via a CLI child sharing the
    // daemon's XDG dirs (so it targets the SAME per-vault cache.db).
    let clear = Command::new(norn_bin())
        .arg("--cwd")
        .arg(vault.path())
        .arg("cache")
        .arg("clear")
        .env("XDG_CACHE_HOME", &daemon.cache_home)
        .env("XDG_STATE_HOME", &daemon.state_home)
        .output()
        .expect("run norn cache clear");
    assert!(
        clear.status.success(),
        "norn cache clear failed: {}",
        String::from_utf8_lossy(&clear.stderr)
    );

    // Next request rebuilds and still returns the correct set.
    let after = find_notes(&mut conn);
    assert_eq!(
        paths(&after),
        vec!["n1.md", "n2.md", "n3.md"],
        "the daemon must rebuild after its cache was cleared"
    );
}

/// An index-relevant config change (adding `links.alias_field`) applies live:
/// a wikilink that only resolves via an alias flips from unresolved to resolved
/// on the same connection's next request — no daemon restart.
#[test]
fn config_index_change_applies() {
    let daemon = spawn_ready_daemon();
    // `target.md` carries an alias "Vault Memory" (its stem is "target", and its
    // title is NOT "Vault Memory", so `[[Vault Memory]]` can ONLY resolve via the
    // alias field). `src.md` links to it by that alias.
    let vault = seed_vault(
        "norn-serve-heal-cfg-",
        &[
            (
                "target.md",
                "---\ntype: note\ntitle: Target\naliases:\n  - Vault Memory\n---\nTarget body\n",
            ),
            (
                "src.md",
                "---\ntype: note\ntitle: Source\n---\nsee [[Vault Memory]]\n",
            ),
        ],
    );

    let mut conn = connect_and_hello(&daemon.socket_path, vault.path());
    conn.initialize();

    // Before: no alias_field configured, so `[[Vault Memory]]` is unresolved and
    // src.md shows up in the unresolved-links set.
    let unresolved_before = find(
        &mut conn,
        serde_json::json!({ "unresolved_links": true, "limit": 1000 }),
    );
    assert!(
        paths(&unresolved_before).contains(&"src.md".to_string()),
        "before config: src.md's alias link must be unresolved, got {:?}",
        paths(&unresolved_before)
    );

    // Add an index-relevant config change: alias_field feeds link resolution.
    std::fs::create_dir_all(vault.path().join(".norn")).unwrap();
    std::fs::write(
        vault.path().join(".norn/config.yaml"),
        "links:\n  alias_field: aliases\n",
    )
    .unwrap();

    // After: the same connection's next request reopens the context with the new
    // index options — the alias link now resolves, so src.md is no longer
    // unresolved, and links_to(target) finds it.
    let unresolved_after = find(
        &mut conn,
        serde_json::json!({ "unresolved_links": true, "limit": 1000 }),
    );
    assert!(
        !paths(&unresolved_after).contains(&"src.md".to_string()),
        "after config: src.md's alias link must now resolve, got {:?}",
        paths(&unresolved_after)
    );

    let links_to_target = find(
        &mut conn,
        serde_json::json!({ "links_to": ["target"], "limit": 1000 }),
    );
    assert!(
        paths(&links_to_target).contains(&"src.md".to_string()),
        "after config: links_to(target) must resolve the alias link from src.md, got {:?}",
        paths(&links_to_target)
    );
}
