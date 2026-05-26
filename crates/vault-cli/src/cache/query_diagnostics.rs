//! Diagnostic count primitive — used for exit-code derivation on
//! cache-direct command paths.

use crate::cache::error::CacheError;

impl crate::cache::Cache {
    /// True if any document has at least one diagnostic with `severity = 'error'`.
    /// Replaces `exit_code_for(&index)` for command paths that don't build a
    /// full GraphIndex.
    pub fn has_diagnostic_errors(&self) -> Result<bool, CacheError> {
        let mut stmt = self
            .conn
            .prepare("SELECT EXISTS(SELECT 1 FROM diagnostics WHERE severity = 'error')")?;
        let exists: i64 = stmt.query_row([], |row| row.get(0))?;
        Ok(exists != 0)
    }
}
