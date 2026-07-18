//! Apply invalidation rules from the cache spec table.
//! Aggressive bias: when in doubt, drop more rather than less.

use camino::Utf8Path;
use rusqlite::{params, Transaction};

use crate::cache::error::CacheError;

/// Delete all rows for the given document path across every table.
pub(crate) fn drop_document(tx: &Transaction, path: &Utf8Path) -> Result<(), CacheError> {
    tx.execute(
        "DELETE FROM documents WHERE path = ?",
        params![path.as_str()],
    )?;
    crate::cache::document_fields::delete_rows(tx, path.as_str())?;
    tx.execute(
        "DELETE FROM headings WHERE doc_path = ?",
        params![path.as_str()],
    )?;
    tx.execute(
        "DELETE FROM block_ids WHERE doc_path = ?",
        params![path.as_str()],
    )?;
    tx.execute(
        "DELETE FROM links WHERE source_path = ?",
        params![path.as_str()],
    )?;
    tx.execute(
        "DELETE FROM diagnostics WHERE doc_path = ?",
        params![path.as_str()],
    )?;
    Ok(())
}
