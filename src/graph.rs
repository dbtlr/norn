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

#[derive(Debug, Clone, Default)]
pub(crate) struct IndexOptions {
    pub ignore: Vec<String>,
    pub alias_field: Option<String>,
}

pub(crate) use aliases::parse_aliases;
pub(crate) use build::{build_index_with_options, concise_diagnostics, has_errors};
// Test-only re-export: build_index is a default-options convenience used solely
// in #[cfg(test)] callers across vault-cli (move_doc, delete_doc, set/validate,
// repair_apply, cache/reader).
#[cfg(test)]
pub(crate) use build::build_index;
