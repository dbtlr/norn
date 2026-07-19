//! The ADR 0008 minimal-edit byte-splicing operations — the crate's defining
//! invariant.
//!
//! A field edit locates only the bytes of the field it touches
//! ([`top_level_property_spans`]) and splices a freshly serialized value
//! ([`serialize_value_preserving_style`]) into exactly that range with
//! `String::replace_range`. Every other byte of the document — key order,
//! comments, quote styles, blank lines, and the entire body — is left untouched,
//! so a one-field edit reads as a one-field diff.
//!
//! Two oracles keep the fragile span scanner honest, both ported from the donor:
//!
//! - the per-scalar round-trip in [`super::quote`] decides quoting by emitting
//!   and re-parsing against a real YAML parser, and
//! - the whole-block **post-image gate** ([`verify_post_image`]) refuses any
//!   edit whose result would not re-parse to exactly the intended mapping.
//!
//! A splice the scanner proposes but the post-image gate vetoes becomes a clean
//! refusal (an `Err`), never a corrupt write.
//!
//! ## Seam left behind
//!
//! The donor funneled these edits through `standards::apply::apply_file_changes`
//! alongside verb-level concerns — planned-change records, document hashes,
//! compare-and-swap `expected_old_value` preconditions, move/delete backlink
//! cascades. Those are mutation-verb responsibilities and port to `norn-core`
//! with the `set` / `edit` verbs; this module keeps only the pure
//! `(document, field ops) -> document` text transform.

use std::ops::Range;

use serde_json::Value;

use super::offsets::{top_level_property_spans, ValueStyle};
use super::parse::extract_frontmatter;
use super::quote::{
    render_key, serialize_array_block_field, serialize_value_preserving_style, QuoteError,
};

/// What to do to a single top-level frontmatter field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldAction {
    /// Replace an existing field's value, preserving its quote/collection style.
    Set(Value),
    /// Remove an existing field (its whole `line_range`).
    Remove,
    /// Add a field that is not already present, appended to the block.
    Add(Value),
}

/// One field operation for [`edit_fields`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldOp {
    pub field: String,
    pub action: FieldAction,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrontmatterEditError {
    #[error("frontmatter could not be parsed: {message}")]
    ParseFailed { message: String },
    #[error("frontmatter is not a top-level mapping")]
    NotAMapping,
    #[error("{reason}")]
    FieldAbsent { field: String, reason: String },
    #[error("field {field} value (style {style:?}) cannot be minimally edited in place")]
    NotEditable { field: String, style: ValueStyle },
    #[error("field {field} is a sequence; a Set on it requires an array value")]
    SequenceRequiresArray { field: String },
    #[error("field {field} is already present; use Set to replace it")]
    FieldAlreadyPresent { field: String },
    #[error("{0}")]
    Quote(String),
    #[error("post-image verification failed: {detail}")]
    PostImageMismatch { detail: String },
}

impl FrontmatterEditError {
    /// A stable kebab code a caller can branch on rather than string-matching the
    /// prose message.
    pub fn code(&self) -> &'static str {
        match self {
            FrontmatterEditError::ParseFailed { .. } => "frontmatter-parse-failed",
            FrontmatterEditError::NotAMapping => "frontmatter-not-a-mapping",
            FrontmatterEditError::FieldAbsent { .. } => "field-absent",
            FrontmatterEditError::NotEditable { .. } => "field-not-editable",
            FrontmatterEditError::SequenceRequiresArray { .. } => "sequence-requires-array",
            FrontmatterEditError::FieldAlreadyPresent { .. } => "field-already-present",
            FrontmatterEditError::Quote(_) => "quote-error",
            FrontmatterEditError::PostImageMismatch { .. } => "post-image-mismatch",
        }
    }
}

impl From<QuoteError> for FrontmatterEditError {
    fn from(error: QuoteError) -> Self {
        FrontmatterEditError::Quote(error.to_string())
    }
}

/// Replace an existing scalar or sequence field's value, preserving style.
pub fn set_field(
    content: &str,
    field: &str,
    value: &Value,
) -> Result<String, FrontmatterEditError> {
    edit_fields(
        content,
        &[FieldOp {
            field: field.to_string(),
            action: FieldAction::Set(value.clone()),
        }],
    )
}

/// Remove an existing top-level field, deleting its whole line range.
pub fn remove_field(content: &str, field: &str) -> Result<String, FrontmatterEditError> {
    edit_fields(
        content,
        &[FieldOp {
            field: field.to_string(),
            action: FieldAction::Remove,
        }],
    )
}

/// Add a not-yet-present field, appended to the end of the block. A document
/// with no frontmatter block gains a synthesized empty one first.
pub fn add_field(
    content: &str,
    field: &str,
    value: &Value,
) -> Result<String, FrontmatterEditError> {
    edit_fields(
        content,
        &[FieldOp {
            field: field.to_string(),
            action: FieldAction::Add(value.clone()),
        }],
    )
}

/// Apply a batch of field operations to a document's frontmatter with a single
/// minimal-edit pass. All edits are located against the original document, then
/// spliced from the rear so earlier byte ranges stay valid. The composed result
/// is verified against the intended mapping before it is returned; a splice that
/// would not re-parse to exactly that mapping is refused.
pub fn edit_fields(content: &str, ops: &[FieldOp]) -> Result<String, FrontmatterEditError> {
    if ops.is_empty() {
        return Ok(content.to_string());
    }

    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    let Some(frontmatter_range) = frontmatter_range else {
        // A malformed / unclosed block (a diagnostic but no located range) is a
        // parse failure to surface, not something to prepend a second block onto.
        if !diagnostics.is_empty() {
            return Err(FrontmatterEditError::ParseFailed {
                message: join_diagnostics(&diagnostics),
            });
        }
        // No frontmatter block at all: synthesize an empty one so an Add can
        // initialize it (schema backfill on a legacy file), then recurse.
        let synthesized = format!("---\n---\n{content}");
        return edit_fields(&synthesized, ops);
    };
    if !diagnostics.is_empty() {
        return Err(FrontmatterEditError::ParseFailed {
            message: join_diagnostics(&diagnostics),
        });
    }
    let Some(frontmatter_value) = frontmatter else {
        return Err(FrontmatterEditError::ParseFailed {
            message: "frontmatter could not be parsed".into(),
        });
    };
    let empty_object = serde_json::Map::new();
    let Some(current_object) = frontmatter_as_mapping(
        &frontmatter_value,
        content,
        &frontmatter_range,
        &empty_object,
    ) else {
        return Err(FrontmatterEditError::NotAMapping);
    };

    let spans = top_level_property_spans(content, frontmatter_range.clone(), current_object);

    // The mapping the ops intend to produce: the parsed original with each op's
    // semantic effect applied. The post-image gate compares the re-parsed result
    // against this.
    let mut expected: serde_json::Map<String, Value> = current_object.clone();
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();

    for op in ops {
        let field = op.field.as_str();
        let span = spans.iter().find(|s| s.name == field);

        match &op.action {
            FieldAction::Set(new_value) => {
                let Some(span) = span else {
                    return Err(FrontmatterEditError::FieldAbsent {
                        field: field.to_string(),
                        reason: span_absent_reason(field, current_object),
                    });
                };
                expected.insert(field.to_string(), new_value.clone());

                // Sequence styles (block AND flow) carry no scalar value_range;
                // replace the field's whole serde-aligned line_range with a fresh
                // serialization, preserving the collection style.
                if is_sequence_style(span.style) {
                    let Value::Array(items) = new_value else {
                        return Err(FrontmatterEditError::SequenceRequiresArray {
                            field: field.to_string(),
                        });
                    };
                    let replacement = if span.style == ValueStyle::FlowSequence {
                        let rendered = serialize_value_preserving_style(new_value, span.style)?;
                        // A flow set rewrites the whole line, dropping any trailing
                        // same-line comment; recover it for the single-line case.
                        let comment = single_line_trailing_comment(content, &span.line_range);
                        format!("{}: {rendered}{comment}\n", render_key(field))
                    } else {
                        serialize_array_block_field(field, items)?
                    };
                    edits.push((span.line_range.clone(), replacement));
                    continue;
                }

                let Some(value_range) = span.value_range.clone() else {
                    return Err(FrontmatterEditError::NotEditable {
                        field: field.to_string(),
                        style: span.style,
                    });
                };
                let replacement = serialize_value_preserving_style(new_value, span.style)?;
                edits.push((value_range, replacement));
            }
            FieldAction::Remove => {
                let Some(span) = span else {
                    return Err(FrontmatterEditError::FieldAbsent {
                        field: field.to_string(),
                        reason: span_absent_reason(field, current_object),
                    });
                };
                expected.remove(field);
                edits.push((span.line_range.clone(), String::new()));
            }
            FieldAction::Add(new_value) => {
                // Add refuses to overwrite; presence in the source (a span) is the
                // authority since current_object may hold a field whose value style
                // cannot be edited.
                if span.is_some() {
                    return Err(FrontmatterEditError::FieldAlreadyPresent {
                        field: field.to_string(),
                    });
                }
                expected.insert(field.to_string(), new_value.clone());
                // Insert at the end of the YAML content, just before the closing
                // `---`. The range ends after the final newline of the block, so a
                // new line splices in without disturbing the fence.
                let insertion = frontmatter_range.end;
                let leading_newline =
                    if insertion == 0 || content.as_bytes().get(insertion - 1) == Some(&b'\n') {
                        ""
                    } else {
                        "\n"
                    };
                let line_to_insert = match new_value {
                    Value::Array(items) => {
                        // Default to block style for new array fields (empty array
                        // renders as `field: []`).
                        let rendered = serialize_array_block_field(field, items)?;
                        format!("{leading_newline}{rendered}")
                    }
                    _ => {
                        let rendered =
                            serialize_value_preserving_style(new_value, ValueStyle::Plain)?;
                        format!("{leading_newline}{}: {rendered}\n", render_key(field))
                    }
                };
                edits.push((insertion..insertion, line_to_insert));
            }
        }
    }

    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = content.to_string();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }

    // Post-image gate: the composed frontmatter must re-parse to exactly the
    // intended mapping. A splice that produced unparseable YAML (a duplicate key,
    // an unclosed flow) or silently dropped/renamed a key (an unquoted `#foo:`
    // line YAML reads as a comment) is caught here and refused before returning.
    verify_post_image(&out, &expected)?;

    Ok(out)
}

/// A sequence-valued field (block or flow): both carry `value_range = None` and
/// are set by replacing the whole `line_range`.
fn is_sequence_style(style: ValueStyle) -> bool {
    matches!(style, ValueStyle::BlockSequence | ValueStyle::FlowSequence)
}

fn span_absent_reason(field: &str, current_object: &serde_json::Map<String, Value>) -> String {
    if current_object.contains_key(field) {
        format!(
            "field {field} is present but cannot be minimal-edited in place \
             (a frontmatter key could not be reliably located)"
        )
    } else {
        format!("field {field} not present in frontmatter")
    }
}

/// The trailing same-line YAML comment (with its leading whitespace, excluding
/// the newline) on the single line `content[line_range]`, or `""` when there is
/// none. Only single-line ranges are handled; a multi-line flow value's comment
/// is dropped. A `#` inside a quoted scalar is never a comment.
fn single_line_trailing_comment(content: &str, line_range: &Range<usize>) -> String {
    let body = content[line_range.clone()].trim_end_matches(['\r', '\n']);
    if body.contains('\n') {
        return String::new();
    }
    trailing_line_comment(body).unwrap_or("").to_string()
}

/// Locate a trailing YAML comment in a single newline-free `line`. Returns the
/// slice from the whitespace preceding `#` to end of line (e.g. `"  # note"`), or
/// `None`. A comment opener is a `#` preceded by whitespace and NOT inside a
/// single- or double-quoted scalar (honoring `''` and `\` escapes).
fn trailing_line_comment(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut comment_start = None;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'#' if i > 0 && matches!(bytes[i - 1], b' ' | b'\t') => {
                    comment_start = Some(i);
                    break;
                }
                _ => {}
            }
        }
        i += 1;
    }
    let start = comment_start?;
    let mut ws = start;
    while ws > 0 && matches!(bytes[ws - 1], b' ' | b'\t') {
        ws -= 1;
    }
    Some(&line[ws..])
}

/// Re-parse the composed frontmatter and confirm it equals `expected` — the
/// parsed original with each op's semantic effect applied. Parsed mappings are
/// compared, so key order and formatting are irrelevant. On a parse failure or a
/// mismatch, refuse: the span locator is a best-effort scanner, so this is the
/// trust-preserving backstop that turns a corrupting write into a clean decline.
fn verify_post_image(
    content: &str,
    expected: &serde_json::Map<String, Value>,
) -> Result<(), FrontmatterEditError> {
    let mut diagnostics = Vec::new();
    let (frontmatter, frontmatter_range, _, _) = extract_frontmatter(content, &mut diagnostics);
    if !diagnostics.is_empty() {
        return Err(FrontmatterEditError::PostImageMismatch {
            detail: format!(
                "result no longer parses: {}",
                join_diagnostics(&diagnostics)
            ),
        });
    }
    let empty = serde_json::Map::new();
    let actual = match (&frontmatter, &frontmatter_range) {
        (Some(value), Some(range)) => frontmatter_as_mapping(value, content, range, &empty)
            .ok_or_else(|| FrontmatterEditError::PostImageMismatch {
                detail: "result frontmatter is no longer a top-level mapping".into(),
            })?,
        (None, None) => &empty,
        _ => {
            return Err(FrontmatterEditError::PostImageMismatch {
                detail: "result frontmatter is no longer a top-level mapping".into(),
            });
        }
    };
    if actual != expected {
        return Err(FrontmatterEditError::PostImageMismatch {
            detail: "result frontmatter does not match the intended fields".into(),
        });
    }
    Ok(())
}

/// Normalize a parsed frontmatter value to its mapping form: a mapping is itself;
/// a YAML-null parse over an empty or whitespace-only block is the empty mapping
/// (an initializable `---\n---\n` block). Anything else — an explicit `null`/`~`
/// scalar, a sequence, a bare scalar — is not a mapping and yields `None`.
fn frontmatter_as_mapping<'a>(
    value: &'a Value,
    content: &str,
    frontmatter_range: &Range<usize>,
    empty: &'a serde_json::Map<String, Value>,
) -> Option<&'a serde_json::Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        Value::Null if content[frontmatter_range.clone()].trim().is_empty() => Some(empty),
        _ => None,
    }
}

fn join_diagnostics(diagnostics: &[crate::diagnostic::Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- The minimal-edit invariant: one field changes, all else is byte-identical.

    #[test]
    fn set_replaces_only_the_value_bytes() {
        let content = "---\ntitle: hello\nstatus: draft\n---\n# body\n";
        let out = set_field(content, "status", &json!("done")).unwrap();
        assert_eq!(out, "---\ntitle: hello\nstatus: done\n---\n# body\n");
    }

    #[test]
    fn set_preserves_double_quote_style() {
        let content = "---\nworkspace: \"[[norn]]\"\n---\n";
        let out = set_field(content, "workspace", &json!("[[other]]")).unwrap();
        assert_eq!(out, "---\nworkspace: \"[[other]]\"\n---\n");
    }

    #[test]
    fn set_preserves_single_quote_style() {
        let content = "---\nworkspace: '[[norn]]'\n---\n";
        let out = set_field(content, "workspace", &json!("[[other]]")).unwrap();
        assert_eq!(out, "---\nworkspace: '[[other]]'\n---\n");
    }

    #[test]
    fn set_preserves_same_line_comment() {
        let content = "---\nstatus: someday  # legacy\n---\n";
        let out = set_field(content, "status", &json!("done")).unwrap();
        assert_eq!(out, "---\nstatus: done  # legacy\n---\n");
    }

    #[test]
    fn set_preserves_key_order_and_untouched_fields() {
        let content = "---\nz: 1\na: 2\nm: 3\n---\nbody\n";
        let out = set_field(content, "a", &json!(99)).unwrap();
        // Key order (z, a, m) is a side effect of never rebuilding the mapping.
        assert_eq!(out, "---\nz: 1\na: 99\nm: 3\n---\nbody\n");
    }

    #[test]
    fn set_leaves_crlf_and_body_untouched() {
        let content = "---\r\ntitle: hello\r\nstatus: draft\r\n---\r\nbody\r\n";
        let out = set_field(content, "title", &json!("changed")).unwrap();
        assert_eq!(
            out,
            "---\r\ntitle: changed\r\nstatus: draft\r\n---\r\nbody\r\n"
        );
    }

    #[test]
    fn set_with_no_trailing_newline_preserves_absence() {
        let content = "---\ntitle: a\n---\nbody without newline";
        let out = set_field(content, "title", &json!("b")).unwrap();
        assert_eq!(out, "---\ntitle: b\n---\nbody without newline");
    }

    #[test]
    fn set_flow_sequence_stays_flow_and_keeps_comment() {
        let content = "---\ntags: [a, b]  # k\n---\n";
        let out = set_field(content, "tags", &json!(["c", "d"])).unwrap();
        assert_eq!(out, "---\ntags: [c, d]  # k\n---\n");
    }

    #[test]
    fn set_block_sequence_stays_block() {
        let content = "---\naliases:\n  - one\n  - two\ntype: note\n---\n";
        let out = set_field(content, "aliases", &json!(["x", "y"])).unwrap();
        assert_eq!(out, "---\naliases:\n  - x\n  - y\ntype: note\n---\n");
    }

    // ---- Refusals (the post-image gate and span refusals).

    #[test]
    fn set_on_block_literal_value_refuses() {
        let content = "---\ndesc: |\n  line one\n  line two\n---\n";
        let err = set_field(content, "desc", &json!("new")).unwrap_err();
        assert!(matches!(err, FrontmatterEditError::NotEditable { .. }));
    }

    #[test]
    fn set_on_absent_field_reports_absent() {
        let content = "---\ntitle: a\n---\n";
        let err = set_field(content, "missing", &json!("x")).unwrap_err();
        assert!(matches!(err, FrontmatterEditError::FieldAbsent { .. }));
        assert_eq!(err.code(), "field-absent");
    }

    #[test]
    fn set_numeric_looking_string_is_quoted_so_post_image_holds() {
        // "123" emitted plain would re-parse as a number; the quoting oracle
        // upgrades it and the value survives the post-image gate.
        let content = "---\nlabel: hi\n---\n";
        let out = set_field(content, "label", &json!("123")).unwrap();
        assert_eq!(out, "---\nlabel: '123'\n---\n");
    }

    // ---- Remove.

    #[test]
    fn remove_deletes_the_whole_line() {
        let content = "---\ntitle: a\nstatus: draft\n---\nbody\n";
        let out = remove_field(content, "status").unwrap();
        assert_eq!(out, "---\ntitle: a\n---\nbody\n");
    }

    #[test]
    fn remove_block_sequence_deletes_all_item_lines() {
        let content = "---\naliases:\n  - one\n  - two\ntype: note\n---\n";
        let out = remove_field(content, "aliases").unwrap();
        assert_eq!(out, "---\ntype: note\n---\n");
    }

    #[test]
    fn remove_absent_field_refuses() {
        let content = "---\ntitle: a\n---\n";
        let err = remove_field(content, "nope").unwrap_err();
        assert!(matches!(err, FrontmatterEditError::FieldAbsent { .. }));
    }

    // ---- Add.

    #[test]
    fn add_appends_a_new_field_at_end_of_block() {
        let content = "---\ntitle: a\n---\nbody\n";
        let out = add_field(content, "status", &json!("draft")).unwrap();
        assert_eq!(out, "---\ntitle: a\nstatus: draft\n---\nbody\n");
    }

    #[test]
    fn add_array_field_defaults_to_block_style() {
        let content = "---\ntitle: a\n---\n";
        let out = add_field(content, "aliases", &json!(["x", "y"])).unwrap();
        assert_eq!(out, "---\ntitle: a\naliases:\n  - x\n  - y\n---\n");
    }

    #[test]
    fn add_empty_array_renders_explicit_flow_list() {
        let content = "---\ntitle: a\n---\n";
        let out = add_field(content, "aliases", &json!([])).unwrap();
        assert_eq!(out, "---\ntitle: a\naliases: []\n---\n");
    }

    #[test]
    fn add_existing_field_refuses() {
        let content = "---\ntitle: a\n---\n";
        let err = add_field(content, "title", &json!("b")).unwrap_err();
        assert!(matches!(
            err,
            FrontmatterEditError::FieldAlreadyPresent { .. }
        ));
    }

    #[test]
    fn add_into_empty_block_initializes_it() {
        let content = "---\n---\nbody\n";
        let out = add_field(content, "type", &json!("note")).unwrap();
        assert_eq!(out, "---\ntype: note\n---\nbody\n");
    }

    #[test]
    fn add_into_document_with_no_block_synthesizes_one() {
        let content = "# just a body\n";
        let out = add_field(content, "type", &json!("note")).unwrap();
        assert_eq!(out, "---\ntype: note\n---\n# just a body\n");
    }

    #[test]
    fn add_quote_requiring_key_round_trips() {
        // An unquoted `#foo:` line YAML reads as a comment — the key would vanish.
        // render_key quotes it, and the post-image gate confirms it survives.
        let content = "---\ntitle: a\n---\n";
        let out = add_field(content, "#foo", &json!("bar")).unwrap();
        let reparsed: serde_yaml::Value =
            serde_yaml::from_str(out.trim_start_matches("---\n").trim_end_matches("---\n"))
                .unwrap();
        let value = serde_json::to_value(reparsed).unwrap();
        assert_eq!(value["#foo"], json!("bar"));
        assert_eq!(value["title"], json!("a"));
    }

    // ---- Batched ops.

    #[test]
    fn batch_set_and_add_apply_together() {
        let content = "---\ntitle: a\nstatus: draft\n---\nbody\n";
        let out = edit_fields(
            content,
            &[
                FieldOp {
                    field: "status".into(),
                    action: FieldAction::Set(json!("done")),
                },
                FieldOp {
                    field: "owner".into(),
                    action: FieldAction::Add(json!("me")),
                },
            ],
        )
        .unwrap();
        assert_eq!(out, "---\ntitle: a\nstatus: done\nowner: me\n---\nbody\n");
    }

    #[test]
    fn empty_ops_returns_input_unchanged() {
        let content = "---\ntitle: a\n---\n";
        assert_eq!(edit_fields(content, &[]).unwrap(), content);
    }

    // ---- Parse failures.

    #[test]
    fn set_on_unclosed_frontmatter_refuses() {
        let content = "---\ntitle: a\nno close here\n";
        let err = set_field(content, "title", &json!("b")).unwrap_err();
        assert!(matches!(err, FrontmatterEditError::ParseFailed { .. }));
    }

    // ---- Donor splice-path edge cases (ported from src/standards/apply.rs).

    #[test]
    fn add_refuses_explicit_null_scalar_block() {
        // An explicit `null` scalar block is NOT an empty block: splicing a key
        // after it would produce invalid YAML, so it must stay a hard refusal
        // rather than be treated as an empty mapping.
        let content = "---\nnull\n---\nbody\n";
        let err = add_field(content, "title", &json!("X")).unwrap_err();
        assert!(
            matches!(err, FrontmatterEditError::NotAMapping),
            "expected NotAMapping, got {err:?}"
        );
    }

    #[test]
    fn add_initializes_whitespace_only_frontmatter_block() {
        // A block whose content is only whitespace parses as null but IS empty —
        // it stays initializable (the pre-existing whitespace line is preserved
        // verbatim; the appended field still parses correctly).
        let content = "---\n   \n---\nbody\n";
        let out = add_field(content, "title", &json!("X")).unwrap();
        assert_eq!(out, "---\n   \ntitle: X\n---\nbody\n");
    }

    // ---- BOM'd documents (NRN-349): the BOM byte is preserved; edits splice
    // after it, and the frontmatter is found rather than duplicated.

    #[test]
    fn set_on_bom_doc_preserves_the_bom_and_every_other_byte() {
        // Minimal-edit invariant under a leading BOM: only the value bytes change;
        // the BOM, fences, and body are byte-identical.
        let content = "\u{feff}---\ntitle: hello\nstatus: draft\n---\n# body\n";
        let out = set_field(content, "status", &json!("done")).unwrap();
        assert_eq!(
            out,
            "\u{feff}---\ntitle: hello\nstatus: done\n---\n# body\n"
        );
        assert!(out.starts_with('\u{feff}'));
    }

    #[test]
    fn set_field_finds_a_field_in_a_bom_doc() {
        let content = "\u{feff}---\ntitle: hello\n---\n";
        let out = set_field(content, "title", &json!("changed")).unwrap();
        assert_eq!(out, "\u{feff}---\ntitle: changed\n---\n");
    }

    #[test]
    fn add_on_bom_doc_appends_into_the_existing_block_not_a_duplicate() {
        // The NRN-339-review adversarial doc: before BOM recognition, a BOM'd doc
        // read as frontmatter-less made `add_field` prepend a SECOND `---` block.
        // Now the existing block is found and the field is appended into it — a
        // single block, BOM intact.
        let content = "\u{feff}---\ntitle: hello\n---\n# body\n";
        let out = add_field(content, "status", &json!("draft")).unwrap();
        assert_eq!(
            out,
            "\u{feff}---\ntitle: hello\nstatus: draft\n---\n# body\n"
        );
        // Exactly one frontmatter block (two fence lines), not two.
        assert_eq!(out.matches("---\n").count(), 2);
    }

    #[test]
    fn set_rejects_scalar_into_block_sequence_target() {
        // Setting a block-sequence field to a scalar must refuse: a sequence field
        // requires an array value, never a bare scalar.
        let content = "---\naliases:\n  - one\n  - two\n---\n";
        let err = set_field(content, "aliases", &json!("one")).unwrap_err();
        assert!(
            matches!(err, FrontmatterEditError::SequenceRequiresArray { .. }),
            "expected SequenceRequiresArray, got {err:?}"
        );
    }
}
