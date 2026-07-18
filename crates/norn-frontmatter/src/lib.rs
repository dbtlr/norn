#![forbid(unsafe_code)]
//! The text layer: frontmatter parse/serialize/edit, headings, sections, wikilink syntax — the edit-without-reformatting invariant. Standalone-publishable future.
//!
//! May never: Know about vaults, schemas, caches, or config.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str = "norn-frontmatter: The text layer: frontmatter parse/serialize/edit, headings, sections, wikilink syntax — the edit-without-reformatting invariant.";
