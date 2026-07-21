//! Markdown heading parsing — the text-layer half of the donor's commonmark
//! pass.
//!
//! Headings are extracted with `pulldown-cmark`, so a `#` inside a fenced or
//! inline code span is never mistaken for a heading. Each heading carries its
//! level, the concatenated text, a GitHub-ish ASCII `slug` (the anchor form),
//! and the byte span of its `#` marker.
//!
//! ## Seam left behind
//!
//! The donor's `parse_commonmark` walked the same `pulldown-cmark` event stream
//! to also collect Markdown `[text](url)` body links into resolvable `Link`
//! records. That link extraction is part of the link model — it produces
//! resolution-bearing types and pairs targets to documents — so it ports to
//! `norn-core` with link resolution, not here. This module keeps only the
//! heading (and slug) text layer.

use crate::span::SourceSpan;
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

/// A Markdown heading located in a document body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_span: Option<SourceSpan>,
    /// Byte offset just past the whole heading construct — where the section
    /// body begins. For an ATX heading this is the byte after its line's `\n`;
    /// for a SETEXT heading it is the byte after the underline's `\n` (the
    /// underline is part of the heading, not the body). Taken from the parser's
    /// heading source range end, so it is correct for both forms and for a
    /// heading at EOF with no trailing newline (NRN-437). `None` on a heading
    /// reconstructed from the cache (spans are a parse-time artifact the cache
    /// does not persist); section resolution always re-parses the live body, so
    /// it never observes the `None` case.
    #[serde(skip)]
    pub body_offset: Option<usize>,
}

/// Parse every heading in `body`. Spans are byte offsets into `body`.
pub fn parse_headings(body: &str) -> Vec<Heading> {
    let parser = Parser::new(body).into_offset_iter();
    let mut headings = Vec::new();
    let mut active_heading: Option<(u8, String, usize)> = None;

    for (event, range) in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                active_heading = Some((heading_level(level), String::new(), range.start));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text, start)) = active_heading.take() {
                    let text = text.trim().to_string();
                    headings.push(Heading {
                        level,
                        slug: slugify(&text),
                        text,
                        source_span: Some(SourceSpan::at(body, start)),
                        // `range.end` covers the whole heading construct: the ATX
                        // line (through its `\n`) or the SETEXT underline line.
                        body_offset: Some(range.end),
                    });
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, heading_text, _)) = active_heading.as_mut() {
                    heading_text.push_str(&text);
                }
            }
            _ => {}
        }
    }

    headings
}

/// Slugify heading text into an anchor: lowercase, ASCII-alphanumerics kept,
/// every other run collapsed to a single `-`, with leading/trailing dashes
/// trimmed. ASCII-only by design (a documented divergence from GitHub slugs).
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;

    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }

    slug.trim_end_matches('-').to_string()
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels_and_text() {
        let body = "# Title\n\n## Section\ntext\n### Sub\n";
        let headings = parse_headings(body);
        assert_eq!(headings.len(), 3);
        assert_eq!((headings[0].level, headings[0].text.as_str()), (1, "Title"));
        assert_eq!(
            (headings[1].level, headings[1].text.as_str()),
            (2, "Section")
        );
        assert_eq!((headings[2].level, headings[2].text.as_str()), (3, "Sub"));
    }

    #[test]
    fn heading_span_points_at_the_marker() {
        let body = "intro\n\n## Alpha\nbody\n";
        let headings = parse_headings(body);
        let span = headings[0].source_span.unwrap();
        assert!(body[span.byte_offset..].starts_with("## Alpha"));
    }

    #[test]
    fn hash_inside_fenced_code_is_not_a_heading() {
        let body = "## Real\n```\n## Fake\n```\n";
        let headings = parse_headings(body);
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].text, "Real");
    }

    #[test]
    fn inline_code_in_heading_is_part_of_text() {
        let body = "## Use `norn`\n";
        let headings = parse_headings(body);
        assert_eq!(headings[0].text, "Use norn");
    }

    #[test]
    fn slugify_lowercases_and_dasherizes() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("HELLO   WORLD"), "hello-world");
    }

    #[test]
    fn slugify_strips_non_ascii_alphanumeric() {
        assert_eq!(slugify("Café"), "caf");
        assert_eq!(slugify("日本語"), "");
    }

    #[test]
    fn slugify_trims_trailing_dashes_and_collapses_internal_runs() {
        assert_eq!(slugify("hello!!! world!!!"), "hello-world");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn slugify_preserves_digits() {
        assert_eq!(slugify("Heading 1.2.3"), "heading-1-2-3");
    }

    #[test]
    fn heading_slug_is_populated() {
        let headings = parse_headings("## Hello World\n");
        assert_eq!(headings[0].slug, "hello-world");
    }
}
