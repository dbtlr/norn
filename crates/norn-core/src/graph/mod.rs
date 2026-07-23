//! The vault graph: walking the vault root and building a resolved graph index.
//!
//! `build` walks the vault under the configured ignore globs and produces a
//! [`GraphIndex`](crate::domain::GraphIndex) of parsed, link-resolved documents;
//! `aliases` reads the configured alias field; `pattern` matches the ignore
//! globs. The per-file text work is delegated to `norn-frontmatter` (frontmatter,
//! headings, wikilink tokens) and [`crate::links`] (link model + resolution).
//!
//! # Ported seam (ADR 0018)
//!
//! [`IndexOptions`] carries only what the graph build reads: the `ignore` globs
//! and the optional `alias_field`. An earlier design also threaded the derived
//! frontmatter-index field set (`auto`, `resolved_index_set`,
//! `resolved_index_set_hash`) through this struct, but those are consumed solely
//! by the cache writer; they belong to the cache-engine port and are deliberately
//! not carried here.

mod aliases;
mod build;
mod pattern;

use camino::Utf8PathBuf;

/// Failure modes of a graph build that cannot even begin — the root is absent,
/// is not a directory, or a walked path is not valid UTF-8.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("vault root does not exist: {0}")]
    MissingRoot(Utf8PathBuf),
    #[error("vault root is not a directory: {0}")]
    RootNotDirectory(Utf8PathBuf),
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(String),
}

/// The inputs a graph build reads from the vault's configuration: which paths to
/// exclude and which frontmatter field (if any) supplies document aliases.
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    pub ignore: Vec<String>,
    pub alias_field: Option<String>,
}

pub use aliases::parse_aliases;
pub(crate) use build::overlay_changed_paths;
pub use build::{
    build_index_with_options, concise_diagnostics, graph_visible_markdown_under, has_errors,
    is_ignored, is_markdown, vault_root_error,
};
#[cfg(test)]
pub(crate) use build::{docs_parsed_count, docs_parsed_reset};
// Default-options convenience used by in-crate tests.
#[cfg(test)]
pub use build::build_index;
