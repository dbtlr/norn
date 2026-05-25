mod offsets;
mod parse;
pub mod quote;

pub use offsets::{
    append_frontmatter_field, frontmatter_property_strings, top_level_property_spans,
    FrontmatterPropertyString, PropertySpan, ValueStyle,
};
pub use parse::extract_frontmatter;
pub use quote::{
    serialize_array_block_for_new_field, serialize_value_preserving_style, QuoteError,
};
