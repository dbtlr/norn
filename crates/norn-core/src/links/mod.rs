//! The link model and resolution.
//!
//! The vault-semantic half of links: `markdown` extracts `[text](url)` body links
//! and image embeds into [`domain::Link`](crate::domain::Link) records,
//! `wikilink` maps `norn-frontmatter` wikilink *tokens* into the same link model
//! (recording the vault source area and a content-absolute span), and `resolve`
//! matches a parsed link's target to a document — resolved / unresolved /
//! ambiguous, with anchor and block-ref validation.
//!
//! # Ported seam (ADR 0018)
//!
//! The *syntax* half — `[[…]]` token recognition, anchor / block-id splitting,
//! slug generation, and heading parsing — was ported to `norn-frontmatter`
//! (NRN-339) and is consumed here, not re-implemented. The [`domain::Link`] value
//! types moved to [`crate::domain`] with the core model (NRN-340). What this
//! module adds is Markdown-link extraction and all of resolution — matching a
//! link to a document — which is the link *model*, not text syntax.

pub mod markdown;
pub mod resolve;
mod target;
pub mod wikilink;

pub use markdown::parse_markdown_links;
pub use resolve::resolve_links;
pub use wikilink::{parse_frontmatter_wikilinks, parse_wikilinks};
