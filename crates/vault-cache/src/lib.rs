//! SQLite-backed cache for the vault graph.
//!
//! Acts as the read path for query commands: `Cache::load_graph_index` returns
//! the same `GraphIndex` shape that `vault-graph::build_index` does, but loads
//! from SQLite instead of walking the filesystem.
//!
//! The cache is *disposable*. Missing, corrupted, schema-mismatched, or
//! identity-drifted caches trigger a silent rebuild rather than erroring.

pub mod error;
pub mod query;

pub use error::CacheError;
pub use find::{FindQuery, FindResult, SortClause, SortDirection};
pub use query::{json_path_for, DocumentQuery};
pub use vault_core::DocumentSummary;

mod change_detection;
mod find;
mod identity;
mod invalidation;
mod lock;
mod open;
mod query_diagnostics;
mod query_documents;
mod query_files;
mod query_links;
mod reader;
mod schema;
mod status;
mod writer;

pub use change_detection::{detect, ChangeDetectOptions, FileChange};
pub use status::CacheStatus;
pub use writer::{IndexOptions, IndexReport};

pub const SCHEMA_VERSION: u32 = 2;

/// Handle to an opened cache. Holds a rusqlite Connection plus the resolved
/// vault root and cache directory path.
pub struct Cache {
    pub(crate) conn: rusqlite::Connection,
    pub(crate) vault_root: camino::Utf8PathBuf,
    pub(crate) cache_dir: camino::Utf8PathBuf,
}

impl Cache {
    /// Delete the on-disk cache (database + WAL/SHM siblings). Holds the
    /// advisory write lock for the duration. After clear the caller should
    /// drop the `Cache` handle; the next `Cache::open` recreates a fresh
    /// database with the current schema and identity meta rows.
    pub fn clear(&mut self) -> Result<(), CacheError> {
        let _lock = lock::WriteLock::acquire(&self.cache_dir, std::time::Duration::from_secs(5))?;
        let db_path = self.cache_dir.join("cache.db");
        // Detach the live connection from the on-disk database so the file
        // can be removed cleanly on platforms (notably Windows) where an
        // open handle blocks deletion. Replace with an in-memory connection
        // so `&mut self.conn` remains usable until the caller drops us.
        drop(std::mem::replace(
            &mut self.conn,
            rusqlite::Connection::open_in_memory()?,
        ));
        if db_path.as_std_path().exists() {
            std::fs::remove_file(db_path.as_std_path()).map_err(|e| CacheError::Io {
                path: db_path.clone(),
                source: e,
            })?;
        }
        let wal = self.cache_dir.join("cache.db-wal");
        let shm = self.cache_dir.join("cache.db-shm");
        let _ = std::fs::remove_file(wal.as_std_path());
        let _ = std::fs::remove_file(shm.as_std_path());
        Ok(())
    }

    /// Expose the underlying connection for query-plan tests.
    ///
    /// Only compiled when the `test-utils` feature is enabled or during
    /// `cargo test` (integration tests need it via the crate's public API).
    #[doc(hidden)]
    pub fn conn_for_test(&self) -> &rusqlite::Connection {
        &self.conn
    }
}
