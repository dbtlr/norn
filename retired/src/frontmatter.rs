//! YAML frontmatter extraction and style-preserving serialization.
//!
//! The facade over three internal pieces: `parse` pulls the frontmatter block
//! and its parsed value out of a document, `offsets` locates top-level property
//! spans in the raw source, and `quote` re-serializes values while preserving
//! the author's quoting and block style. The graph build reads through here;
//! `set` and the mutation appliers write through the `quote` seam so an edit
//! touches only the field it changed.

mod offsets;
mod parse;
mod quote;

pub(crate) use offsets::{frontmatter_property_strings, top_level_property_spans, ValueStyle};
pub(crate) use parse::extract_frontmatter;
pub(crate) use quote::{
    render_key, serialize_array_block_field, serialize_new_document,
    serialize_value_preserving_style, QuoteError,
};
