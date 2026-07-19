#![forbid(unsafe_code)]
//! The text layer: frontmatter parse/serialize/edit, headings, sections, wikilink syntax — the edit-without-reformatting invariant. Standalone-publishable future.
//!
//! May never: Know about vaults, schemas, caches, or config.
//!
//! # Surface
//!
//! - [`frontmatter`] — extract the YAML block ([`frontmatter::parse`]), locate
//!   top-level property spans, style-preserving serialization, and the ADR 0008
//!   minimal-edit field operations ([`frontmatter::set_field`],
//!   [`frontmatter::remove_field`], [`frontmatter::add_field`],
//!   [`frontmatter::edit_fields`]). Editing one field leaves every other byte of
//!   the document untouched — the crate's defining invariant.
//! - [`heading`] — Markdown heading parsing and slugs.
//! - [`section`] — heading-delimited section byte ranges.
//! - [`wikilink`] — `[[…]]` token syntax (recognition only; resolution is a
//!   `norn-core` concern).
//! - [`Diagnostic`] / [`SourceSpan`] — the shared diagnostic and position types.
//!
//! Wikilink *resolution* (matching a target to a document, aliases, ambiguity)
//! and Markdown-link / link-graph modeling are deliberately out of scope; they
//! port to `norn-core` atop the syntax this crate provides.

mod diagnostic;
mod span;

pub mod frontmatter;
pub mod heading;
pub mod section;
pub mod wikilink;

pub use diagnostic::{Diagnostic, Severity};
pub use span::SourceSpan;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str =
    "norn-frontmatter: the text layer — frontmatter, headings, sections, wikilink syntax";
