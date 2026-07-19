//! Wikilink SYNTAX — token-level recognition of `[[…]]` references in text.
//!
//! This is the lexical layer only: it recognizes a wikilink and decomposes it
//! into target, alias, heading anchor, and block reference, plus whether it is an
//! embed (`![[…]]`). It also reads block-id definitions (`^block-id`).
//!
//! ## Resolution is out of scope (the deliberate seam)
//!
//! Matching a parsed `target` to a document in a vault — alias tables, path
//! resolution, ambiguity, resolved/unresolved status — is **resolution**, not
//! syntax. The donor entangled the two: `links::wikilink` produced a
//! `core::Link` already carrying `resolved_path`, `candidates`, `status`, and a
//! `source_context` naming the vault area. That resolution layer ports to
//! `norn-core` in a later task. Here a [`Wikilink`] is a pure syntactic token
//! with no vault knowledge; a resolver consumes these tokens later.

use std::ops::Range;
use std::sync::LazyLock;

use crate::span::SourceSpan;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use regex::Regex;

static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(!?)\[\[([^\]]+)\]\]").expect("valid wikilink regex"));

/// A recognized wikilink token, decomposed but unresolved.
///
/// For `![[Note#Heading|Alias]]`: `embed = true`, `target = "Note"`,
/// `anchor = Some("Heading")`, `alias = Some("Alias")`, `block_ref = None`.
/// For a block reference `[[Note#^blk]]`: `block_ref = Some("blk")`,
/// `anchor = None`. A same-note reference like `[[#Heading]]` has an empty
/// `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wikilink {
    /// The exact matched text, including delimiters and any leading `!`.
    pub raw: String,
    /// True for an embed (`![[…]]`).
    pub embed: bool,
    /// The link target (may be empty for a same-note anchor/block reference).
    pub target: String,
    /// The display alias after `|`, trimmed.
    pub alias: Option<String>,
    /// A heading anchor after `#` (mutually exclusive with `block_ref`).
    pub anchor: Option<String>,
    /// A block reference after `#^`.
    pub block_ref: Option<String>,
    /// Byte span of the whole match within the parsed text.
    pub span: SourceSpan,
}

/// Parse wikilinks in a Markdown `body`, skipping any inside inline code spans or
/// fenced code blocks (where `[[…]]` is literal, not a link). Spans are byte
/// offsets into `body`.
pub fn parse_wikilinks(body: &str) -> Vec<Wikilink> {
    let ignored = ignored_code_ranges(body);
    parse_tokens(body, &ignored)
}

/// Parse wikilinks in arbitrary `text` with no code-span exclusion — for text
/// that is not Markdown body, such as a frontmatter string value. Spans are byte
/// offsets into `text`.
pub fn parse_wikilinks_in_text(text: &str) -> Vec<Wikilink> {
    parse_tokens(text, &[])
}

fn parse_tokens(text: &str, ignored: &[Range<usize>]) -> Vec<Wikilink> {
    WIKILINK_RE
        .captures_iter(text)
        .filter_map(|captures| {
            let full_match = captures.get(0)?;
            let match_range = full_match.start()..full_match.end();
            if ignored.iter().any(|r| ranges_overlap(r, &match_range)) {
                return None;
            }

            let raw = full_match.as_str().to_string();
            let embed = captures.get(1).is_some_and(|m| m.as_str() == "!");
            let inner = captures.get(2)?.as_str();
            let (target_part, alias) = inner
                .split_once('|')
                .map_or((inner, None), |(target, alias)| {
                    (target, Some(alias.trim().to_string()))
                });
            let (target, anchor, block_ref) = split_anchor_or_block_ref(target_part.trim());

            Some(Wikilink {
                raw,
                embed,
                target,
                alias,
                anchor,
                block_ref,
                span: SourceSpan::at(text, full_match.start()),
            })
        })
        .collect()
}

/// Split a reference target on its first `#` into `(target, anchor)`. A leading
/// `#` (a same-note reference) yields an empty target.
pub fn split_anchor(raw: &str) -> (String, Option<String>) {
    match raw.split_once('#') {
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string())),
        None => (raw.to_string(), None),
    }
}

/// Split a reference target into `(target, anchor, block_ref)`. A `#^id` fragment
/// is a block reference; any other `#frag` is a heading anchor. Only the first
/// `#` splits, so extra hashes stay inside the anchor.
pub fn split_anchor_or_block_ref(raw: &str) -> (String, Option<String>, Option<String>) {
    match raw.split_once('#') {
        Some((target, reference)) if reference.starts_with('^') => {
            (target.to_string(), None, Some(reference[1..].to_string()))
        }
        Some((target, anchor)) => (target.to_string(), Some(anchor.to_string()), None),
        None => (raw.to_string(), None, None),
    }
}

static BLOCK_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)\^([A-Za-z0-9_-]+)\s*$").expect("valid block id regex"));

/// Read trailing block-id definitions (`… ^block-id`) from each line of `body`.
/// These are the anchor targets a `[[Note#^block-id]]` reference points at.
pub fn parse_block_ids(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| {
            BLOCK_ID_RE
                .captures(line)
                .and_then(|captures| captures.get(1))
                .map(|block_id| block_id.as_str().to_string())
        })
        .collect()
}

/// Byte ranges of inline code spans and fenced code blocks in `body`, where a
/// `[[…]]` sequence is literal text and must not be recognized as a wikilink.
fn ignored_code_ranges(body: &str) -> Vec<Range<usize>> {
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

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_wikilink() {
        let links = parse_wikilinks("see [[Target]] here\n");
        assert_eq!(links.len(), 1);
        let l = &links[0];
        assert_eq!(l.raw, "[[Target]]");
        assert!(!l.embed);
        assert_eq!(l.target, "Target");
        assert_eq!(l.alias, None);
        assert_eq!(l.anchor, None);
        assert_eq!(l.block_ref, None);
    }

    #[test]
    fn alias_is_split_and_trimmed() {
        let links = parse_wikilinks("[[Target| Display Name ]]\n");
        assert_eq!(links[0].target, "Target");
        assert_eq!(links[0].alias.as_deref(), Some("Display Name"));
    }

    #[test]
    fn heading_anchor_is_split() {
        let links = parse_wikilinks("[[Note#Heading]]\n");
        assert_eq!(links[0].target, "Note");
        assert_eq!(links[0].anchor.as_deref(), Some("Heading"));
        assert_eq!(links[0].block_ref, None);
    }

    #[test]
    fn block_ref_is_split() {
        let links = parse_wikilinks("[[Note#^block-id]]\n");
        assert_eq!(links[0].target, "Note");
        assert_eq!(links[0].anchor, None);
        assert_eq!(links[0].block_ref.as_deref(), Some("block-id"));
    }

    #[test]
    fn embed_marker_is_recognized() {
        let links = parse_wikilinks("![[Image.png]]\n");
        assert!(links[0].embed);
        assert_eq!(links[0].target, "Image.png");
        assert_eq!(links[0].raw, "![[Image.png]]");
    }

    #[test]
    fn same_note_anchor_has_empty_target() {
        let links = parse_wikilinks("[[#Heading]]\n");
        assert_eq!(links[0].target, "");
        assert_eq!(links[0].anchor.as_deref(), Some("Heading"));
    }

    #[test]
    fn alias_with_anchor_splits_both() {
        let links = parse_wikilinks("[[Note#Heading|Alias]]\n");
        assert_eq!(links[0].target, "Note");
        assert_eq!(links[0].anchor.as_deref(), Some("Heading"));
        assert_eq!(links[0].alias.as_deref(), Some("Alias"));
    }

    #[test]
    fn span_points_at_the_match() {
        let body = "abc [[Target]] def\n";
        let links = parse_wikilinks(body);
        let span = links[0].span;
        assert_eq!(&body[span.byte_offset..span.byte_offset + 10], "[[Target]]");
    }

    #[test]
    fn wikilinks_inside_inline_code_are_ignored() {
        let links = parse_wikilinks("before `[[ignored]]` after [[real]]\n");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "real");
    }

    #[test]
    fn wikilinks_inside_fenced_code_are_ignored() {
        let body = "outside [[real]]\n\n```\n[[in code]]\n```\n\nafter [[real2]]\n";
        let links = parse_wikilinks(body);
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["real", "real2"]);
    }

    #[test]
    fn parse_in_text_does_not_exclude_code() {
        // Raw text (e.g. a frontmatter value) has no Markdown code semantics.
        let links = parse_wikilinks_in_text("`[[still parsed]]`");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "still parsed");
    }

    #[test]
    fn multiple_links_on_one_line() {
        let links = parse_wikilinks("[[a]] and [[b]] and [[c]]\n");
        let targets: Vec<&str> = links.iter().map(|l| l.target.as_str()).collect();
        assert_eq!(targets, vec!["a", "b", "c"]);
    }

    #[test]
    fn split_anchor_separates_target_and_anchor() {
        assert_eq!(
            split_anchor("Note#Heading"),
            ("Note".into(), Some("Heading".into()))
        );
        assert_eq!(split_anchor("Note"), ("Note".into(), None));
        assert_eq!(
            split_anchor("#Heading"),
            ("".into(), Some("Heading".into()))
        );
    }

    #[test]
    fn split_anchor_or_block_ref_distinguishes_block_refs() {
        assert_eq!(
            split_anchor_or_block_ref("Note#^block-id"),
            ("Note".into(), None, Some("block-id".into()))
        );
        assert_eq!(
            split_anchor_or_block_ref("Note#Heading"),
            ("Note".into(), Some("Heading".into()), None)
        );
        assert_eq!(
            split_anchor_or_block_ref("Note"),
            ("Note".into(), None, None)
        );
    }

    #[test]
    fn split_anchor_keeps_extra_hashes_inside_anchor() {
        let (target, anchor, block) = split_anchor_or_block_ref("Note#Heading#With#Hashes");
        assert_eq!(target, "Note");
        assert_eq!(anchor, Some("Heading#With#Hashes".into()));
        assert_eq!(block, None);
    }

    #[test]
    fn parse_block_ids_finds_trailing_and_line_start_refs() {
        assert_eq!(
            parse_block_ids("Some paragraph. ^block-1\n"),
            vec!["block-1"]
        );
        assert_eq!(parse_block_ids("^block-2\n"), vec!["block-2"]);
        assert_eq!(
            parse_block_ids("first ^a\nsecond ^b\nthird\n"),
            vec!["a", "b"]
        );
    }

    #[test]
    fn parse_block_ids_rejects_unsupported_characters() {
        assert!(parse_block_ids("hello ^bad.id\n").is_empty());
    }

    #[test]
    fn parse_block_ids_allows_trailing_whitespace() {
        assert_eq!(parse_block_ids("hello ^ok  \n"), vec!["ok"]);
    }
}
