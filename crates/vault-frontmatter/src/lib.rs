mod offsets;
mod parse;

pub use offsets::{
    frontmatter_property_strings, top_level_property_spans, FrontmatterPropertyString,
    PropertySpan, ValueStyle,
};
pub use parse::extract_frontmatter;
