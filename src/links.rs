//! Link parsing and resolution.
//!
//! The facade over the link layer: `commonmark` parses Markdown links,
//! `wikilink` parses `[[…]]` links in the body and frontmatter, `block` reads
//! block IDs, `anchor` handles heading/block anchors, and `resolve` turns a
//! parsed reference into a target path in the vault. The graph build calls
//! these per file; the resolved `core::Link`s it produces feed backlink and
//! link-rewrite work everywhere else.

mod anchor;
mod block;
mod commonmark;
mod resolve;
mod wikilink;

pub(crate) use block::parse_block_ids;
pub(crate) use commonmark::parse_commonmark;
pub(crate) use resolve::resolve_links;
pub(crate) use wikilink::{parse_frontmatter_wikilinks, parse_wikilinks};
