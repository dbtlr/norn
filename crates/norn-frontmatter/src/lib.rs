#![forbid(unsafe_code)]
//! The text layer: frontmatter parse/serialize/edit, headings, sections, wikilink syntax — the edit-without-reformatting invariant. Standalone-publishable future.
//!
//! May never: Know about vaults, schemas, caches, or config.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str =
    "norn-frontmatter: the text layer — frontmatter, headings, sections, wikilink syntax";
