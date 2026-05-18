mod graph;
mod pattern;

use camino::Utf8PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("vault root does not exist: {0}")]
    MissingRoot(Utf8PathBuf),
    #[error("vault root is not a directory: {0}")]
    RootNotDirectory(Utf8PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
}

#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub ignore: Vec<String>,
}

pub use graph::{build_index, build_index_with_options, concise_diagnostics, has_errors};
pub use pattern::pattern_matches_path;
