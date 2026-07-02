mod aliases;
mod build;
mod pattern;

use camino::Utf8PathBuf;

#[derive(Debug, thiserror::Error)]
pub(crate) enum IndexError {
    #[error("vault root does not exist: {0}")]
    MissingRoot(Utf8PathBuf),
    #[error("vault root is not a directory: {0}")]
    RootNotDirectory(Utf8PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
}

#[derive(Debug, Clone)]
pub(crate) struct IndexOptions {
    pub ignore: Vec<String>,
    pub alias_field: Option<String>,
    /// Global auto-index toggle for the derived frontmatter index (Wave 2);
    /// defaults to true. Consumed by the cache writer and query router
    /// (later tasks) — not read anywhere in this task's scope.
    #[allow(dead_code)]
    pub auto: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            alias_field: None,
            auto: true,
        }
    }
}

pub(crate) use aliases::parse_aliases;
pub(crate) use build::{build_index_with_options, concise_diagnostics, has_errors};
// Test-only re-export: build_index is a default-options convenience used solely
// in #[cfg(test)] callers across norn (move_doc, delete_doc, set/validate,
// repair_apply, cache/reader).
#[cfg(test)]
pub(crate) use build::build_index;
