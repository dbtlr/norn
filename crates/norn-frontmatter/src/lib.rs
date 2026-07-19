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
//!
//! # Code blocks are opaque (ADR 0019)
//!
//! Every targeted body parser here treats fenced code blocks, indented code blocks, and inline code
//! spans as a *different document*: no semantic token — a wikilink, a heading, a
//! block-id, or anything a future parser extracts — may match inside them. What a
//! reader sees as a literal code sample, norn reads as literal text, never as
//! vault structure. [`wikilink::parse_wikilinks`] and
//! [`wikilink::parse_block_ids`] exclude code byte-ranges explicitly; the
//! `pulldown-cmark`-based [`heading`] parser skips code events for free. Any
//! parser added later MUST honor this rule or justify the exception against the
//! ADR. The one nuance: a `^block-id` on the line *after* a fence references the
//! code block itself and stays valid — the exclusion covers what is *inside* the
//! fences, not the anchor line trailing them.

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
