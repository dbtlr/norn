//! Markdown-link extraction — the link-producing half of the commonmark pass.
//!
//! Walking the `pulldown-cmark` event stream once collects both headings and
//! `[text](url)` / `![alt](url)` links. Heading (and slug) parsing is text-layer
//! syntax and lives in `norn_frontmatter::heading`; this module keeps only the
//! link half, which is part of the link *model* — it produces resolution-bearing
//! [`Link`] records that later match to documents.

use crate::domain::{Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus, SourceSpan};
use camino::Utf8Path;
use pulldown_cmark::{Event, Parser, Tag};

use super::target::{is_local_file_target, is_local_markdown_target, split_and_decode_destination};

/// Extract every local Markdown link and image embed from a document body.
///
/// `content` is the whole document and `body_start` the byte offset of `body`
/// within it, so the recorded [`SourceSpan`] is absolute in the file.
/// `[text](url)` links become [`LinkKind::Markdown`]; `![alt](url)` images
/// become [`LinkKind::Embed`]. External and same-note targets are skipped.
pub fn parse_markdown_links(
    source_path: &Utf8Path,
    content: &str,
    body: &str,
    body_start: usize,
) -> Vec<Link> {
    let parser = Parser::new(body).into_offset_iter();
    let mut links = Vec::new();

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                let raw = dest_url.to_string();
                if is_local_markdown_target(&raw) {
                    let (target, anchor, block_ref) = split_and_decode_destination(&raw);
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Markdown,
                        target,
                        label: None,
                        anchor,
                        block_ref,
                        source_span: Some(SourceSpan::at(content, body_start + range.start)),
                        source_context: Some(LinkSourceContext {
                            area: LinkSourceArea::Body,
                            property: None,
                        }),
                        resolved_path: None,
                        unresolved_reason: None,
                        candidates: Vec::new(),
                        status: LinkStatus::Unresolved,
                    });
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let raw = dest_url.to_string();
                if is_local_file_target(&raw) {
                    let (target, anchor, block_ref) = split_and_decode_destination(&raw);
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Embed,
                        target,
                        label: None,
                        anchor,
                        block_ref,
                        source_span: Some(SourceSpan::at(content, body_start + range.start)),
                        source_context: Some(LinkSourceContext {
                            area: LinkSourceArea::Body,
                            property: None,
                        }),
                        resolved_path: None,
                        unresolved_reason: None,
                        candidates: Vec::new(),
                        status: LinkStatus::Unresolved,
                    });
                }
            }
            _ => {}
        }
    }

    links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Link> {
        // A document with no frontmatter: body starts at offset 0.
        parse_markdown_links(Utf8Path::new("a.md"), body, body, 0)
    }

    #[test]
    fn extracts_local_markdown_link_with_anchor() {
        let links = parse("See [Delta](folder/delta.md#Delta-Heading).\n");
        assert_eq!(links.len(), 1);
        let link = &links[0];
        assert_eq!(link.kind, LinkKind::Markdown);
        assert_eq!(link.target, "folder/delta.md");
        assert_eq!(link.anchor.as_deref(), Some("Delta-Heading"));
        assert_eq!(link.status, LinkStatus::Unresolved);
    }

    #[test]
    fn extracts_image_embed_as_embed_kind() {
        let links = parse("![Picture](Assets/pic.png)\n");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Embed);
        assert_eq!(links[0].target, "Assets/pic.png");
    }

    #[test]
    fn skips_external_and_same_note_link_targets() {
        let links = parse("[ext](https://example.com) and [self](#Heading)\n");
        assert!(links.is_empty());
    }

    #[test]
    fn decodes_percent_escapes_in_target_and_anchor() {
        let links = parse("[with anchor](Markdown%20Target.md#Encoded%20Heading)\n");
        assert_eq!(links[0].target, "Markdown Target.md");
        assert_eq!(links[0].anchor.as_deref(), Some("Encoded Heading"));
    }

    #[test]
    fn encoded_hash_in_destination_stays_one_path_segment() {
        // NRN-356: split-then-decode. `%23` is not a literal `#`, so the whole
        // reference is one path segment naming a file called `note#draft.md` —
        // NOT `note` + anchor `draft.md` (which decode-then-split produced).
        let links = parse("[hashpath](note%23draft.md)\n");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "note#draft.md");
        assert_eq!(links[0].anchor, None);
        assert_eq!(links[0].block_ref, None);
    }

    #[test]
    fn block_ref_fragment_populates_block_ref_not_anchor() {
        // NRN-356: a `#^id` fragment on a Markdown destination classifies as a
        // block reference, exactly like the wikilink path.
        let links = parse("[blk](target.md#^blk1)\n");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "target.md");
        assert_eq!(links[0].block_ref.as_deref(), Some("blk1"));
        assert_eq!(links[0].anchor, None);
    }

    #[test]
    fn skips_scheme_bearing_and_protocol_relative_destinations() {
        // NRN-357: generic scheme classification excludes every external form —
        // including mixed-case schemes, `tel:`/`file:`, protocol-relative `//`,
        // and Windows drive letters — that the old lowercase prefix list let
        // through as (unresolved) local links.
        let links = parse(
            "[a](tel:+15551234) [b](file:///etc/hosts) [c](//example.com/x) \
             [d](C:/Users/x/n.md) [e](hTTp://example.com)\n",
        );
        assert!(links.is_empty());
    }

    #[test]
    fn image_embed_carries_block_ref() {
        // The split-then-decode path applies to image embeds too.
        let links = parse("![pic](target.md#^blk2)\n");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].kind, LinkKind::Embed);
        assert_eq!(links[0].block_ref.as_deref(), Some("blk2"));
    }

    #[test]
    fn markdown_links_skip_a_fence() {
        // Code opacity (ADR 0019): the pulldown-cmark event stream never emits a
        // `Tag::Link` for a `[text](url)` inside a fence, so a Markdown link in a
        // code sample is never extracted — the link half of the sweep.
        let body = "real [Delta](delta.md)\n\n```\n[fake md](fake.md)\n```\n";
        let links = parse(body);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "delta.md");
    }

    #[test]
    fn source_span_is_absolute_in_content() {
        // Body begins partway through the file; the recorded offset must be
        // content-absolute (body_start + in-body offset).
        let body = "[Delta](delta.md)\n";
        let links = parse_markdown_links(Utf8Path::new("a.md"), body, body, 100);
        assert_eq!(links[0].source_span.unwrap().byte_offset, 100);
    }
}
