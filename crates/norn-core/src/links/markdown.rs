//! Markdown-link extraction — the link-producing half of the donor's
//! `parse_commonmark` pass.
//!
//! The donor walked the `pulldown-cmark` event stream once to collect both
//! headings and `[text](url)` / `![alt](url)` links. Heading (and slug) parsing
//! is text-layer syntax and now lives in `norn_frontmatter::heading`; this module
//! keeps only the link half, which is part of the link *model* — it produces
//! resolution-bearing [`Link`] records that later match to documents.

use crate::domain::{Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus, SourceSpan};
use camino::Utf8Path;
use pulldown_cmark::{Event, Parser, Tag};

use super::target::{decode_percent_escapes, is_local_file_target, is_local_markdown_target};

/// Split a target on its first `#` into `(target, anchor)`; a plain target has no
/// anchor. Mirrors `norn_frontmatter::wikilink::split_anchor` on owned strings.
fn split_anchor(raw: &str) -> (String, Option<String>) {
    match raw.split_once('#') {
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string())),
        None => (raw.to_string(), None),
    }
}

/// Extract every local Markdown link and image embed from a document body.
///
/// `content` is the whole document and `body_start` the byte offset of `body`
/// within it, so the recorded [`SourceSpan`] is absolute in the file (matching the
/// donor). `[text](url)` links become [`LinkKind::Markdown`]; `![alt](url)` images
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
                    let (target, anchor) = split_anchor(&decode_percent_escapes(&raw));
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Markdown,
                        target,
                        label: None,
                        anchor,
                        block_ref: None,
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
                    let (target, anchor) = split_anchor(&decode_percent_escapes(&raw));
                    links.push(Link {
                        source_path: source_path.to_path_buf(),
                        raw,
                        kind: LinkKind::Embed,
                        target,
                        label: None,
                        anchor,
                        block_ref: None,
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
