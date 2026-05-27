//! Shared report types for document mutation commands (`vault move`, `vault delete`).

use camino::Utf8PathBuf;
use serde::Serialize;

/// Summary of link rewrites produced by a mutation operation.
#[derive(Debug, Clone, Serialize)]
pub struct LinkSummary {
    pub total: usize,
    pub files: Vec<LinkFile>,
}

/// Per-file count of link rewrites.
#[derive(Debug, Clone, Serialize)]
pub struct LinkFile {
    pub path: Utf8PathBuf,
    pub count: usize,
}
