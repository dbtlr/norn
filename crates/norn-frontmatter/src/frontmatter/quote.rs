use super::offsets::ValueStyle;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum QuoteError {
    // Superseded by NonScalarValue / StructuredOriginalStyle / ArrayIntoScalar
    // (no code path constructs this); safe to delete in a cleanup pass.
    #[allow(dead_code)]
    #[error("cannot represent value {value:?} in original style {original_style:?}")]
    Unrepresentable {
        value: Value,
        original_style: ValueStyle,
    },
    #[error("only scalar values are supported for minimal-edit set_frontmatter")]
    NonScalarValue,
    #[error("structured original style {0:?} does not support minimal-edit set_frontmatter")]
    StructuredOriginalStyle(ValueStyle),
    #[error(
        "cannot set an array value on a scalar field; remove the field first, then push values"
    )]
    ArrayIntoScalar,
}

/// Returns the YAML bytes that replace the `value_range` of an original property
/// when applying a `set_frontmatter` change. Preserves quote style when the new
/// value can be represented in the original style; upgrades to a stricter style
/// otherwise. Never downgrades.
///
/// Supports scalar string, number, boolean, and null values as well as
/// `Value::Array` when the `original_style` is `FlowSequence` or
/// `BlockSequence`.  Returns `QuoteError::ArrayIntoScalar` when an array value
/// is supplied for a scalar-style field, and `QuoteError::NonScalarValue` for
/// objects.
///
/// For `BlockSequence` the returned string is the full block replacement
/// including the `key:` prefix line — callers must replace `span.line_range`
/// (not `span.value_range`) when emitting block arrays.
pub fn serialize_value_preserving_style(
    new_value: &Value,
    original_style: ValueStyle,
) -> Result<String, QuoteError> {
    match new_value {
        Value::Array(items) => return serialize_array(items, original_style),
        Value::Object(_) => return Err(QuoteError::NonScalarValue),
        _ => {}
    }

    // Scalar path: refuse structured original styles.
    match original_style {
        ValueStyle::BlockLiteral
        | ValueStyle::BlockFolded
        | ValueStyle::FlowSequence
        | ValueStyle::FlowMapping
        | ValueStyle::BlockSequence
        | ValueStyle::BlockMapping => {
            return Err(QuoteError::StructuredOriginalStyle(original_style));
        }
        _ => {}
    }

    match new_value {
        Value::Null => Ok("~".to_string()),
        Value::Bool(b) => Ok(if *b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        Value::Number(n) => Ok(n.to_string()),
        Value::String(s) => Ok(serialize_string_value(
            s,
            original_style,
            ScalarContext::Block,
        )),
        // Array / Object already handled above.
        Value::Array(_) | Value::Object(_) => unreachable!(),
    }
}

/// Serialize an array value, respecting the original field style.
///
/// - `FlowSequence` → inline `[item1, item2]`
/// - `BlockSequence` → returns the **key-less** block items: `  - item1\n  - item2\n`
///   (the caller is responsible for emitting `key:\n` before this output and
///   using `span.line_range` as the replacement range).
/// - Scalar styles → `Err(QuoteError::ArrayIntoScalar)` (refusing to turn a
///   scalar field into an array; caller should remove then push).
/// - Other structured styles → `Err(QuoteError::StructuredOriginalStyle)`.
fn serialize_array(items: &[Value], original_style: ValueStyle) -> Result<String, QuoteError> {
    match original_style {
        ValueStyle::BlockSequence => {
            // Return only the items portion. Caller appends after `key:\n`.
            serialize_array_block_items(items)
        }
        ValueStyle::FlowSequence => {
            // Inline `[item1, item2, item3]`. Each string item quoted per
            // scalar rules (Plain when safe); non-string items unquoted.
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&render_array_item(item, ScalarContext::Flow)?);
            }
            out.push(']');
            Ok(out)
        }
        ValueStyle::Plain
        | ValueStyle::SingleQuoted
        | ValueStyle::DoubleQuoted
        | ValueStyle::EmptyValue => Err(QuoteError::ArrayIntoScalar),
        ValueStyle::BlockLiteral
        | ValueStyle::BlockFolded
        | ValueStyle::FlowMapping
        | ValueStyle::BlockMapping => Err(QuoteError::StructuredOriginalStyle(original_style)),
    }
}

/// Serialize a brand-new document with frontmatter and body.
///
/// Used by `norn new` (and any future caller that synthesizes a complete
/// Markdown file from scratch). Unlike [`serialize_value_preserving_style`]
/// which operates on individual values for in-place editing, this entry
/// point emits a full `---` frontmatter block plus body.
///
/// Semantics:
/// - Fields are emitted in key order (BTreeMap iteration order).
/// - `Value::Null` fields are skipped (e.g., required-but-undefaulted
///   fields per the `norn new` warn-don't-block model).
/// - Array values use [`serialize_array_block_field`].
/// - Scalar values pick a style based on YAML safety: wikilinks / strings
///   with `:` / strings starting with YAML indicators are quoted; others
///   are plain.
/// - If `body` doesn't end with a newline, one is appended.
pub fn serialize_new_document(
    frontmatter: &std::collections::BTreeMap<String, serde_json::Value>,
    body: &str,
) -> Result<String, QuoteError> {
    let mut out = String::from("---\n");
    for (field, value) in frontmatter {
        if value.is_null() {
            continue;
        }
        if let Some(items) = value.as_array() {
            out.push_str(&serialize_array_block_field(field, items)?);
        } else {
            let style = scalar_style_for(value);
            let serialized = serialize_value_preserving_style(value, style)?;
            out.push_str(&format!("{}: {serialized}\n", render_key(field)));
        }
    }
    out.push_str("---\n");
    if !body.is_empty() {
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

fn scalar_style_for(value: &Value) -> ValueStyle {
    if let Some(s) = value.as_str() {
        let first = s.chars().next();
        let needs_quoting = s.starts_with('[')
            || s.contains(':')
            || matches!(first, Some('-' | '?' | '*' | '&' | '!' | '%' | '@' | '`'));
        if needs_quoting {
            return ValueStyle::DoubleQuoted;
        }
    }
    ValueStyle::Plain
}

/// Serialize a complete `field: <array>` frontmatter entry (trailing newline
/// included). Non-empty arrays emit block style (`field:\n  - item1\n`); an
/// empty array emits an explicit `field: []\n` — a bare `field:` line reads
/// back as YAML null, not the empty list it was written as (NRN-141). The
/// single entry point for every path that writes an array-valued field line
/// (`norn new`, `add_frontmatter`, block-sequence `set`).
pub fn serialize_array_block_field(field: &str, items: &[Value]) -> Result<String, QuoteError> {
    let key = render_key(field);
    if items.is_empty() {
        return Ok(format!("{key}: []\n"));
    }
    Ok(format!("{key}:\n{}", serialize_array_block_items(items)?))
}

/// Serialize an array as block-style YAML items: the items portion only
/// (`  - item1\n  - item2\n`, 2-space indent, trailing newline), each item
/// quoted per scalar rules. An empty array emits an empty string — callers
/// route through [`serialize_array_block_field`], which renders an empty
/// array as `field: []` instead.
fn serialize_array_block_items(items: &[Value]) -> Result<String, QuoteError> {
    let mut out = String::new();
    for item in items {
        let rendered = render_array_item(item, ScalarContext::Block)?;
        out.push_str("  - ");
        out.push_str(&rendered);
        out.push('\n');
    }
    Ok(out)
}

/// The YAML lexical context a scalar is being emitted into. Round-trip
/// verification must reparse the scalar in the same context it will live in:
/// in a `Flow` context (`k: [<val>]`) the flow indicators (`,` `[` `]` `{` `}`),
/// `: `, and leading/trailing space are structural where a `Block` context
/// (`k: <val>`) treats them as legal plain bytes. Emitting a flow item verified
/// only in block context silently mis-splits (`a,b` → two items) or produces
/// invalid YAML (`a]b` → `[a]b]`) (NRN-141). In a `Key` context (`<key>: x`)
/// the scalar is a mapping KEY: a bare `#` turns the line into a comment, an
/// embedded `: ` splits it into nested mappings, and `123`/`true`/`null` parse
/// to non-string keys — all must escalate to a quote (NRN-142).
#[derive(Debug, Clone, Copy)]
enum ScalarContext {
    Block,
    Flow,
    Key,
}

/// Render a single array item as a YAML scalar string in the given lexical
/// context.
///
/// Strings are rendered via plain-style quoting (upgraded when necessary).
/// Numbers, booleans, and null are rendered without quotes.
/// Objects produce `QuoteError::NonScalarValue`.
fn render_array_item(item: &Value, context: ScalarContext) -> Result<String, QuoteError> {
    match item {
        Value::String(s) => Ok(serialize_string_value(s, ValueStyle::Plain, context)),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(if *b {
            "true".to_string()
        } else {
            "false".to_string()
        }),
        Value::Null => Ok("~".to_string()),
        Value::Array(_) | Value::Object(_) => Err(QuoteError::NonScalarValue),
    }
}

/// YAML scalar quoting rank, ordered by strictness. Escalation only moves up
/// this ladder, never down (so an explicit quote style is never weakened).
/// Double-quoted with escaping is the guaranteed terminal — it can represent
/// any string that YAML can hold.
const RANK_PLAIN: u8 = 0;
const RANK_SINGLE: u8 = 1;
const RANK_DOUBLE: u8 = 2;

/// Render `s` as a YAML scalar at the given quoting rank.
fn render_scalar_at_rank(s: &str, rank: u8) -> String {
    match rank {
        RANK_PLAIN => s.to_string(),
        RANK_SINGLE => format!("'{}'", s.replace('\'', "''")),
        _ => format!("\"{}\"", escape_double_quoted(s)),
    }
}

/// True if the rendered scalar, spliced into the template line for `context`,
/// reparses through a real YAML parser back to exactly `expected` (as a string —
/// a value that reparses to a number, bool, or mapping does NOT round-trip).
/// This is the invariant that replaces the hand-maintained plain-scalar
/// denylist: correctness is decided by the YAML standard, not by an enumerated
/// hazard list (NRN-118).
fn scalar_round_trips(rendered: &str, expected: &str, context: ScalarContext) -> bool {
    match context {
        ScalarContext::Block => {
            let doc = format!("k: {rendered}");
            match serde_yaml::from_str::<serde_yaml::Value>(&doc) {
                Ok(v) => v.get("k").and_then(|x| x.as_str()) == Some(expected),
                Err(_) => false,
            }
        }
        ScalarContext::Flow => {
            // Verify inside a flow sequence: the scalar reparses only if it is the
            // sole element AND decodes to exactly `expected` — so a value that
            // would mis-split on `,`/`]`/`[` fails here and escalates to a quote.
            let doc = format!("k: [{rendered}]");
            match serde_yaml::from_str::<serde_yaml::Value>(&doc) {
                Ok(v) => match v.get("k").and_then(|x| x.as_sequence()) {
                    Some(seq) if seq.len() == 1 => seq[0].as_str() == Some(expected),
                    _ => false,
                },
                Err(_) => false,
            }
        }
        ScalarContext::Key => {
            // Verify in KEY position: the scalar reparses only if the mapping has
            // exactly one entry whose key is byte-exactly `expected` and whose
            // value is the sentinel `x`. A plain `#foo` fails (the line becomes a
            // comment); a plain `a: b` fails (`a: b: x` is invalid or mis-keyed);
            // `123`/`true`/`null` fail as strings (non-string keys).
            let doc = format!("{rendered}: x\n");
            match serde_yaml::from_str::<serde_yaml::Value>(&doc) {
                Ok(serde_yaml::Value::Mapping(m)) if m.len() == 1 => m
                    .iter()
                    .next()
                    .is_some_and(|(k, v)| k.as_str() == Some(expected) && v.as_str() == Some("x")),
                _ => false,
            }
        }
    }
}

/// Render a field NAME for YAML key position, quoting only when required.
///
/// The mapping-value counterpart is [`serialize_string_value`]: both are entry
/// points into the same [`escalate_to_round_trip`] ladder, differing only in
/// context (`Key` vs `Block`/`Flow`) and starting rank (a key always proposes
/// plain first). A plain identifier key (`status`, `title`) round-trips at
/// [`RANK_PLAIN`], so it renders identically to the bare name — no
/// gratuitous quoting.
///
/// Every line-rebuild that emits a key (flow/block collection `set`,
/// `add_frontmatter` splices, `norn new` field lines) routes through here so a
/// quote-requiring key (`#foo`, `a: b`) reserializes correctly instead of
/// producing a comment line or invalid YAML (NRN-142).
pub fn render_key(field: &str) -> String {
    escalate_to_round_trip(field, RANK_PLAIN, ScalarContext::Key)
}

fn serialize_string_value(s: &str, original_style: ValueStyle, context: ScalarContext) -> String {
    // Pick the starting style, preserving an explicit quote style and preferring
    // plain for a plain/empty origin. `is_plain_safe` is only a fast first guess
    // here — the round-trip check below is the authority.
    let start_rank = match original_style {
        ValueStyle::DoubleQuoted => RANK_DOUBLE,
        ValueStyle::SingleQuoted => RANK_SINGLE,
        // Plain / EmptyValue.
        _ => {
            if is_plain_safe(s) {
                RANK_PLAIN
            } else if !s.contains('\'') {
                RANK_SINGLE
            } else {
                RANK_DOUBLE
            }
        }
    };
    escalate_to_round_trip(s, start_rank, context)
}

/// The single quoting-escalation loop behind every scalar emission (values via
/// [`serialize_string_value`], keys via [`render_key`]): emit at `start_rank`,
/// then climb the ladder until the rendered scalar round-trips exactly
/// in `context`. Double-quoted+escape is the terminal that round-trips every
/// representable string in VALUE position, so for values the trailing fallback
/// is unreachable. In KEY position one input class defeats every rank — a
/// simple key past libyaml's 1024-byte parse limit, which no quoting style can
/// make parseable — so the terminal render is returned UNVERIFIED there; the
/// post-image gates (apply's `verify_post_image`, create's
/// `verify_created_document`) backstop it, converting the resulting write into
/// a clean refusal (NRN-142).
fn escalate_to_round_trip(s: &str, start_rank: u8, context: ScalarContext) -> String {
    for rank in start_rank..=RANK_DOUBLE {
        let rendered = render_scalar_at_rank(s, rank);
        if scalar_round_trips(&rendered, s, context) {
            return rendered;
        }
    }
    render_scalar_at_rank(s, RANK_DOUBLE)
}

/// Escape a string for a YAML double-quoted scalar. Double-quoted is the
/// terminal quoting rank, so this must produce output that reparses byte-
/// identically for EVERY input — in particular control characters, which YAML
/// forbids literally inside quotes (they make the parser reject the document)
/// and NEL / line- / paragraph-separators, which fold to a space if left bare.
/// Uses YAML's named escapes where defined and `\xXX` for the rest (NRN-118).
fn escape_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\0' => out.push_str("\\0"),
            '\u{07}' => out.push_str("\\a"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0b}' => out.push_str("\\v"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{1b}' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            // Any remaining control character (C0, DEL, C1) must be escaped —
            // all are <= 0xFF, so a two-hex `\xXX` is always sufficient.
            c if c.is_control() => out.push_str(&format!("\\x{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn is_plain_safe(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap();
    if matches!(
        first,
        '-' | '?'
            | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '#'
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
    ) {
        return false;
    }
    if first.is_whitespace() {
        return false;
    }
    if s.contains(": ") || s.contains(" #") {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
    ) {
        return false;
    }
    if s.chars().last().is_some_and(char::is_whitespace) {
        return false;
    }
    if s.contains('\n') || s.contains('\r') {
        return false;
    }
    // Strings containing a single quote get upgraded to double-quoted so we
    // don't have to deal with single-quote escaping in a plain context.
    if s.contains('\'') {
        return false;
    }
    true
}

#[cfg(test)]
mod serialize_new_document_tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn serialize_new_document_basic_scalar_fields() {
        let mut fm = BTreeMap::new();
        fm.insert("type".into(), json!("task"));
        fm.insert("status".into(), json!("backlog"));

        let out = serialize_new_document(&fm, "").unwrap();
        assert!(out.starts_with("---\n"), "got:\n{out}");
        assert!(out.contains("status: backlog\n"));
        assert!(out.contains("type: task\n"));
        // Closing fence must appear after the last frontmatter field,
        // not immediately after the opening fence.
        // The output should contain a second `---\n` that closes the block;
        // we verify the closing fence is not the first occurrence.
        let first_fence_end = out.find("---\n").unwrap() + 4;
        let remaining = &out[first_fence_end..];
        assert!(
            remaining.contains("---\n"),
            "closing fence missing, got:\n{out}"
        );
        // Empty body — file ends with closing fence + newline only.
        assert!(out.ends_with("---\n"));
    }

    #[test]
    fn serialize_new_document_quotes_wikilink_values() {
        let mut fm = BTreeMap::new();
        fm.insert("workspace".into(), json!("[[norn]]"));

        let out = serialize_new_document(&fm, "").unwrap();
        // Wikilinks must be quoted — `[` starts a YAML flow sequence otherwise.
        assert!(
            out.contains("workspace: \"[[norn]]\"\n") || out.contains("workspace: '[[norn]]'\n"),
            "expected quoted wikilink, got:\n{out}"
        );
    }

    #[test]
    fn serialize_new_document_with_body() {
        let mut fm = BTreeMap::new();
        fm.insert("type".into(), json!("note"));

        let out = serialize_new_document(&fm, "# Hello\n\nBody content.\n").unwrap();
        assert!(out.ends_with("# Hello\n\nBody content.\n"), "got:\n{out}");
    }

    #[test]
    fn serialize_new_document_appends_trailing_newline_to_body() {
        let mut fm = BTreeMap::new();
        fm.insert("type".into(), json!("note"));

        let out = serialize_new_document(&fm, "Body without newline").unwrap();
        assert!(out.ends_with("Body without newline\n"), "got:\n{out}");
    }

    #[test]
    fn serialize_new_document_skips_null_values() {
        let mut fm = BTreeMap::new();
        fm.insert("type".into(), json!("note"));
        fm.insert("description".into(), serde_json::Value::Null);

        let out = serialize_new_document(&fm, "").unwrap();
        assert!(out.contains("type: note\n"));
        assert!(
            !out.contains("description"),
            "null field should not be emitted, got:\n{out}"
        );
    }

    #[test]
    fn serialize_new_document_arrays_emit_as_block() {
        let mut fm = BTreeMap::new();
        fm.insert("aliases".into(), json!(["foo", "bar"]));

        let out = serialize_new_document(&fm, "").unwrap();
        // Block-style: `aliases:` on one line followed by `- foo` / `- bar` lines.
        assert!(out.contains("aliases:"), "got:\n{out}");
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn serialize_new_document_keys_sorted_alphabetically() {
        // BTreeMap iterates in key order, so output should be alphabetical.
        let mut fm = BTreeMap::new();
        fm.insert("z_last".into(), json!("z"));
        fm.insert("a_first".into(), json!("a"));
        fm.insert("m_middle".into(), json!("m"));

        let out = serialize_new_document(&fm, "").unwrap();
        let a_pos = out.find("a_first:").unwrap();
        let m_pos = out.find("m_middle:").unwrap();
        let z_pos = out.find("z_last:").unwrap();
        assert!(a_pos < m_pos);
        assert!(m_pos < z_pos);
    }

    #[test]
    fn serialize_new_document_empty_array_emits_empty_flow_list() {
        // NRN-141: a bare `field:` line reads back as null; an empty array must
        // serialize as `field: []` so it round-trips to the empty list.
        let mut fm = BTreeMap::new();
        fm.insert("aliases".into(), json!([]));

        let out = serialize_new_document(&fm, "").unwrap();
        assert_eq!(out, "---\naliases: []\n---\n");
        let yaml = out.trim_start_matches("---\n").trim_end_matches("---\n");
        let parsed: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            serde_json::to_value(parsed).unwrap().get("aliases"),
            Some(&json!([]))
        );
    }

    #[test]
    fn serialize_new_document_empty_frontmatter_emits_just_fences() {
        let fm = BTreeMap::new();
        let out = serialize_new_document(&fm, "").unwrap();
        assert_eq!(out, "---\n---\n");
    }

    #[test]
    fn serialize_new_document_quotes_keys_that_require_quoting() {
        // NRN-142: a quote-needing field name (reachable via `norn new
        // --field '#foo=bar'`) must emit a quoted key line so the created
        // document round-trips instead of dropping the key to a comment. The
        // create_document path has no post-image gate, so this is the only guard.
        let mut fm = BTreeMap::new();
        fm.insert("#foo".into(), json!("bar"));
        fm.insert("a: b".into(), json!(["x"]));

        let out = serialize_new_document(&fm, "").unwrap();
        let yaml = out.trim_start_matches("---\n").trim_end_matches("---\n");
        let parsed: serde_yaml::Value = serde_yaml::from_str(yaml)
            .unwrap_or_else(|e| panic!("created frontmatter must parse: {out:?}: {e}"));
        let json = serde_json::to_value(parsed).unwrap();
        assert_eq!(json["#foo"], json!("bar"));
        assert_eq!(json["a: b"], json!(["x"]));
    }

    #[test]
    fn serialize_new_document_value_with_colon_is_quoted() {
        // Strings containing `:` must be quoted to avoid YAML ambiguity
        // (`time: 10:30` would parse `10:30` as a sexagesimal in older YAML).
        let mut fm = BTreeMap::new();
        fm.insert("note".into(), json!("see 12:34 for context"));

        let out = serialize_new_document(&fm, "").unwrap();
        assert!(
            out.contains("note: \"see 12:34 for context\"")
                || out.contains("note: 'see 12:34 for context'"),
            "expected quoted value with embedded colon, got:\n{out}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn double_quoted_string_stays_double_quoted() {
        let result =
            serialize_value_preserving_style(&json!("[[other]]"), ValueStyle::DoubleQuoted)
                .unwrap();
        assert_eq!(result, "\"[[other]]\"");
    }

    #[test]
    fn single_quoted_string_stays_single_quoted() {
        let result =
            serialize_value_preserving_style(&json!("[[other]]"), ValueStyle::SingleQuoted)
                .unwrap();
        assert_eq!(result, "'[[other]]'");
    }

    #[test]
    fn plain_string_safe_for_plain_stays_plain() {
        let result =
            serialize_value_preserving_style(&json!("completed"), ValueStyle::Plain).unwrap();
        assert_eq!(result, "completed");
    }

    #[test]
    fn plain_string_with_colon_upgrades_to_single_quoted() {
        let result = serialize_value_preserving_style(&json!("a: b"), ValueStyle::Plain).unwrap();
        assert_eq!(result, "'a: b'");
    }

    #[test]
    fn plain_string_with_leading_dash_upgrades() {
        let result = serialize_value_preserving_style(&json!("-foo"), ValueStyle::Plain).unwrap();
        assert!(result.starts_with('\'') || result.starts_with('"'));
    }

    #[test]
    fn plain_string_containing_single_quote_upgrades_to_double_quoted() {
        let result = serialize_value_preserving_style(&json!("don't"), ValueStyle::Plain).unwrap();
        assert_eq!(result, "\"don't\"");
    }

    #[test]
    fn single_quoted_value_with_internal_single_quote_doubles_it() {
        let result =
            serialize_value_preserving_style(&json!("don't"), ValueStyle::SingleQuoted).unwrap();
        assert_eq!(result, "'don''t'");
    }

    #[test]
    fn double_quoted_value_with_internal_double_quote_escapes_it() {
        let result =
            serialize_value_preserving_style(&json!("say \"hi\""), ValueStyle::DoubleQuoted)
                .unwrap();
        assert_eq!(result, "\"say \\\"hi\\\"\"");
    }

    #[test]
    fn number_renders_plain_regardless_of_original_style() {
        let result =
            serialize_value_preserving_style(&json!(42), ValueStyle::DoubleQuoted).unwrap();
        assert_eq!(result, "42");
    }

    #[test]
    fn boolean_renders_plain() {
        let result = serialize_value_preserving_style(&json!(true), ValueStyle::Plain).unwrap();
        assert_eq!(result, "true");
    }

    #[test]
    fn null_renders_as_tilde() {
        let result = serialize_value_preserving_style(&json!(null), ValueStyle::Plain).unwrap();
        assert_eq!(result, "~");
    }

    /// Reparse `key: <rendered>` and return the value as a string, or `None` if
    /// it did not round-trip to a string scalar. Mirrors the real splice site.
    fn reparse(rendered: &str) -> Option<String> {
        let doc = format!("k: {rendered}");
        let v: serde_yaml::Value = serde_yaml::from_str(&doc).ok()?;
        v.get("k").and_then(|x| x.as_str()).map(str::to_string)
    }

    #[test]
    fn trailing_colon_string_round_trips_not_plain() {
        // "foo:" passed the is_plain_safe denylist (no ": " space) and emitted
        // plain, which reparses as a mapping — corrupt frontmatter (NRN-118).
        let result = serialize_value_preserving_style(&json!("foo:"), ValueStyle::Plain).unwrap();
        assert_eq!(result, "'foo:'");
        assert_eq!(reparse(&result).as_deref(), Some("foo:"));
    }

    #[test]
    fn embedded_newline_escalates_to_double_quoted_not_folded_single() {
        // A newline string was emitted single-quoted, which folds the newline to
        // a space (silent corruption). Only double-quoting preserves it (NRN-118).
        let result =
            serialize_value_preserving_style(&json!("a\nb"), ValueStyle::SingleQuoted).unwrap();
        assert_eq!(result, "\"a\\nb\"");
        assert_eq!(reparse(&result).as_deref(), Some("a\nb"));
    }

    #[test]
    fn numeric_looking_string_is_quoted_to_stay_a_string() {
        // "123" emitted plain reparses as the number 123, not the string (NRN-118).
        let result = serialize_value_preserving_style(&json!("123"), ValueStyle::Plain).unwrap();
        assert_eq!(result, "'123'");
        assert_eq!(reparse(&result).as_deref(), Some("123"));
    }

    #[test]
    fn control_chars_round_trip_via_double_quoted_terminal() {
        // The double-quoted terminal must be a TRUE terminal: control chars
        // emitted literally make serde_yaml reject the doc, so all ranks fail
        // round-trip and the fallback emits corrupt bytes. Every control char
        // must escape and reparse to itself (NRN-118 round-trip review).
        for c in [
            '\u{0}', '\u{7}', '\u{8}', '\u{b}', '\u{c}', '\u{1b}', '\u{7f}',
        ] {
            let s = format!("a{c}b");
            let result = serialize_value_preserving_style(&json!(s), ValueStyle::Plain).unwrap();
            assert_eq!(
                reparse(&result).as_deref(),
                Some(s.as_str()),
                "control char U+{:04X} did not round-trip (emitted {result:?})",
                c as u32
            );
        }
    }

    #[test]
    fn nel_and_line_separators_round_trip_not_folded() {
        // U+0085 (NEL), U+2028, U+2029 are folded to spaces inside double quotes
        // unless escaped — silent value corruption at the terminal (NRN-118).
        for c in ['\u{85}', '\u{2028}', '\u{2029}'] {
            let s = format!("a{c}b");
            let result = serialize_value_preserving_style(&json!(s), ValueStyle::Plain).unwrap();
            assert_eq!(
                reparse(&result).as_deref(),
                Some(s.as_str()),
                "U+{:04X} did not round-trip (emitted {result:?})",
                c as u32
            );
        }
    }

    #[test]
    fn array_into_plain_scalar_style_returns_array_into_scalar_error() {
        let err = serialize_value_preserving_style(&json!([1, 2]), ValueStyle::Plain).unwrap_err();
        assert!(matches!(err, QuoteError::ArrayIntoScalar));
    }

    #[test]
    fn array_into_single_quoted_style_returns_array_into_scalar_error() {
        let err = serialize_value_preserving_style(&json!(["foo"]), ValueStyle::SingleQuoted)
            .unwrap_err();
        assert!(matches!(err, QuoteError::ArrayIntoScalar));
    }

    #[test]
    fn object_value_returns_non_scalar_error() {
        let err =
            serialize_value_preserving_style(&json!({"a": 1}), ValueStyle::Plain).unwrap_err();
        assert!(matches!(err, QuoteError::NonScalarValue));
    }

    #[test]
    fn scalar_into_block_sequence_style_returns_structured_error() {
        let err = serialize_value_preserving_style(&json!("anything"), ValueStyle::BlockSequence)
            .unwrap_err();
        assert!(matches!(err, QuoteError::StructuredOriginalStyle(_)));
    }

    #[test]
    fn serialize_array_block_style_emits_block_items() {
        let value = json!(["foo", "bar"]);
        let out = serialize_value_preserving_style(&value, ValueStyle::BlockSequence).unwrap();
        assert_eq!(out, "  - foo\n  - bar\n");
    }

    #[test]
    fn serialize_array_flow_style_emits_flow() {
        let value = json!(["foo", "bar"]);
        let out = serialize_value_preserving_style(&value, ValueStyle::FlowSequence).unwrap();
        assert!(out.starts_with('[') && out.ends_with(']'));
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn serialize_array_flow_empty_emits_empty_brackets() {
        let value = json!([]);
        let out = serialize_value_preserving_style(&value, ValueStyle::FlowSequence).unwrap();
        assert_eq!(out, "[]");
    }

    #[test]
    fn serialize_array_block_field_emits_key_and_indented_items() {
        let items = vec![json!("foo"), json!("bar")];
        let out = serialize_array_block_field("aliases", &items).unwrap();
        assert_eq!(out, "aliases:\n  - foo\n  - bar\n");
    }

    #[test]
    fn serialize_array_block_field_empty_emits_empty_flow_list() {
        // NRN-141: a bare `field:` line reads back as null; an explicit `[]`
        // round-trips as the empty list.
        let out = serialize_array_block_field("aliases", &[]).unwrap();
        assert_eq!(out, "aliases: []\n");
    }

    #[test]
    fn serialize_array_items_quote_strings_needing_quotes() {
        let value = json!(["a: b", "plain"]);
        let out = serialize_value_preserving_style(&value, ValueStyle::BlockSequence).unwrap();
        assert!(out.contains("'a: b'"));
        assert!(out.contains("plain"));
    }

    #[test]
    fn serialize_array_flow_with_numbers_and_bools() {
        let value = json!([42, true, null]);
        let out = serialize_value_preserving_style(&value, ValueStyle::FlowSequence).unwrap();
        assert_eq!(out, "[42, true, ~]");
    }

    #[test]
    fn render_key_round_trips_adversarial_names() {
        // Every adversarial field name must survive render_key → spliced into a
        // `{key}: x` KEY line → serde parse → a single mapping entry whose key is
        // byte-exactly the intended name. The serializer decides quoting by round-
        // tripping in a key template, not a character blacklist (NRN-142).
        let cases: Vec<&str> = vec![
            "#foo",
            "a: b",
            "a:b",
            "-foo",
            "?x",
            "[x]",
            "{x}",
            "a,b",
            "&anchor",
            "*alias",
            "!tag",
            "\"quoted\"",
            "'single'",
            "key with spaces",
            " leading",
            "trailing ",
            "",
            "123",
            "true",
            "false",
            "null",
            "~",
            "café ☕",
            "status",
            "title",
            "a_b_c",
        ];
        for name in &cases {
            let rendered = render_key(name);
            let doc = format!("{rendered}: x\n");
            let parsed: serde_yaml::Value = serde_yaml::from_str(&doc).unwrap_or_else(|e| {
                panic!("key line must be valid YAML for {name:?} (rendered {rendered:?}): {e}")
            });
            let map = parsed
                .as_mapping()
                .unwrap_or_else(|| panic!("expected mapping for {name:?}: {rendered:?}"));
            assert_eq!(map.len(), 1, "exactly one key for {name:?}: {rendered:?}");
            let (k, v) = map.iter().next().unwrap();
            assert_eq!(
                k.as_str(),
                Some(*name),
                "key must round-trip byte-exact for {name:?}: {rendered:?}"
            );
            assert_eq!(v.as_str(), Some("x"), "value for {name:?}: {rendered:?}");
        }
    }

    #[test]
    fn render_key_leaves_plain_identifiers_unquoted() {
        // Minimal churn: an identifier-plain key renders as the bare name (no
        // gratuitous quoting).
        for name in ["status", "title", "kind", "aliases", "a_b", "field123"] {
            assert_eq!(
                render_key(name),
                name,
                "plain identifier must render unquoted"
            );
        }
    }

    #[test]
    fn flow_sequence_items_round_trip_in_flow_context() {
        // Every adversarial item must survive serialize → embed as `key: [...]`
        // → serde parse → identical list. In a FLOW context the flow indicators
        // (`,` `[` `]` `{` `}`), `: `, and leading/trailing space are structural,
        // so an item that would be plain-safe in a block context must be quoted
        // here. The serializer decides this by round-tripping in a flow template,
        // not a character blacklist (NRN-141).
        let cases: Vec<&str> = vec![
            "a,b",
            "a]b",
            "a[b",
            "a: b",
            "a:b",
            "{x}",
            "}x{",
            "#x",
            " a",
            "a ",
            "\"q\"",
            "'q'",
            "a\nb",
            "*ref",
            "&anch",
            "!tag",
            "-",
            "?",
            "null",
            "true",
            "123",
            "",
            "café ☕",
            "plain",
            "hello world",
        ];
        let items: Vec<Value> = cases.iter().map(|s| json!(s)).collect();
        let rendered =
            serialize_value_preserving_style(&Value::Array(items), ValueStyle::FlowSequence)
                .unwrap();
        let doc = format!("key: {rendered}");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&doc)
            .unwrap_or_else(|e| panic!("rendered flow must be valid YAML: {rendered:?}: {e}"));
        let seq = parsed
            .get("key")
            .and_then(|v| v.as_sequence())
            .unwrap_or_else(|| panic!("expected a sequence, got: {rendered:?}"));
        let got: Vec<String> = seq
            .iter()
            .map(|v| v.as_str().expect("string item").to_string())
            .collect();
        let expected: Vec<String> = cases.iter().map(|s| s.to_string()).collect();
        assert_eq!(got, expected, "rendered: {rendered:?}");
    }
}
