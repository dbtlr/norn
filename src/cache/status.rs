//! Cache status snapshot for `norn cache status`.

use serde::Serialize;

use crate::cache::error::CacheError;

#[derive(Debug, Clone, Serialize)]
pub struct CacheStatus {
    /// Cache isolation channel: `live` (installed binary) or `dev` (cargo build
    /// tree). `cache_path` is schema-qualified — `<entry>/v{schema}/cache.db` on
    /// live, `<entry>/dev/v{schema}/cache.db` on dev (NRN-269 + NRN-286).
    pub channel: String,
    pub cache_path: camino::Utf8PathBuf,
    pub size_bytes: u64,
    pub doc_count: u64,
    pub file_count: u64,
    pub link_count: u64,
    pub schema_version: u32,
    pub last_full_rebuild: Option<String>,
}

impl crate::cache::Cache {
    pub fn status(&self) -> Result<CacheStatus, CacheError> {
        let db_path = self.cache_dir.join("cache.db");
        let size_bytes = std::fs::metadata(db_path.as_std_path())
            .map(|m| m.len())
            .unwrap_or(0);
        let doc_count: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get::<_, i64>(0))?
            as u64;
        let file_count: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))?
            as u64;
        let link_count: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM links", [], |r| r.get::<_, i64>(0))?
            as u64;
        let schema_version: String = self.conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        let schema_version: u32 = schema_version.parse().unwrap_or(0);
        let last_full_rebuild: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'last_full_rebuild_ts'",
                [],
                |r| r.get(0),
            )
            .ok();
        Ok(CacheStatus {
            channel: self.channel_label().to_string(),
            cache_path: db_path,
            size_bytes,
            doc_count,
            file_count,
            link_count,
            schema_version,
            last_full_rebuild,
        })
    }
}
