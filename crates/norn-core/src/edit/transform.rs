//! The pure body-transform for section edits: `(old_body, [EditOp]) -> new_body`.
//! Ops apply sequentially, each against the result of the prior. Any anchor
//! failure aborts the whole batch (atomic refuse) via `EditError`. This is the
//! novel core the executor's compose path delegates to; everything downstream
//! (lock, audit, report) is reused from the frontmatter verbs.
//!
//! Section anchors resolve through the single [`norn_frontmatter::section`]
//! resolver — the same primitive `get --section` reads through — so a section
//! read and a section write agree on the span and the missing/ambiguous failure
//! modes. Its [`SectionError`] is adapted here into the indexed [`EditError`]
//! refusal shape (carrying the op index and kind). Resolution is fence-safe, so
//! `#` inside a fenced code block never resolves as a heading.

use crate::edit::ops::EditOp;
use norn_frontmatter::section::{
    resolve_section as resolve_section_span, SectionError, SectionSpan,
};

/// Per-op descriptor for the success report `edits` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditDescriptor {
    pub op: String,
    pub anchor_desc: String,
    /// Match count for `str_replace`; `None` for structural ops.
    pub occurrences: Option<usize>,
}

#[derive(Debug)]
pub struct EditTransform {
    pub new_body: String,
    pub descriptors: Vec<EditDescriptor>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("edit {index} ({kind}): empty anchor is not allowed")]
    InvalidOp { index: usize, kind: &'static str },
    #[error("edit {index} ({kind}): string not found: {anchor}")]
    StrNotFound {
        index: usize,
        kind: &'static str,
        anchor: String,
    },
    #[error("edit {index} ({kind}): string matched {count} times, expected exactly 1 (set replace_all to replace every occurrence): {anchor}")]
    StrAmbiguous {
        index: usize,
        kind: &'static str,
        anchor: String,
        count: usize,
    },
    #[error("edit {index} ({kind}): heading not found: {heading:?}")]
    HeadingNotFound {
        index: usize,
        kind: &'static str,
        heading: String,
    },
    #[error(
        "edit {index} ({kind}): {count} headings named {heading:?}; heading must be unambiguous"
    )]
    HeadingAmbiguous {
        index: usize,
        kind: &'static str,
        heading: String,
        count: usize,
    },
    /// Opt-in compare-and-swap precondition failed: the document's current
    /// content hash does not match the caller-supplied `expected_hash`. Not
    /// per-op / indexed — it guards the whole edit before any transform runs.
    /// Carries `path` so the message names the drifted document (CLI parity) and
    /// the structured MCP refusal envelope gets a resolved `error.path`.
    #[error(
        "document {path} has drifted from the expected hash: expected {expected}, found {actual}"
    )]
    ContentDrift {
        path: String,
        expected: String,
        actual: String,
    },
}

impl EditError {
    /// The stable, machine-branchable kebab code for this refusal (NRN-220), so
    /// a consumer branches on the code — `anchor-not-found`, `anchor-ambiguous`,
    /// … — rather than string-matching the prose message. Mirrors the `.code()`
    /// convention of `standards::apply::ApplyError`.
    pub fn code(&self) -> &'static str {
        match self {
            EditError::InvalidOp { .. } => "empty-anchor",
            EditError::StrNotFound { .. } => "anchor-not-found",
            EditError::StrAmbiguous { .. } => "anchor-ambiguous",
            EditError::HeadingNotFound { .. } => "heading-not-found",
            EditError::HeadingAmbiguous { .. } => "heading-ambiguous",
            // Vocabulary-aligned with the applier's index-snapshot CAS
            // (`standards::apply::ApplyError::StaleDocumentHash`) so a consumer's
            // retry-on-drift branch is unified across both drift checks.
            EditError::ContentDrift { .. } => "stale-document-hash",
        }
    }

    /// The offending document path when this refusal names one — the value for
    /// the structured error envelope's `path`. Only the whole-document
    /// `ContentDrift` CAS carries a path; the per-op anchor errors identify an
    /// anchor, not a path (the document is named by the report's `target`).
    pub fn path(&self) -> Option<&str> {
        match self {
            EditError::ContentDrift { path, .. } => Some(path),
            _ => None,
        }
    }
}

/// Apply `ops` to `old_body` sequentially. Returns the new body plus per-op
/// descriptors, or the first `EditError` (nothing is applied on error — the
/// caller never writes a partial result).
pub fn apply_edits(old_body: &str, ops: &[EditOp]) -> Result<EditTransform, EditError> {
    let mut body = old_body.to_string();
    let mut descriptors = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        let occurrences = apply_one(&mut body, op, index)?;
        descriptors.push(EditDescriptor {
            op: op.kind().to_string(),
            anchor_desc: op.anchor_desc(),
            occurrences,
        });
    }
    Ok(EditTransform {
        new_body: body,
        descriptors,
    })
}

/// Apply a single op in place. Returns the str_replace match count (else None).
fn apply_one(body: &mut String, op: &EditOp, index: usize) -> Result<Option<usize>, EditError> {
    match op {
        EditOp::StrReplace {
            old,
            new,
            replace_all,
        } => {
            if old.is_empty() {
                return Err(EditError::InvalidOp {
                    index,
                    kind: op.kind(),
                });
            }
            let count = body.matches(old.as_str()).count();
            if count == 0 {
                return Err(EditError::StrNotFound {
                    index,
                    kind: op.kind(),
                    anchor: op.anchor_desc(),
                });
            }
            if !replace_all && count > 1 {
                return Err(EditError::StrAmbiguous {
                    index,
                    kind: op.kind(),
                    anchor: op.anchor_desc(),
                    count,
                });
            }
            *body = if *replace_all {
                body.replace(old.as_str(), new)
            } else {
                body.replacen(old.as_str(), new, 1)
            };
            Ok(Some(count))
        }
        EditOp::ReplaceSection { heading, content } => {
            let span = resolve_section(body, heading, index, op.kind())?;
            let sep = body_separator(body, &span);
            let region = format!("{sep}{}", block(content));
            *body = splice(body, span.body_start..span.end, &region);
            Ok(None)
        }
        EditOp::DeleteSection { heading } => {
            let span = resolve_section(body, heading, index, op.kind())?;
            *body = splice(body, span.heading_start..span.end, "");
            Ok(None)
        }
        EditOp::AppendToSection { heading, content } => {
            let span = resolve_section(body, heading, index, op.kind())?;
            let existing = &body[span.body_start..span.end];
            // Preserve the section's trailing whitespace (e.g. the blank line
            // before the next heading): split into the body up to the last
            // non-blank char (`head`) and the trailing whitespace (`tail`), then
            // splice the new line in between.
            let head_len = existing.trim_end().len();
            let head = &existing[..head_len];
            // The section's trailing whitespace (`tail`) separates the appended
            // line from the following heading or EOF. If it does not begin with a
            // newline — an empty section, or one whose only trailing whitespace is
            // spaces — the appended line would weld onto the next heading (or leave
            // the document without a final newline), so substitute a single newline
            // (NRN-137). A tail that already starts with a newline is preserved
            // verbatim, keeping any blank line the section held before the next
            // heading.
            let tail = &existing[head_len..];
            let tail = if tail.starts_with('\n') { tail } else { "\n" };
            let line = content.trim_matches('\n');
            let region = if head.is_empty() {
                format!("{line}{tail}")
            } else {
                format!("{head}\n{line}{tail}")
            };
            // A heading at EOF with no trailing newline needs a separator before
            // the appended content, else the line welds onto the marker (NRN-437).
            let region = format!("{}{region}", body_separator(body, &span));
            *body = splice(body, span.body_start..span.end, &region);
            Ok(None)
        }
        EditOp::InsertBeforeHeading { heading, content } => {
            let span = resolve_section(body, heading, index, op.kind())?;
            *body = splice(
                body,
                span.heading_start..span.heading_start,
                &block(content),
            );
            Ok(None)
        }
        EditOp::InsertAfterHeading { heading, content } => {
            let span = resolve_section(body, heading, index, op.kind())?;
            let sep = body_separator(body, &span);
            let region = format!("{sep}{}", block(content));
            *body = splice(body, span.body_start..span.body_start, &region);
            Ok(None)
        }
    }
}

/// A normalized text block: trailing newlines stripped, then exactly one `\n`
/// appended — unless empty (stays ""). Used at every insertion/replace seam so
/// an op never produces a missing or doubled newline. `content` is otherwise
/// inserted verbatim (norn does not reflow it).
fn block(content: &str) -> String {
    let trimmed = content.trim_end_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn splice(body: &str, range: std::ops::Range<usize>, replacement: &str) -> String {
    let mut s = String::with_capacity(body.len() - (range.end - range.start) + replacement.len());
    s.push_str(&body[..range.start]);
    s.push_str(replacement);
    s.push_str(&body[range.end..]);
    s
}

/// Resolve a unique section by exact heading text against the CURRENT body,
/// adapting the shared resolver's [`SectionError`] into the indexed
/// [`EditError`] this batch reports (the op `index` and `kind` are edit-side
/// context the pure resolver does not carry). Re-parsed per op (sequential
/// semantics) — cheap for vault-scale docs and avoids cross-op offset
/// bookkeeping. Fence-safe: `#` inside a fenced code block never resolves.
fn resolve_section(
    body: &str,
    heading: &str,
    index: usize,
    kind: &'static str,
) -> Result<SectionSpan, EditError> {
    resolve_section_span(body, heading).map_err(|err| match err {
        SectionError::HeadingNotFound { heading } => EditError::HeadingNotFound {
            index,
            kind,
            heading,
        },
        SectionError::HeadingAmbiguous { heading, count } => EditError::HeadingAmbiguous {
            index,
            kind,
            heading,
            count,
        },
    })
}

/// The separator an op that inserts body content at `span.body_start` must emit
/// first when the heading line itself is not newline-terminated — a heading at
/// EOF with no trailing newline (`## A`), where `body_start == end == len`.
/// Without it the inserted content would weld onto the heading marker
/// (`## Ax`), demoting the heading (NRN-437). A normal newline-terminated
/// heading needs no separator (empty string), leaving every ordinary shape
/// unchanged.
fn body_separator(body: &str, span: &SectionSpan) -> &'static str {
    if body[span.heading_start..span.body_start].ends_with('\n') {
        ""
    } else {
        "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_replace(old: &str, new: &str, all: bool) -> EditOp {
        EditOp::StrReplace {
            old: old.into(),
            new: new.into(),
            replace_all: all,
        }
    }

    #[test]
    fn str_replace_unique_succeeds() {
        let out = apply_edits("hello world", &[str_replace("world", "norn", false)]).unwrap();
        assert_eq!(out.new_body, "hello norn");
        assert_eq!(out.descriptors[0].occurrences, Some(1));
    }

    #[test]
    fn str_replace_not_found_refuses() {
        let err = apply_edits("hello", &[str_replace("xyz", "q", false)]).unwrap_err();
        assert!(matches!(err, EditError::StrNotFound { index: 0, .. }));
    }

    #[test]
    fn str_replace_ambiguous_refuses() {
        let err = apply_edits("a a a", &[str_replace("a", "b", false)]).unwrap_err();
        assert_eq!(
            err,
            EditError::StrAmbiguous {
                index: 0,
                kind: "str_replace",
                anchor: r#"old="a""#.into(),
                count: 3
            }
        );
    }

    #[test]
    fn str_replace_all_replaces_every_occurrence() {
        let out = apply_edits("a a a", &[str_replace("a", "b", true)]).unwrap();
        assert_eq!(out.new_body, "b b b");
        assert_eq!(out.descriptors[0].occurrences, Some(3));
    }

    #[test]
    fn str_replace_empty_old_refuses() {
        let err = apply_edits("x", &[str_replace("", "y", false)]).unwrap_err();
        assert!(matches!(err, EditError::InvalidOp { index: 0, .. }));
    }

    const DOC: &str = "intro\n\n## Alpha\na1\na2\n\n## Beta\nb1\n";

    #[test]
    fn replace_section_swaps_body_keeps_heading() {
        let op = EditOp::ReplaceSection {
            heading: "Alpha".into(),
            content: "new alpha".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Alpha\nnew alpha\n## Beta\nb1\n");
    }

    #[test]
    fn delete_section_removes_heading_and_body() {
        let op = EditOp::DeleteSection {
            heading: "Alpha".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Beta\nb1\n");
    }

    #[test]
    fn last_section_runs_to_eof() {
        let op = EditOp::ReplaceSection {
            heading: "Beta".into(),
            content: "B".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Alpha\na1\na2\n\n## Beta\nB\n");
    }

    #[test]
    fn heading_not_found_refuses() {
        let op = EditOp::DeleteSection {
            heading: "Gamma".into(),
        };
        assert!(matches!(
            apply_edits(DOC, &[op]).unwrap_err(),
            EditError::HeadingNotFound { .. }
        ));
    }

    #[test]
    fn duplicate_heading_is_ambiguous() {
        let doc = "## Dup\nx\n## Dup\ny\n";
        let op = EditOp::DeleteSection {
            heading: "Dup".into(),
        };
        assert!(matches!(
            apply_edits(doc, &[op]).unwrap_err(),
            EditError::HeadingAmbiguous { count: 2, .. }
        ));
    }

    #[test]
    fn hash_inside_fence_is_not_a_heading() {
        let doc = "## Real\n```\n## Fake\n```\nbody\n";
        // "Fake" must not resolve.
        let op = EditOp::DeleteSection {
            heading: "Fake".into(),
        };
        assert!(matches!(
            apply_edits(doc, &[op]).unwrap_err(),
            EditError::HeadingNotFound { .. }
        ));
        // "Real" owns the whole doc including the fence.
        let op2 = EditOp::ReplaceSection {
            heading: "Real".into(),
            content: "z".into(),
        };
        assert_eq!(apply_edits(doc, &[op2]).unwrap().new_body, "## Real\nz\n");
    }

    #[test]
    fn append_to_section_adds_after_last_nonblank_line() {
        let op = EditOp::AppendToSection {
            heading: "Alpha".into(),
            content: "- new".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "intro\n\n## Alpha\na1\na2\n- new\n\n## Beta\nb1\n"
        );
    }

    #[test]
    fn append_to_empty_section() {
        let doc = "## Empty\n\n## Next\nx\n";
        let op = EditOp::AppendToSection {
            heading: "Empty".into(),
            content: "first".into(),
        };
        let out = apply_edits(doc, &[op]).unwrap();
        assert_eq!(out.new_body, "## Empty\nfirst\n## Next\nx\n");
    }

    #[test]
    fn append_to_empty_section_adjacent_heading() {
        let doc = "## Empty\n## Next\nx\n";
        let op = EditOp::AppendToSection {
            heading: "Empty".into(),
            content: "first".into(),
        };
        let out = apply_edits(doc, &[op]).unwrap();
        assert_eq!(out.new_body, "## Empty\nfirst\n## Next\nx\n");
    }

    #[test]
    fn append_to_empty_section_at_eof() {
        let doc = "## Empty\n";
        let op = EditOp::AppendToSection {
            heading: "Empty".into(),
            content: "first".into(),
        };
        let out = apply_edits(doc, &[op]).unwrap();
        assert_eq!(out.new_body, "## Empty\nfirst\n");
    }

    #[test]
    fn append_to_empty_section_with_whitespace_only_tail() {
        let doc = "## Empty\n   ";
        let op = EditOp::AppendToSection {
            heading: "Empty".into(),
            content: "first".into(),
        };
        let out = apply_edits(doc, &[op]).unwrap();
        assert_eq!(out.new_body, "## Empty\nfirst\n");
    }

    #[test]
    fn append_to_nonempty_section_without_trailing_newline() {
        let doc = "## A\nx";
        let op = EditOp::AppendToSection {
            heading: "A".into(),
            content: "y".into(),
        };
        let out = apply_edits(doc, &[op]).unwrap();
        assert_eq!(out.new_body, "## A\nx\ny\n");
    }

    #[test]
    fn insert_before_heading_places_content_above() {
        let op = EditOp::InsertBeforeHeading {
            heading: "Beta".into(),
            content: "BRIDGE".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "intro\n\n## Alpha\na1\na2\n\nBRIDGE\n## Beta\nb1\n"
        );
    }

    #[test]
    fn insert_after_heading_places_content_below_heading() {
        let op = EditOp::InsertAfterHeading {
            heading: "Beta".into(),
            content: "LEAD".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "intro\n\n## Alpha\na1\na2\n\n## Beta\nLEAD\nb1\n"
        );
    }

    #[test]
    fn ops_apply_sequentially_each_sees_prior_result() {
        // Op 1 renames the heading; op 2 must anchor on the NEW name.
        let ops = vec![
            EditOp::StrReplace {
                old: "## Alpha".into(),
                new: "## Renamed".into(),
                replace_all: false,
            },
            EditOp::AppendToSection {
                heading: "Renamed".into(),
                content: "tail".into(),
            },
        ];
        let out = apply_edits(DOC, &ops).unwrap();
        assert_eq!(
            out.new_body,
            "intro\n\n## Renamed\na1\na2\ntail\n\n## Beta\nb1\n"
        );
    }

    #[test]
    fn batch_is_atomic_failure_yields_no_partial() {
        let ops = vec![
            EditOp::StrReplace {
                old: "intro".into(),
                new: "INTRO".into(),
                replace_all: false,
            },
            EditOp::DeleteSection {
                heading: "Missing".into(),
            },
        ];
        let err = apply_edits(DOC, &ops).unwrap_err();
        assert!(matches!(err, EditError::HeadingNotFound { index: 1, .. }));
    }

    // ── NRN-437: SETEXT headings (`Alpha\n-----`) ────────────────────────────
    // The underline is part of the heading, not the body. Every section op must
    // keep the underline welded to its title line — replacing/inserting body
    // content must never consume the underline (demoting the heading to a
    // paragraph) nor push new text between the title and its underline (dragging
    // the inserted line under the heading).
    const SETEXT: &str = "Alpha\n-----\nbody under alpha.\n\n## Beta\nb1\n";

    #[test]
    fn setext_replace_section_keeps_the_underline() {
        let op = EditOp::ReplaceSection {
            heading: "Alpha".into(),
            content: "NEW".into(),
        };
        let out = apply_edits(SETEXT, &[op]).unwrap();
        assert_eq!(out.new_body, "Alpha\n-----\nNEW\n## Beta\nb1\n");
    }

    #[test]
    fn setext_append_to_section_keeps_the_underline() {
        let op = EditOp::AppendToSection {
            heading: "Alpha".into(),
            content: "- new".into(),
        };
        let out = apply_edits(SETEXT, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "Alpha\n-----\nbody under alpha.\n- new\n\n## Beta\nb1\n"
        );
    }

    #[test]
    fn setext_delete_section_removes_title_and_underline() {
        let op = EditOp::DeleteSection {
            heading: "Alpha".into(),
        };
        let out = apply_edits(SETEXT, &[op]).unwrap();
        assert_eq!(out.new_body, "## Beta\nb1\n");
    }

    #[test]
    fn setext_insert_before_heading_places_content_above_title() {
        let op = EditOp::InsertBeforeHeading {
            heading: "Alpha".into(),
            content: "TOP".into(),
        };
        let out = apply_edits(SETEXT, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "TOP\nAlpha\n-----\nbody under alpha.\n\n## Beta\nb1\n"
        );
    }

    #[test]
    fn setext_insert_after_heading_places_content_below_underline() {
        let op = EditOp::InsertAfterHeading {
            heading: "Alpha".into(),
            content: "LEAD".into(),
        };
        let out = apply_edits(SETEXT, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "Alpha\n-----\nLEAD\nbody under alpha.\n\n## Beta\nb1\n"
        );
    }

    // ── NRN-437: heading at EOF with no trailing newline (`## Tail`) ──────────
    // `body_start == end == len`, so an op inserting body content must supply the
    // missing line terminator first, else the content welds onto the marker
    // (`## TailLEAD`). The heading line must remain a heading.
    const EOF_HEADING: &str = "intro\n\n## Tail";

    #[test]
    fn eof_heading_replace_section_separates_from_marker() {
        let op = EditOp::ReplaceSection {
            heading: "Tail".into(),
            content: "NEW".into(),
        };
        let out = apply_edits(EOF_HEADING, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Tail\nNEW\n");
    }

    #[test]
    fn eof_heading_append_to_section_separates_from_marker() {
        let op = EditOp::AppendToSection {
            heading: "Tail".into(),
            content: "- new".into(),
        };
        let out = apply_edits(EOF_HEADING, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Tail\n- new\n");
    }

    #[test]
    fn eof_heading_delete_section_removes_the_heading() {
        let op = EditOp::DeleteSection {
            heading: "Tail".into(),
        };
        let out = apply_edits(EOF_HEADING, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n");
    }

    #[test]
    fn eof_heading_insert_before_heading_places_content_above() {
        let op = EditOp::InsertBeforeHeading {
            heading: "Tail".into(),
            content: "TOP".into(),
        };
        let out = apply_edits(EOF_HEADING, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\nTOP\n## Tail");
    }

    #[test]
    fn eof_heading_insert_after_heading_separates_from_marker() {
        let op = EditOp::InsertAfterHeading {
            heading: "Tail".into(),
            content: "LEAD".into(),
        };
        let out = apply_edits(EOF_HEADING, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Tail\nLEAD\n");
    }

    // ── NRN-164: forgiving ATX-prefixed heading anchors on the edit-op path ───
    // The plan/CLI edit ops inherit the shared resolver's exact-first-then-ATX
    // forgiveness: an anchor passed in its natural markdown form (`## Alpha`)
    // resolves the `Alpha` section without the caller stripping the prefix.

    #[test]
    fn atx_prefixed_anchor_resolves_replace_section() {
        let op = EditOp::ReplaceSection {
            heading: "## Alpha".into(),
            content: "new alpha".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(out.new_body, "intro\n\n## Alpha\nnew alpha\n## Beta\nb1\n");
    }

    #[test]
    fn atx_prefixed_anchor_level_is_syntax_noise() {
        // A `## Sub` anchor resolves a `### Sub` heading — level is ignored.
        let doc = "## Parent\np\n### Sub\ns\n";
        let op = EditOp::AppendToSection {
            heading: "## Sub".into(),
            content: "tail".into(),
        };
        let out = apply_edits(doc, &[op]).unwrap();
        assert_eq!(out.new_body, "## Parent\np\n### Sub\ns\ntail\n");
    }

    #[test]
    fn atx_prefixed_anchor_trailing_closer_stripped_insert_before() {
        let op = EditOp::InsertBeforeHeading {
            heading: "## Beta ##".into(),
            content: "BRIDGE".into(),
        };
        let out = apply_edits(DOC, &[op]).unwrap();
        assert_eq!(
            out.new_body,
            "intro\n\n## Alpha\na1\na2\n\nBRIDGE\n## Beta\nb1\n"
        );
    }

    #[test]
    fn atx_prefixed_missing_heading_still_refuses_unchanged() {
        let op = EditOp::DeleteSection {
            heading: "## Gamma".into(),
        };
        let err = apply_edits(DOC, &[op]).unwrap_err();
        assert_eq!(
            err,
            EditError::HeadingNotFound {
                index: 0,
                kind: "delete_section",
                heading: "## Gamma".into(),
            }
        );
    }
}
