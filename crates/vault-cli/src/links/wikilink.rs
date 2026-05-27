use std::ops::Range;
use std::sync::LazyLock;

use camino::Utf8Path;
use regex::Regex;
use serde_json::Value;
use vault_core::{Link, LinkKind, LinkSourceArea, LinkSourceContext, LinkStatus};
use vault_frontmatter::frontmatter_property_strings;

use super::anchor::{source_span, split_anchor_or_block_ref};
use super::commonmark::ignored_wikilink_ranges;

static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[\[([^\]]+)\]\]").expect("valid wikilink regex"));

pub fn parse_wikilinks(
    source_path: &Utf8Path,
    content: &str,
    body: &str,
    body_start: usize,
) -> Vec<Link> {
    let ignored_ranges = ignored_wikilink_ranges(body);
    parse_wikilinks_in_text(
        source_path,
        body,
        Some((content, body_start, ignored_ranges)),
        Some(LinkSourceContext {
            area: LinkSourceArea::Body,
            property: None,
        }),
    )
}

fn parse_wikilinks_in_text(
    source_path: &Utf8Path,
    text: &str,
    source: Option<(&str, usize, Vec<Range<usize>>)>,
    source_context: Option<LinkSourceContext>,
) -> Vec<Link> {
    let wikilink_re = &*WIKILINK_RE;
    wikilink_re
        .captures_iter(text)
        .filter_map(|captures| {
            let full_match = captures.get(0)?;
            if source.as_ref().is_some_and(|(_, _, ignored_ranges)| {
                ignored_ranges
                    .iter()
                    .any(|range| ranges_overlap(range, &(full_match.start()..full_match.end())))
            }) {
                return None;
            }

            let source_span = source.as_ref().map(|(content, base_offset, _)| {
                source_span(content, base_offset + full_match.start())
            });

            let raw = full_match.as_str().to_string();
            let is_embed = captures.get(1).is_some_and(|m| m.as_str() == "!");
            let inner = captures.get(2)?.as_str();
            let (target_part, label) = inner
                .split_once('|')
                .map_or((inner, None), |(target, label)| {
                    (target, Some(label.trim().to_string()))
                });
            let (target, anchor, block_ref) = split_anchor_or_block_ref(target_part.trim());

            Some(Link {
                source_path: source_path.to_path_buf(),
                raw,
                kind: if is_embed {
                    LinkKind::Embed
                } else {
                    LinkKind::Wikilink
                },
                target,
                label,
                anchor,
                block_ref,
                source_span,
                source_context: source_context.clone(),
                resolved_path: None,
                unresolved_reason: None,
                candidates: Vec::new(),
                status: LinkStatus::Unresolved,
            })
        })
        .collect()
}

pub fn parse_frontmatter_wikilinks(
    source_path: &Utf8Path,
    content: &str,
    frontmatter_range: Option<Range<usize>>,
    frontmatter: &Value,
) -> Vec<Link> {
    let Some(object) = frontmatter.as_object() else {
        return Vec::new();
    };

    frontmatter_property_strings(object, content, frontmatter_range)
        .into_iter()
        .flat_map(|property_string| {
            parse_wikilinks_in_text(
                source_path,
                property_string.text,
                property_string
                    .offset
                    .map(|offset| (content, offset, Vec::new())),
                Some(LinkSourceContext {
                    area: LinkSourceArea::Frontmatter,
                    property: Some(property_string.property),
                }),
            )
        })
        .collect()
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}
