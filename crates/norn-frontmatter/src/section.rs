//! Heading-delimited sections — resolving an exact heading to the byte range it
//! owns.
//!
//! A section is addressed by exact heading text and runs from its heading line
//! to the next same-or-higher-level heading (or end of body). The three byte
//! offsets in [`SectionSpan`] let a caller read, replace, delete, or insert
//! around a section while touching nothing else.
//!
//! This is the single section resolver. A section READ (`get --section`) and a
//! section WRITE (`norn edit`'s body-transform ops in `norn-core`) both consume
//! it, so they agree on the span and on the missing/ambiguous failure modes;
//! the edit path adapts [`SectionError`] into its own indexed refusal shape at
//! the call site.

use crate::heading::{parse_headings, Heading};

/// Indices of the headings whose text matches `text` exactly.
fn matching_indices(headings: &[Heading], text: &str) -> Vec<usize> {
    headings
        .iter()
        .enumerate()
        .filter(|(_, h)| h.text == text)
        .map(|(i, _)| i)
        .collect()
}

/// The heading TEXT of an ATX-prefixed anchor, or `None` when the anchor is not
/// an ATX form (NRN-164). The anchor must begin with a run of 1–6 `#` followed
/// by at least one space or tab — CommonMark's ATX opening. The very same
/// `pulldown-cmark` pass that produced the document's headings then extracts the
/// anchor's text, so a trailing ATX closer (`## X ##` → `X`) and inline markup
/// are handled identically to a real heading: a `#` that is part of the text
/// (`## C#`) is preserved, never mistaken for a closer. Parsing the tiny anchor
/// only on the miss path keeps the exact-match hot path untouched.
fn atx_anchor_text(anchor: &str) -> Option<String> {
    let hashes = anchor.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    if !matches!(anchor.as_bytes().get(hashes), Some(b' ') | Some(b'\t')) {
        return None;
    }
    parse_headings(anchor).into_iter().next().map(|h| h.text)
}

/// Byte ranges describing a section addressed by exact heading text. Offsets are
/// relative to the `body` the span was resolved against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionSpan {
    /// Start of the heading line (the `#`).
    pub heading_start: usize,
    /// Start of the section body (just after the heading line's `\n`).
    pub body_start: usize,
    /// End of the section: start of the next same-or-higher-level heading, or EOF.
    pub end: usize,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SectionError {
    #[error("heading not found: {heading:?}")]
    HeadingNotFound { heading: String },
    #[error("{count} headings named {heading:?}; heading must be unambiguous")]
    HeadingAmbiguous { heading: String, count: usize },
}

/// Resolve a unique section by exact heading text against `body`. Fence-safe:
/// `pulldown-cmark` never emits a heading for a `#` inside a fenced code block.
pub fn resolve_section(body: &str, heading: &str) -> Result<SectionSpan, SectionError> {
    let headings = parse_headings(body);
    resolve_section_in(&headings, body, heading)
}

/// The resolution core, factored out so a caller resolving many headings against
/// one immutable body parses the heading list once and reuses it. Both paths
/// share the identical match / boundary / failure logic.
pub fn resolve_section_in(
    headings: &[Heading],
    body: &str,
    heading: &str,
) -> Result<SectionSpan, SectionError> {
    // Exact match FIRST — the anchor verbatim, exactly as before. This preserves
    // every currently-resolving anchor, including a heading whose text
    // legitimately begins with `#`.
    let mut matches = matching_indices(headings, heading);
    // Forgiving ATX fallback (NRN-164, ADR 0010): only on a TOTAL miss, and only
    // when the anchor is an ATX-prefixed form (`## State`, `### X ##`), retry on
    // the heading TEXT with the `#` prefix (and any trailing closer) stripped.
    // Heading level is treated as syntax noise — the retry matches on text, so a
    // `## X` anchor resolves a `### X` heading. Silent acceptance, per ADR 0010's
    // forgiving-inputs doctrine. Exact-first guarantees no working anchor flips.
    if matches.is_empty() {
        if let Some(text) = atx_anchor_text(heading) {
            matches = matching_indices(headings, &text);
        }
    }
    match matches.len() {
        0 => Err(SectionError::HeadingNotFound {
            heading: heading.to_string(),
        }),
        1 => {
            let i = matches[0];
            let level = headings[i].level;
            let heading_start = headings[i]
                .source_span
                .as_ref()
                .map(|s| s.byte_offset)
                .unwrap_or(0);
            // Body begins just past the whole heading construct (NRN-437): the
            // parser's heading range end, which covers a SETEXT underline and a
            // heading at EOF with no trailing newline. A manual "byte after the
            // first newline" scan would land on the underline of a SETEXT
            // heading and corrupt it. Freshly-parsed headings always carry
            // `body_offset`; the fallback is defensive (a cache heading is never
            // resolved) — an empty section running to EOF.
            let body_start = headings[i]
                .body_offset
                .unwrap_or(body.len())
                .min(body.len());
            // Section ends at the next heading whose level <= this one.
            let end = headings[i + 1..]
                .iter()
                .find(|h| h.level <= level)
                .and_then(|h| h.source_span.as_ref())
                .map(|s| s.byte_offset)
                .unwrap_or(body.len());
            Ok(SectionSpan {
                heading_start,
                body_start,
                end,
            })
        }
        n => Err(SectionError::HeadingAmbiguous {
            heading: heading.to_string(),
            count: n,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "intro\n\n## Alpha\na1\na2\n\n## Beta\nb1\n";

    #[test]
    fn section_body_and_end_bracket_the_content() {
        let span = resolve_section(DOC, "Alpha").unwrap();
        assert_eq!(&DOC[span.heading_start..span.body_start], "## Alpha\n");
        assert_eq!(&DOC[span.body_start..span.end], "a1\na2\n\n");
    }

    #[test]
    fn last_section_runs_to_eof() {
        let span = resolve_section(DOC, "Beta").unwrap();
        assert_eq!(&DOC[span.body_start..span.end], "b1\n");
        assert_eq!(span.end, DOC.len());
    }

    #[test]
    fn section_ends_at_next_same_or_higher_level_heading() {
        // A deeper subsection is part of its parent's range.
        let doc = "## Parent\np\n### Child\nc\n## Sibling\ns\n";
        let span = resolve_section(doc, "Parent").unwrap();
        assert_eq!(&doc[span.body_start..span.end], "p\n### Child\nc\n");
    }

    #[test]
    fn missing_heading_refuses() {
        let err = resolve_section(DOC, "Gamma").unwrap_err();
        assert_eq!(
            err,
            SectionError::HeadingNotFound {
                heading: "Gamma".into()
            }
        );
    }

    #[test]
    fn duplicate_heading_is_ambiguous() {
        let doc = "## Dup\nx\n## Dup\ny\n";
        let err = resolve_section(doc, "Dup").unwrap_err();
        assert_eq!(
            err,
            SectionError::HeadingAmbiguous {
                heading: "Dup".into(),
                count: 2
            }
        );
    }

    #[test]
    fn heading_inside_fence_does_not_resolve() {
        let doc = "## Real\n```\n## Fake\n```\nbody\n";
        assert!(matches!(
            resolve_section(doc, "Fake"),
            Err(SectionError::HeadingNotFound { .. })
        ));
        // "Real" owns the whole doc including the fence.
        let span = resolve_section(doc, "Real").unwrap();
        assert_eq!(span.end, doc.len());
    }

    #[test]
    fn resolve_section_in_reuses_a_parsed_heading_list() {
        let headings = parse_headings(DOC);
        let a = resolve_section_in(&headings, DOC, "Alpha").unwrap();
        let b = resolve_section_in(&headings, DOC, "Beta").unwrap();
        assert_eq!(a, resolve_section(DOC, "Alpha").unwrap());
        assert_eq!(b, resolve_section(DOC, "Beta").unwrap());
    }

    // NRN-437: for a SETEXT heading, `body_start` must land AFTER the underline
    // line — the underline is part of the heading, not the body. A read of the
    // body (`body_start..end`) must therefore never surface the underline, and a
    // read of the whole section (`heading_start..end`, the `get --section` shape)
    // returns the heading, its underline, and the body intact.
    #[test]
    fn setext_body_start_is_after_the_underline() {
        let doc = "Alpha\n-----\nbody under alpha.\n\n## Beta\nb\n";
        let span = resolve_section(doc, "Alpha").unwrap();
        // heading region is the title line PLUS the underline line.
        assert_eq!(&doc[span.heading_start..span.body_start], "Alpha\n-----\n");
        // the body carries no underline.
        assert_eq!(&doc[span.body_start..span.end], "body under alpha.\n\n");
        // the whole-section read (get --section) is lossless.
        assert_eq!(
            &doc[span.heading_start..span.end],
            "Alpha\n-----\nbody under alpha.\n\n"
        );
    }

    #[test]
    fn setext_underline_only_section_has_empty_body() {
        // Two same-level (H1) setext headings back to back: `Alpha` owns nothing
        // between its underline and the next same-level heading.
        let doc = "Alpha\n=====\nBeta\n=====\nb\n";
        let span = resolve_section(doc, "Alpha").unwrap();
        assert_eq!(&doc[span.heading_start..span.body_start], "Alpha\n=====\n");
        assert_eq!(&doc[span.body_start..span.end], "");
    }

    // NRN-437: a heading at EOF with no trailing newline resolves to an empty
    // body running to EOF — the read never refuses valid CommonMark.
    #[test]
    fn heading_at_eof_without_trailing_newline_reads_cleanly() {
        let doc = "intro\n\n## Tail";
        let span = resolve_section(doc, "Tail").unwrap();
        assert_eq!(span.body_start, doc.len());
        assert_eq!(span.end, doc.len());
        assert_eq!(&doc[span.heading_start..span.end], "## Tail");
    }

    // ── NRN-164: forgiving ATX-prefixed anchors ──────────────────────────────
    // The section resolver tries the anchor EXACTLY first, then — only on a total
    // miss — retries an ATX-prefixed anchor (`## X`) on the heading TEXT.

    #[test]
    fn atx_prefixed_anchor_resolves_by_stripping_the_prefix() {
        // `## Alpha` (the natural markdown form) resolves the `## Alpha` heading.
        let span = resolve_section(DOC, "## Alpha").unwrap();
        assert_eq!(span, resolve_section(DOC, "Alpha").unwrap());
    }

    #[test]
    fn atx_anchor_level_is_syntax_noise_text_matches() {
        // A `## X` anchor resolves a `### X` heading — level differs, text matches.
        let doc = "### Deep\nbody\n";
        let span = resolve_section(doc, "## Deep").unwrap();
        assert_eq!(span, resolve_section(doc, "Deep").unwrap());
        // …and the reverse: a `#### X` anchor onto an `## X` heading.
        let span2 = resolve_section(DOC, "#### Alpha").unwrap();
        assert_eq!(span2, resolve_section(DOC, "Alpha").unwrap());
    }

    #[test]
    fn atx_anchor_trailing_closer_is_stripped() {
        // A closed ATX anchor (`## Alpha ##`) resolves the same section.
        let span = resolve_section(DOC, "## Alpha ##").unwrap();
        assert_eq!(span, resolve_section(DOC, "Alpha").unwrap());
    }

    #[test]
    fn exact_match_wins_over_forgiving_strip_ambiguous_doc() {
        // The ambiguity guard: a doc with BOTH a heading whose text is literally
        // `## Verbatim` (an ATX heading whose text begins with `#`) AND a
        // different heading `Verbatim`. The anchor `## Verbatim` must resolve the
        // FIRST (exact text match), never be stripped to `Verbatim` and land on
        // the wrong section. Exact-first is what prevents the mis-resolution.
        let doc = "#### ## Verbatim\nright body\n\n## Verbatim\nwrong body\n";
        let span = resolve_section(doc, "## Verbatim").unwrap();
        assert_eq!(&doc[span.body_start..span.end], "right body\n\n");
        // Sanity: the bare text still resolves the OTHER (level-2) section.
        let other = resolve_section(doc, "Verbatim").unwrap();
        assert_eq!(&doc[other.body_start..other.end], "wrong body\n");
    }

    #[test]
    fn non_atx_hash_anchor_is_not_stripped() {
        // `#Alpha` (no space after the hashes) is NOT an ATX opening, so it is
        // never stripped — it stays a total miss.
        let err = resolve_section(DOC, "#Alpha").unwrap_err();
        assert_eq!(
            err,
            SectionError::HeadingNotFound {
                heading: "#Alpha".into()
            }
        );
    }

    #[test]
    fn missing_heading_still_refuses_after_forgiving_retry() {
        // Neither the exact anchor nor its stripped text matches: the error is
        // unchanged and reports the anchor AS PASSED.
        let err = resolve_section(DOC, "## Gamma").unwrap_err();
        assert_eq!(
            err,
            SectionError::HeadingNotFound {
                heading: "## Gamma".into()
            }
        );
    }
}
