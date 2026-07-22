//! YAML frontmatter: extraction, style-preserving serialization, and the
//! ADR 0008 minimal-edit byte-splicing operations.
//!
//! - [`parse`] pulls the frontmatter block and its parsed value out of a
//!   document ([`ParsedFrontmatter`]).
//! - [`offsets`] locates top-level property spans in the raw source
//!   ([`top_level_property_spans`], [`PropertySpan`], [`ValueStyle`]).
//! - [`quote`] re-serializes values while preserving the author's quoting and
//!   block style, with correctness decided by round-tripping through a real YAML
//!   parser rather than a hand-maintained denylist.
//!
//! The style-preserving splice PRIMITIVES ([`top_level_property_spans`],
//! [`serialize_value_preserving_style`], [`render_key`], …) are the crate's
//! ADR 0008 minimal-edit surface; `norn-core`'s `standards::apply` composes them
//! into the field-operation layer that the mutation verbs drive.

mod offsets;
mod parse;
mod quote;

pub use offsets::{
    frontmatter_property_strings, top_level_property_spans, FrontmatterPropertyString,
    PropertySpan, ValueStyle,
};
pub use parse::{extract_frontmatter, parse, ParsedFrontmatter};
pub use quote::{
    render_key, serialize_array_block_field, serialize_new_document,
    serialize_value_preserving_style, QuoteError,
};
