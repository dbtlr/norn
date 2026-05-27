//! Cache schema DDL.

use rusqlite::Connection;

use crate::cache::error::CacheError;

pub(crate) const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS documents (
    path             TEXT PRIMARY KEY,
    stem             TEXT NOT NULL,
    hash             TEXT NOT NULL,
    frontmatter_json TEXT,
    body_text        TEXT NOT NULL,
    mtime_ns         INTEGER NOT NULL,
    size_bytes       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_documents_stem ON documents(stem);

CREATE TABLE IF NOT EXISTS files (
    path       TEXT PRIMARY KEY,
    ext        TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mtime_ns   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS links (
    rowid                  INTEGER PRIMARY KEY,
    source_path            TEXT NOT NULL,
    raw                    TEXT NOT NULL,
    kind                   TEXT NOT NULL,
    target_raw             TEXT NOT NULL,
    resolved_path          TEXT,
    anchor                 TEXT,
    block_ref              TEXT,
    label                  TEXT,
    source_span_start      INTEGER,
    source_span_end        INTEGER,
    source_span_line       INTEGER,
    source_span_column     INTEGER,
    source_context         TEXT,
    source_context_property TEXT,
    status                 TEXT NOT NULL,
    unresolved_reason      TEXT,
    candidates_json        TEXT
);
CREATE INDEX IF NOT EXISTS idx_links_source ON links(source_path);
CREATE INDEX IF NOT EXISTS idx_links_resolved ON links(resolved_path);
CREATE INDEX IF NOT EXISTS idx_links_target_raw ON links(target_raw);

CREATE TABLE IF NOT EXISTS headings (
    doc_path                TEXT NOT NULL,
    level                   INTEGER NOT NULL,
    text                    TEXT NOT NULL,
    slug                    TEXT NOT NULL,
    source_span_line        INTEGER,
    source_span_column      INTEGER,
    source_span_byte_offset INTEGER,
    PRIMARY KEY (doc_path, slug)
);
CREATE INDEX IF NOT EXISTS idx_headings_slug ON headings(slug);

CREATE TABLE IF NOT EXISTS block_ids (
    doc_path TEXT NOT NULL,
    block_id TEXT NOT NULL,
    PRIMARY KEY (doc_path, block_id)
);
CREATE INDEX IF NOT EXISTS idx_block_ids_id ON block_ids(block_id);

CREATE TABLE IF NOT EXISTS diagnostics (
    rowid    INTEGER PRIMARY KEY,
    doc_path TEXT NOT NULL,
    severity TEXT NOT NULL,
    code     TEXT NOT NULL,
    message  TEXT NOT NULL,
    detail   TEXT
);
CREATE INDEX IF NOT EXISTS idx_diagnostics_doc ON diagnostics(doc_path);
"#;

pub fn apply_schema(conn: &Connection) -> Result<(), CacheError> {
    conn.execute_batch(DDL)?;
    Ok(())
}
