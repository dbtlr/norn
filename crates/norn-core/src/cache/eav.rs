//! Writer for the derived `document_fields` EAV table (Wave 2 frontmatter
//! index, ADR 0004).
//!
//! Every document gets exactly one row per field in the declared index set:
//! scalars are canonicalized via [`crate::cache::canonical`] so stored values
//! match what the query layer computes; arrays expand to one row per non-null
//! element (an empty array gets a single SQL NULL row, meaning "present, no
//! scalar"); and a field that's absent from frontmatter (or present but null,
//! or whose frontmatter failed to parse entirely) gets the `x'00'` BLOB
//! sentinel — every (doc, declared field) pair always has at least one row.
//! Fields outside the declared index set get no rows at all.
//!
//! Re-shred-on-open (donor `reshred_if_needed`) is intentionally absent (ADR
//! 0017): the db is created fresh at owner summon under a known index set, so
//! the full build shreds against the correct set from birth. There is no cache
//! that predates the current config to reconcile.

use std::collections::BTreeSet;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params, Transaction};

use crate::cache::canonical::canonicalize_scalar;
use crate::cache::error::CacheError;

/// Sentinel marking "field absent from frontmatter, or present with a null
/// value" (including frontmatter that failed to parse entirely, which is
/// treated as all-fields-absent). A BLOB so it can never collide with a
/// legitimately-typed stored value — TEXT/INTEGER/REAL hold real data, and
/// plain SQL NULL is reserved for "array present but empty".
pub(crate) fn absent_sentinel() -> SqlValue {
    SqlValue::Blob(vec![0u8])
}

/// Visit one document's declared frontmatter fields as canonical EAV rows.
/// The field name is borrowed from `index_set`, and each value is produced only
/// when the caller consumes it, so full-build and incremental TEMP staging
/// share one authority without materializing or cloning the full row set.
pub(crate) fn visit_expanded_rows<E>(
    frontmatter: Option<&serde_json::Value>,
    index_set: &BTreeSet<String>,
    mut visit: impl FnMut(&str, SqlValue) -> Result<(), E>,
) -> Result<(), E> {
    for field in index_set {
        match frontmatter.and_then(|fm| fm.get(field)) {
            None | Some(serde_json::Value::Null) => visit(field, absent_sentinel())?,
            Some(serde_json::Value::Array(items)) => {
                let mut emitted = false;
                for item in items.iter().filter(|item| !item.is_null()) {
                    visit(field, canonicalize_scalar(item))?;
                    emitted = true;
                }
                if !emitted {
                    // Empty and all-null arrays share the same "present, no
                    // scalar" meaning: one SQL NULL presence row.
                    visit(field, SqlValue::Null)?;
                }
            }
            Some(other) => visit(field, canonicalize_scalar(other))?,
        }
    }
    Ok(())
}

/// Delete every `document_fields` row for `path`.
pub(crate) fn delete_rows(tx: &Transaction, path: &str) -> Result<(), CacheError> {
    tx.execute("DELETE FROM document_fields WHERE path = ?", params![path])?;
    Ok(())
}

/// Insert fresh `document_fields` rows for `path` against the declared
/// `index_set`. Assumes any stale rows for `path` have already been removed.
pub(crate) fn insert_rows(
    tx: &Transaction,
    path: &str,
    frontmatter: Option<&serde_json::Value>,
    index_set: &BTreeSet<String>,
) -> Result<(), CacheError> {
    visit_expanded_rows(frontmatter, index_set, |field, value| {
        tx.execute(
            "INSERT INTO document_fields (path, key, value) VALUES (?, ?, ?)",
            params![path, field, value],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_with_documents_table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::cache::schema::apply_schema(&conn).unwrap();
        conn
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

    #[test]
    fn row_visitor_preserves_scalar_array_and_absence_semantics() {
        let fm = serde_json::json!({
            "scalar": "[[active]]",
            "empty": [],
            "all_null": [null, null],
            "mixed": ["a", null, 2],
            "null": null,
        });
        let fields = set(&["scalar", "empty", "all_null", "mixed", "null", "absent"]);
        let mut rows = Vec::new();

        visit_expanded_rows(Some(&fm), &fields, |field, value| {
            rows.push((field.to_string(), value));
            Ok::<(), ()>(())
        })
        .unwrap();

        assert_eq!(
            rows,
            vec![
                ("absent".to_string(), absent_sentinel()),
                ("all_null".to_string(), SqlValue::Null),
                ("empty".to_string(), SqlValue::Null),
                ("mixed".to_string(), SqlValue::Text("a".to_string())),
                ("mixed".to_string(), SqlValue::Integer(2)),
                ("null".to_string(), absent_sentinel()),
                ("scalar".to_string(), SqlValue::Text("active".to_string())),
            ]
        );
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

        assert_eq!(rows_for(&conn, "a.md", "tags"), vec![SqlValue::Null]);
    }

    #[test]
    fn array_with_all_null_elements_gets_one_null_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"tags": [null, null]});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["tags"])).unwrap();
        tx.commit().unwrap();

        assert_eq!(rows_for(&conn, "a.md", "tags"), vec![SqlValue::Null]);
    }

    #[test]
    fn null_field_value_gets_sentinel_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"status": null});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        assert_eq!(rows_for(&conn, "a.md", "status"), vec![absent_sentinel()]);
    }

    #[test]
    fn missing_field_gets_sentinel_row() {
        let mut conn = open_with_documents_table();
        let tx = conn.transaction().unwrap();
        let fm: serde_json::Value = serde_json::json!({"other": "x"});
        insert_rows(&tx, "a.md", Some(&fm), &set(&["status"])).unwrap();
        tx.commit().unwrap();

        assert_eq!(rows_for(&conn, "a.md", "status"), vec![absent_sentinel()]);
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
}
