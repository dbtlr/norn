//! Wikilink link-model extraction — turning `norn-frontmatter` wikilink *tokens*
//! into resolution-bearing domain [`Link`] records.
//!
//! The lexical layer (recognizing `[[…]]`, splitting target/anchor/block-ref,
//! excluding code spans) is `norn_frontmatter::wikilink`. This module is the link
//! *model* half the donor entangled with it: it maps each token to a
//! [`LinkKind::Wikilink`] / [`LinkKind::Embed`] [`Link`], records the vault
//! [`LinkSourceArea`] the token came from, and re-bases the token's
//! text-relative span to a content-absolute [`SourceSpan`] matching the donor.

use crate::domain::{Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus, SourceSpan};
use camino::Utf8Path;
use norn_frontmatter::frontmatter::frontmatter_property_strings;
use norn_frontmatter::wikilink::{parse_wikilinks_in_text, Wikilink};
use serde_json::Value;

/// Parse `[[…]]` links in a document body (skipping code spans) into [`Link`]s.
///
/// `content` is the whole document and `body_start` the byte offset of `body`
/// within it, so recorded spans are content-absolute (matching the donor).
pub fn parse_wikilinks(
    source_path: &Utf8Path,
    content: &str,
    body: &str,
    body_start: usize,
) -> Vec<Link> {
    let context = LinkSourceContext {
        area: LinkSourceArea::Body,
        property: None,
    };
    norn_frontmatter::wikilink::parse_wikilinks(body)
        .into_iter()
        .map(|token| token_to_link(source_path, token, Some((content, body_start)), &context))
        .collect()
}

/// Parse `[[…]]` links appearing inside frontmatter string / string-list values.
///
/// Each contributing property value is scanned as raw text (no code-span
/// semantics). When the value's byte offset in `content` is known the span is
/// content-absolute; when it is not (folded/quoted scalars whose offset can't be
/// located) the [`Link`] carries no span, exactly as the donor did.
pub fn parse_frontmatter_wikilinks(
    source_path: &Utf8Path,
    content: &str,
    frontmatter_range: Option<std::ops::Range<usize>>,
    frontmatter: &Value,
) -> Vec<Link> {
    let Some(object) = frontmatter.as_object() else {
        return Vec::new();
    };

    frontmatter_property_strings(object, content, frontmatter_range)
        .into_iter()
        .flat_map(|property_string| {
            let context = LinkSourceContext {
                area: LinkSourceArea::Frontmatter,
                property: Some(property_string.property.clone()),
            };
            let base = property_string.offset.map(|offset| (content, offset));
            parse_wikilinks_in_text(property_string.text)
                .into_iter()
                .map(move |token| token_to_link(source_path, token, base, &context))
        })
        .collect()
}

/// Convert one syntactic [`Wikilink`] token into a domain [`Link`]. `base` is
/// `Some((content, offset))` when the token's text starts at byte `offset` within
/// `content` (so the span can be re-based to content-absolute) or `None` when the
/// origin offset is unknown (yielding a span-less link).
fn token_to_link(
    source_path: &Utf8Path,
    token: Wikilink,
    base: Option<(&str, usize)>,
    context: &LinkSourceContext,
) -> Link {
    let source_span =
        base.map(|(content, offset)| SourceSpan::at(content, offset + token.span.byte_offset));

    Link {
        source_path: source_path.to_path_buf(),
        raw: token.raw,
        kind: if token.embed {
            LinkKind::Embed
        } else {
            LinkKind::Wikilink
        },
        target: token.target,
        label: token.alias,
        anchor: token.anchor,
        block_ref: token.block_ref,
        source_span,
        source_context: Some(context.clone()),
        resolved_path: None,
        unresolved_reason: None,
        candidates: Vec::new(),
        status: LinkStatus::Unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn body_wikilink_becomes_wikilink_link_with_alias_label() {
        let body = "See [[Target|Display]] here.\n";
        let links = parse_wikilinks(Utf8Path::new("a.md"), body, body, 0);
        assert_eq!(links.len(), 1);
        let link = &links[0];
        assert_eq!(link.kind, LinkKind::Wikilink);
        assert_eq!(link.target, "Target");
        assert_eq!(link.label.as_deref(), Some("Display"));
        assert_eq!(
            link.source_context.as_ref().unwrap().area,
            LinkSourceArea::Body
        );
    }

    #[test]
    fn embed_wikilink_becomes_embed_kind() {
        let body = "![[gamma]]\n";
        let links = parse_wikilinks(Utf8Path::new("a.md"), body, body, 0);
        assert_eq!(links[0].kind, LinkKind::Embed);
        assert_eq!(links[0].target, "gamma");
    }

    #[test]
    fn body_wikilink_in_code_span_is_ignored() {
        let body = "code `[[ignored]]` and [[real]]\n";
        let links = parse_wikilinks(Utf8Path::new("a.md"), body, body, 0);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "real");
    }

    #[test]
    fn body_wikilink_span_is_content_absolute() {
        let body = "[[Target]]\n";
        let links = parse_wikilinks(Utf8Path::new("a.md"), body, body, 42);
        assert_eq!(links[0].source_span.unwrap().byte_offset, 42);
    }

    #[test]
    fn block_ref_and_anchor_are_carried_through() {
        let body = "[[Note#Heading]] and [[Note#^blk]]\n";
        let links = parse_wikilinks(Utf8Path::new("a.md"), body, body, 0);
        assert_eq!(links[0].anchor.as_deref(), Some("Heading"));
        assert_eq!(links[0].block_ref, None);
        assert_eq!(links[1].block_ref.as_deref(), Some("blk"));
        assert_eq!(links[1].anchor, None);
    }

    #[test]
    fn frontmatter_wikilink_records_property_and_area() {
        let content = "---\nrelated: \"[[Other]]\"\n---\nbody\n";
        let frontmatter = json!({ "related": "[[Other]]" });
        // The frontmatter block occupies bytes 0..range_end; give the parser the
        // real yaml range so property offsets can be located.
        let range = Some(0..content.find("\n---\nbody").unwrap());
        let links =
            parse_frontmatter_wikilinks(Utf8Path::new("a.md"), content, range, &frontmatter);
        assert_eq!(links.len(), 1);
        let ctx = links[0].source_context.as_ref().unwrap();
        assert_eq!(ctx.area, LinkSourceArea::Frontmatter);
        assert_eq!(ctx.property.as_deref(), Some("related"));
        assert_eq!(links[0].target, "Other");
    }

    #[test]
    fn non_object_frontmatter_yields_no_links() {
        let links = parse_frontmatter_wikilinks(
            Utf8Path::new("a.md"),
            "---\n- a\n---\n",
            None,
            &json!(["a"]),
        );
        assert!(links.is_empty());
    }
}
