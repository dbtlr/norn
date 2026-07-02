//! Writer and re-shred logic for the derived `document_fields` EAV table
//! (Wave 2 frontmatter index — see `crate::standards::index_policy`).
//!
//! Every document gets exactly one row per field in the declared index set:
//! scalars are canonicalized via `crate::cache::canonical` so stored values
//! match what the query layer computes; arrays expand to one row per
//! non-null element (an empty array gets a single SQL NULL row, meaning
//! "present, no scalar"); and a field that's absent from frontmatter (or
//! present but null, or whose frontmatter failed to parse entirely) gets the
//! `x'00'` BLOB sentinel — every (doc, declared field) pair always has at
//! least one row. Fields outside the declared index set get no rows at all.

use std::collections::BTreeSet;

use camino::Utf8Path;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Connection, Transaction};

use crate::cache::canonical::canonicalize_scalar;
use crate::cache::error::CacheError;
use crate::cache::lock::WriteLock;

/// Sentinel marking "field absent from frontmatter, or present with a null
/// value" (including frontmatter that failed to parse entirely, which is
/// treated as all-fields-absent). A BLOB so it can never collide with a
/// legitimately-typed stored value — TEXT/INTEGER/REAL hold real data, and
/// plain SQL NULL is reserved for "array present but empty".
pub(crate) fn absent_sentinel() -> SqlValue {
    SqlValue::Blob(vec![0u8])
}

/// Row values for one (document, field) pair, given the document's
/// (possibly-missing/unparsed) frontmatter.
fn field_row_values(frontmatter: Option<&serde_json::Value>, field: &str) -> Vec<SqlValue> {
    match frontmatter.and_then(|fm| fm.get(field)) {
        None | Some(serde_json::Value::Null) => vec![absent_sentinel()],
        Some(serde_json::Value::Array(items)) => {
            if items.is_empty() {
                vec![SqlValue::Null]
            } else {
                let non_null: Vec<SqlValue> = items
                    .iter()
                    .filter(|v| !v.is_null())
                    .map(canonicalize_scalar)
                    .collect();
                if non_null.is_empty() {
                    // Non-empty array, but every element was null: same
                    // "present, no scalar" meaning as an empty array — emit
                    // the single NULL presence row rather than zero rows.
                    vec![SqlValue::Null]
                } else {
                    non_null
                }
            }
        }
        Some(other) => vec![canonicalize_scalar(other)],
    }
}

/// Delete every `document_fields` row for `path`. Callers rewriting a
/// document's rows call this before `insert_rows`; callers dropping a
/// document entirely (delete/move/rename) call this alone.
pub(crate) fn delete_rows(tx: &Transaction, path: &str) -> Result<(), CacheError> {
    tx.execute("DELETE FROM document_fields WHERE path = ?", params![path])?;
    Ok(())
}

/// Insert fresh `document_fields` rows for `path` against the declared
/// `index_set`. Assumes any stale rows for `path` have already been removed
/// — a full rebuild clears the whole table up front; incremental refresh
/// goes through `invalidation::drop_document` (which calls `delete_rows`)
/// before re-inserting.
pub(crate) fn insert_rows(
    tx: &Transaction,
    path: &str,
    frontmatter: Option<&serde_json::Value>,
    index_set: &BTreeSet<String>,
) -> Result<(), CacheError> {
    for field in index_set {
        for value in field_row_values(frontmatter, field) {
            tx.execute(
                "INSERT INTO document_fields (path, key, value) VALUES (?, ?, ?)",
                params![path, field, value],
            )?;
        }
    }
    Ok(())
}

/// Re-derive every `document_fields` row from the cached
/// `documents.frontmatter_json` column — no filesystem re-parse. Caller is
/// responsible for clearing the table first (see `reshred_if_needed`).
fn reshred_all(tx: &Transaction, index_set: &BTreeSet<String>) -> Result<(), CacheError> {
    let mut stmt = tx.prepare("SELECT path, frontmatter_json FROM documents")?;
    let rows = stmt.query_map([], |r| {
        let path: String = r.get(0)?;
        let frontmatter_json: Option<String> = r.get(1)?;
        Ok((path, frontmatter_json))
    })?;
    let mut docs: Vec<(String, Option<String>)> = Vec::new();
    for row in rows {
        docs.push(row?);
    }
    drop(stmt);

    for (path, frontmatter_json) in docs {
        let frontmatter: Option<serde_json::Value> = frontmatter_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        insert_rows(tx, &path, frontmatter.as_ref(), index_set)?;
    }
    Ok(())
}

/// Re-shred `document_fields` from cached frontmatter when the resolved
/// index set has changed since the cache was last built — or its
/// `index_set_hash` meta row is missing entirely (caches that predate this
/// column). Silent: no user-facing output, matching the cache's normal
/// "disposable, self-healing" posture. A no-op (hash already matches) skips
/// the lock, the transaction, and the `ANALYZE` entirely — the fast path
/// never blocks. The `ANALYZE` is scoped to `document_fields` alone so it
/// refreshes that table's own planner stats without perturbing existing
/// planner decisions for `links`/`documents`.
///
/// Callers must be authoritative (see `Cache::open_with_index`) — this is
/// the only place a re-shred is initiated on open.
///
/// Takes the per-vault `WriteLock` before mutating, exactly like
/// `Cache::rebuild`/`index_incremental`: the initial hash check above is a
/// cheap unlocked pre-check, so once the lock is held the stored hash is
/// re-read and re-compared (TOCTOU guard) in case a concurrent process
/// already reconciled while this one was waiting.
pub(crate) fn reshred_if_needed(
    conn: &mut Connection,
    cache_dir: &Utf8Path,
    index_set: &BTreeSet<String>,
    index_set_hash: &str,
) -> Result<(), CacheError> {
    if stored_index_set_hash(conn) == Some(index_set_hash.to_string()) {
        return Ok(());
    }

    let _lock = WriteLock::acquire(cache_dir, std::time::Duration::from_secs(5))?;

    // Re-check inside the lock: another process may have already reconciled
    // the hash while we were waiting for it to release.
    if stored_index_set_hash(conn) == Some(index_set_hash.to_string()) {
        return Ok(());
    }

    let tx = conn.transaction()?;
    tx.execute("DELETE FROM document_fields", [])?;
    reshred_all(&tx, index_set)?;
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('index_set_hash', ?)",
        params![index_set_hash],
    )?;
    tx.commit()?;
    conn.execute("ANALYZE document_fields", [])?;
    Ok(())
}

fn stored_index_set_hash(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'index_set_hash'",
        [],
        |r| r.get(0),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_with_documents_table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::cache::schema::apply_schema(&conn).unwrap();
        conn
    }

    fn insert_doc(conn: &Connection, path: &str, frontmatter_json: Option<&str>) {
        conn.execute(
            "INSERT INTO documents (path, stem, hash, frontmatter_json, body_text, mtime_ns, size_bytes) \
             VALUES (?, ?, 'h', ?, '', 0, 0)",
            params![path, path, frontmatter_json],
        )
        .unwrap();
    }

    fn rows_for(conn: &Connection, path: &str, field: &str) -> Vec<SqlValue> {
        let mut stmt = conn
            .prepare("SELECT value FROM document_fields WHERE path = ? AND key = ? ORDER BY rowid")
            .unwrap();
        stmt.query_map(params![path, field], |r| r.get::<_, SqlValue>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn set(fields: &[&str]) -> BTreeSet<String> {
        fields.iter().map(|s| s.to_string()).collect()
    }

    /// `reshred_if_needed` acquires the `WriteLock` on a hash mismatch, which
    /// needs a real filesystem directory for the `.lock` file — independent
    /// of the (often in-memory) `Connection` under test.
    fn temp_cache_dir() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        (tmp, dir)
    }

    #[test]
    fn scalar_string_value_gets_one_canonicalized_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"status": "[[active]]"});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "status");
        assert_eq!(rows, vec![SqlValue::Text("active".to_string())]);
    }

    #[test]
    fn array_value_gets_one_row_per_element() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"tags": ["a", "b", "c"]});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["tags"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "tags");
        assert_eq!(
            rows,
            vec![
                SqlValue::Text("a".to_string()),
                SqlValue::Text("b".to_string()),
                SqlValue::Text("c".to_string()),
            ]
        );
    }

    #[test]
    fn empty_array_gets_one_null_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"tags": []});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["tags"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "tags");
        assert_eq!(rows, vec![SqlValue::Null]);
    }

    #[test]
    fn array_with_null_elements_skips_them() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"tags": ["a", null, "b"]});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["tags"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "tags");
        assert_eq!(
            rows,
            vec![
                SqlValue::Text("a".to_string()),
                SqlValue::Text("b".to_string()),
            ]
        );
    }

    #[test]
    fn array_with_all_null_elements_gets_one_null_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"tags": [null, null]});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["tags"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "tags");
        assert_eq!(
            rows,
            vec![SqlValue::Null],
            "non-empty array whose elements are all null must still get the \
             single NULL presence row, same as a genuinely empty array"
        );
    }

    #[test]
    fn null_field_value_gets_sentinel_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"status": null});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "status");
        assert_eq!(rows, vec![absent_sentinel()]);
    }

    #[test]
    fn missing_field_gets_sentinel_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"other": "x"});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        let rows = rows_for(&conn, "a.md", "status");
        assert_eq!(rows, vec![absent_sentinel()]);
    }

    #[test]
    fn undeclared_field_produces_no_rows() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"status": "active", "secret": "x"});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        assert!(rows_for(&conn, "a.md", "secret").is_empty());
    }

    #[test]
    fn parse_failed_frontmatter_produces_all_sentinel_rows() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        insert_rows(&tx, "a.md", None, &set(&["status", "tags"])).unwrap();
        tx.commit().unwrap();

        assert_eq!(rows_for(&conn, "a.md", "status"), vec![absent_sentinel()]);
        assert_eq!(rows_for(&conn, "a.md", "tags"), vec![absent_sentinel()]);
    }

    #[test]
    fn typed_scalars_round_trip_with_correct_sqlite_types() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({
            "count": 42,
            "score": 2.5,
            "flag": true,
        });
        insert_rows(&tx, "a.md", Some(&fm), &set(&["count", "score", "flag"])).unwrap();
        tx.commit().unwrap();

        assert_eq!(
            rows_for(&conn, "a.md", "count"),
            vec![SqlValue::Integer(42)]
        );
        assert_eq!(rows_for(&conn, "a.md", "score"), vec![SqlValue::Real(2.5)]);
        assert_eq!(rows_for(&conn, "a.md", "flag"), vec![SqlValue::Integer(1)]);
    }

    #[test]
    fn delete_rows_then_insert_rows_replaces_exactly() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm1: serde_json::Value = serde_json::json!({"status": "backlog"});
        insert_rows(&tx, "a.md", Some(&fm1), &set(&["status"])).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            rows_for(&conn, "a.md", "status"),
            vec![SqlValue::Text("backlog".to_string())]
        );

        let tx = conn.transaction().unwrap();
        delete_rows(&tx, "a.md").unwrap();
        let fm2: serde_json::Value = serde_json::json!({"status": "done"});
        insert_rows(&tx, "a.md", Some(&fm2), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        assert_eq!(
            rows_for(&conn, "a.md", "status"),
            vec![SqlValue::Text("done".to_string())]
        );
    }

    #[test]
    fn reshred_if_needed_regenerates_rows_from_cached_frontmatter_json() {
        let mut conn = open_with_documents_table();
        insert_doc(&conn, "a.md", Some(r#"{"status": "active"}"#));
        insert_doc(&conn, "b.md", Some(r#"{"other": "x"}"#));
        let (_tmp, cache_dir) = temp_cache_dir();

        reshred_if_needed(&mut conn, &cache_dir, &set(&["status"]), "hash-1").unwrap();

        assert_eq!(
            rows_for(&conn, "a.md", "status"),
            vec![SqlValue::Text("active".to_string())]
        );
        assert_eq!(rows_for(&conn, "b.md", "status"), vec![absent_sentinel()]);

        let stamped: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'index_set_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stamped, "hash-1");
    }

    #[test]
    fn reshred_if_needed_is_a_no_op_when_hash_matches() {
        let mut conn = open_with_documents_table();
        insert_doc(&conn, "a.md", Some(r#"{"status": "active"}"#));
        let (_tmp, cache_dir) = temp_cache_dir();
        reshred_if_needed(&mut conn, &cache_dir, &set(&["status"]), "hash-1").unwrap();

        // Tamper with a row directly; if reshred runs again it would be
        // regenerated back to the real value, so an unchanged tamper proves
        // the second call was a no-op.
        conn.execute(
            "UPDATE document_fields SET value = 'tampered' WHERE path = 'a.md' AND key = 'status'",
            [],
        )
        .unwrap();

        reshred_if_needed(&mut conn, &cache_dir, &set(&["status"]), "hash-1").unwrap();

        assert_eq!(
            rows_for(&conn, "a.md", "status"),
            vec![SqlValue::Text("tampered".to_string())]
        );
    }

    #[test]
    fn reshred_if_needed_runs_when_hash_missing() {
        let mut conn = open_with_documents_table();
        insert_doc(&conn, "a.md", Some(r#"{"status": "active"}"#));
        let (_tmp, cache_dir) = temp_cache_dir();
        // No prior index_set_hash meta row at all.
        reshred_if_needed(&mut conn, &cache_dir, &set(&["status"]), "hash-1").unwrap();

        assert_eq!(
            rows_for(&conn, "a.md", "status"),
            vec![SqlValue::Text("active".to_string())]
        );
    }

    // Concurrency/TOCTOU regression: a hash mismatch must acquire the
    // per-vault `WriteLock` before mutating, exactly like `Cache::rebuild`.
    // Holds the lock externally for a short window and asserts
    // `reshred_if_needed` blocks for (at least) that window rather than
    // racing straight into DELETE+INSERT.
    #[test]
    fn reshred_if_needed_waits_for_write_lock_held_by_another_holder() {
        let mut conn = open_with_documents_table();
        insert_doc(&conn, "a.md", Some(r#"{"status": "active"}"#));
        let (_tmp, cache_dir) = temp_cache_dir();

        let lock_path = cache_dir.join(".lock");
        let held =
            crate::cache::acquire_flock(&lock_path, std::time::Duration::from_millis(100)).unwrap();
        let holder = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(held);
        });

        let start = std::time::Instant::now();
        reshred_if_needed(&mut conn, &cache_dir, &set(&["status"]), "hash-1").unwrap();
        let elapsed = start.elapsed();
        holder.join().unwrap();

        assert!(
            elapsed >= std::time::Duration::from_millis(150),
            "reshred_if_needed should have waited for the external lock \
             holder to release, elapsed={elapsed:?}"
        );
        assert_eq!(
            rows_for(&conn, "a.md", "status"),
            vec![SqlValue::Text("active".to_string())]
        );
    }
}
