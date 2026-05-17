use std::ops::Range;

use camino::Utf8Path;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use vault_core::{Heading, Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus};

use crate::anchor::{
    decode_percent_escapes, heading_level, is_local_file_target, is_local_markdown_target, slugify,
    source_span, split_anchor,
};

pub fn parse_commonmark(
    source_path: &Utf8Path,
    content: &str,
    body: &str,
    body_start: usize,
) -> (Vec<Heading>, Vec<Link>) {
    let parser = Parser::new(body).into_offset_iter();
    let mut headings = Vec::new();
    let mut links = Vec::new();
    let mut active_heading: Option<(u8, String, usize)> = None;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active_heading = Some((
                    heading_level(level),
                    String::new(),
                    body_start + range.start,
                ));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text, start)) = active_heading.take() {
                    let text = text.trim().to_string();
                    headings.push(Heading {
                        level,
                        slug: slugify(&text),
                        text,
                        source_span: Some(source_span(content, start)),
                    });
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, heading_text, _)) = active_heading.as_mut() {
                    heading_text.push_str(&text);
                }
            }
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
                        source_span: Some(source_span(content, body_start + range.start)),
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
                        source_span: Some(source_span(content, body_start + range.start)),
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

    (headings, links)
}

pub(crate) fn ignored_wikilink_ranges(body: &str) -> Vec<Range<usize>> {
    let parser = Parser::new(body).into_offset_iter();
    let mut ignored_ranges = Vec::new();
    let mut active_code_block_start = None;

    for (event, range) in parser {
        match event {
            Event::Code(_) => ignored_ranges.push(range),
            Event::Start(Tag::CodeBlock(_)) => active_code_block_start = Some(range.start),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(start) = active_code_block_start.take() {
                    ignored_ranges.push(start..range.end);
                }
            }
            _ => {}
        }
    }

    ignored_ranges
}
