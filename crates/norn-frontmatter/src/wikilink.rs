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

/// Whether `target` can be represented as a wikilink target — i.e. reconstructing
/// a link with it and re-parsing yields the SAME target. The delimiter bytes a
/// target must not contain: `|` (begins the alias), `#` (begins the anchor /
/// block ref), and `[` / `]` (the `[[ ]]` fences). A target carrying any of them
/// would silently re-parse as a different link shape — e.g. `a|b` becomes target
/// `a` with alias `b` — so a rewrite to such a name must be refused, not emitted
/// (a rename to a filename with these bytes is where this bites). Newlines and
/// other bytes round-trip and are permitted.
pub fn wikilink_target_is_representable(target: &str) -> bool {
    !target.contains(['|', '#', '[', ']'])
}

/// Reconstruct a wikilink's text with `new_target` in place of its target,
/// preserving the embed marker (`!`), the heading anchor / block-reference
/// suffix, and the display alias. The inverse of [`parse_wikilinks`]'s
/// decomposition: given a parsed [`Wikilink`] plus a replacement target, emit
/// canonical `[[…]]` text. The alias and anchor are re-emitted from their parsed
/// (trimmed) forms, and a block reference re-emits as `#^id` — the authoritative
/// round-trip, so a rewriter can never drop the `!` or the `|alias` the way a
/// `strip_prefix("[[")` hand-roll does (NRN-431), and the `#`/`^` split follows
/// the parser (a bare `^` stays in the target — NRN-433).
///
/// Returns `None` when `new_target` is not representable (see
/// [`wikilink_target_is_representable`]): emitting it would corrupt the link into
/// a different shape, so the caller must refuse/skip the rewrite instead.
pub fn reconstruct_wikilink(link: &Wikilink, new_target: &str) -> Option<String> {
    if !wikilink_target_is_representable(new_target) {
        return None;
    }
    let mut out = String::new();
    if link.embed {
        out.push('!');
    }
    out.push_str("[[");
    out.push_str(new_target);
    if let Some(block_ref) = &link.block_ref {
        out.push_str("#^");
        out.push_str(block_ref);
    } else if let Some(anchor) = &link.anchor {
        out.push('#');
        out.push_str(anchor);
    }
    if let Some(alias) = &link.alias {
        out.push('|');
        out.push_str(alias);
    }
    out.push_str("]]");
    Some(out)
}

/// The single span-based wikilink rewriter (NRN-424/NRN-412). Rewrite selected
/// `[[…]]` references in `text` by splicing a replacement at each parsed link's
/// exact byte span. `parse` chooses code-awareness — [`parse_wikilinks`] for a
/// Markdown body (fenced / inline-code links are opaque per ADR 0019, so they
/// are never touched — NRN-432) or [`parse_wikilinks_in_text`] for a raw
/// frontmatter value. `replace(link)` returns `Some(text)` to substitute for
/// `link` verbatim at its span, or `None` to leave it untouched; it is `FnMut`
/// so a caller can rewrite only the first matching link. Because every
/// substitution lands on a parser-recognized span, the embed marker, alias, and
/// anchor survive by construction and code-fenced samples are structurally
/// excluded — the three ways the ad-hoc rewriters diverged from the parser.
pub fn splice_wikilinks(
    text: &str,
    parse: impl Fn(&str) -> Vec<Wikilink>,
    mut replace: impl FnMut(&Wikilink) -> Option<String>,
) -> String {
    let links = parse(text);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for link in &links {
        let start = link.span.byte_offset;
        let Some(replacement) = replace(link) else {
            continue;
        };
        // captures_iter yields non-overlapping matches left-to-right, so spans
        // are ascending and the cursor advances monotonically.
        out.push_str(&text[cursor..start]);
        out.push_str(&replacement);
        cursor = start + link.raw.len();
    }
    out.push_str(&text[cursor..]);
    out
}

static BLOCK_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|\s)\^([A-Za-z0-9_-]+)\s*$").expect("valid block id regex"));

/// Read trailing block-id definitions (`… ^block-id`) from each line of `body`.
/// These are the anchor targets a `[[Note#^block-id]]` reference points at.
///
/// Fenced code blocks and inline code spans are opaque (ADR 0019): a `^id`
/// inside them is literal sample text, never an anchor, so any match whose bytes
/// fall in a code range is dropped — the same exclusion [`parse_wikilinks`]
/// applies. A `^id` on the line *after* a closing fence lies outside the fence's
/// range and stays a valid anchor (the Obsidian-aligned nuance: it references
/// the code block itself).
pub fn parse_block_ids(body: &str) -> Vec<String> {
    let ignored = ignored_code_ranges(body);
    let mut block_ids = Vec::new();
    let mut line_start = 0;
    for line in body.split_inclusive('\n') {
        if let Some(block_id) = BLOCK_ID_RE.captures(line).and_then(|c| c.get(1)) {
            let id_range = (line_start + block_id.start())..(line_start + block_id.end());
            if !ignored.iter().any(|r| ranges_overlap(r, &id_range)) {
                block_ids.push(block_id.as_str().to_string());
            }
        }
        line_start += line.len();
    }
    block_ids
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

    // ---- Reconstruction + span-based rewrite (NRN-424).

    fn only(text: &str) -> Wikilink {
        parse_wikilinks_in_text(text)
            .into_iter()
            .next()
            .expect("one link")
    }

    #[test]
    fn reconstruct_is_identity_for_tight_links() {
        // Emission fidelity: for a TIGHT link (no interior whitespace),
        // reconstructing with its own target round-trips to the exact raw across
        // every embed / alias / anchor / block-ref combination. (Padded links do
        // NOT round-trip — that canonicalization is the decided PD-119 behavior.)
        for raw in [
            "[[Target]]",
            "![[Target]]",
            "[[Target|Alias]]",
            "![[Target|Alias]]",
            "[[Target#Heading]]",
            "![[Target#Heading]]",
            "[[Target#Heading|Alias]]",
            "[[Target#^blk]]",
            "![[Target#^blk|Alias]]",
        ] {
            let link = only(raw);
            assert_eq!(
                reconstruct_wikilink(&link, &link.target).as_deref(),
                Some(raw),
                "tight link must round-trip: {raw}"
            );
        }
    }

    #[test]
    fn reconstruct_preserves_embed_and_alias() {
        // NRN-431: an embed's `!` and its `|alias` survive a target rewrite.
        assert_eq!(
            reconstruct_wikilink(&only("![[old|Display]]"), "new").as_deref(),
            Some("![[new|Display]]")
        );
    }

    #[test]
    fn reconstruct_preserves_anchor_and_block_ref() {
        assert_eq!(
            reconstruct_wikilink(&only("[[old#Heading]]"), "new").as_deref(),
            Some("[[new#Heading]]")
        );
        assert_eq!(
            reconstruct_wikilink(&only("[[old#^blk]]"), "new").as_deref(),
            Some("[[new#^blk]]")
        );
    }

    #[test]
    fn reconstruct_keeps_caret_in_target() {
        // NRN-433: a bare `^` is an ordinary target char, not a block sigil, so
        // it round-trips inside the target rather than being split off.
        let link = only("[[a^b]]");
        assert_eq!(link.target, "a^b");
        assert_eq!(
            reconstruct_wikilink(&link, "renamed").as_deref(),
            Some("[[renamed]]")
        );
    }

    #[test]
    fn reconstruct_refuses_unrepresentable_targets() {
        // A `new_target` carrying a wikilink delimiter would re-parse as a
        // different link shape (`a|b` → target `a`, alias `b`), so reconstruction
        // refuses it rather than silently corrupt the backlink.
        let link = only("[[old]]");
        for bad in ["a|b", "a#b", "a]]b", "a[[b", "a]b", "a[b"] {
            assert!(
                reconstruct_wikilink(&link, bad).is_none(),
                "unrepresentable target must be refused: {bad}"
            );
            assert!(!wikilink_target_is_representable(bad));
        }
        // A caret-bearing name is representable (a bare `^` is an ordinary char).
        assert!(wikilink_target_is_representable("a^b"));
        assert!(reconstruct_wikilink(&link, "a^b").is_some());
    }

    #[test]
    fn splice_rewrites_only_matching_targets() {
        let body = "[[keep]] and [[old]] and [[old|Alias]]\n";
        let out = splice_wikilinks(body, parse_wikilinks, |link| {
            (link.target == "old")
                .then(|| reconstruct_wikilink(link, "new"))
                .flatten()
        });
        assert_eq!(out, "[[keep]] and [[new]] and [[new|Alias]]\n");
    }

    #[test]
    fn splice_body_is_opaque_to_code_fences() {
        // NRN-432: a fenced `[[old]]` is literal sample text and must not be
        // rewritten, even though it is the first file occurrence.
        let body = "```\n[[old]]\n```\n\nprose [[old]]\n";
        let out = splice_wikilinks(body, parse_wikilinks, |link| {
            (link.target == "old")
                .then(|| reconstruct_wikilink(link, "new"))
                .flatten()
        });
        assert_eq!(out, "```\n[[old]]\n```\n\nprose [[new]]\n");
    }

    #[test]
    fn splice_can_rewrite_first_match_only() {
        let body = "[[old]] then [[old]]\n";
        let mut done = false;
        let out = splice_wikilinks(body, parse_wikilinks, |link| {
            if !done && link.target == "old" {
                done = true;
                reconstruct_wikilink(link, "new")
            } else {
                None
            }
        });
        assert_eq!(out, "[[new]] then [[old]]\n");
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

    // ---- Code opacity (ADR 0019): a `^id` inside code is not an anchor.

    #[test]
    fn block_id_inside_fenced_code_is_not_an_anchor() {
        let body = "real ^outside\n\n```\n^incode\n```\n";
        assert_eq!(parse_block_ids(body), vec!["outside"]);
    }

    #[test]
    fn block_id_inside_inline_code_is_not_an_anchor() {
        // A bare `^id` line that is entirely an inline code span is literal text.
        // pulldown-cmark reports the span (backticks included) as a Code range;
        // the id's bytes fall inside it, so the exclusion drops it.
        let body = "prose\n`^incode`\n";
        assert!(parse_block_ids(body).is_empty());
    }

    #[test]
    fn block_id_on_line_after_a_fence_is_still_an_anchor() {
        // The Obsidian-aligned nuance: a `^id` on the line AFTER a closing fence
        // references the code block itself and remains valid — it lies outside
        // the fence's byte range.
        let body = "```\ncode line\n```\n^after-fence\n";
        assert_eq!(parse_block_ids(body), vec!["after-fence"]);
    }

    // ---- Sweep (ADR 0019): every body parser is opaque to a single fence.
    // A fixture with a fake heading, a wikilink, a Markdown link, and a block-id
    // all inside one fence — none of the text-layer parsers may extract from it.

    const FENCED_ZOO: &str = "\
outside [[real-link]]

```
# Fake Heading
[[fake-wikilink]]
[fake md](fake.md)
^fake-block-id
```
";

    #[test]
    fn sweep_wikilinks_skip_a_fence() {
        let targets: Vec<String> = parse_wikilinks(FENCED_ZOO)
            .into_iter()
            .map(|w| w.target)
            .collect();
        assert_eq!(targets, vec!["real-link"]);
    }

    #[test]
    fn sweep_block_ids_skip_a_fence() {
        assert!(parse_block_ids(FENCED_ZOO).is_empty());
    }

    #[test]
    fn sweep_headings_skip_a_fence() {
        // The heading parser is pulldown-cmark-based; a `#` inside a fence never
        // emits a heading event.
        assert!(crate::heading::parse_headings(FENCED_ZOO).is_empty());
    }
}
