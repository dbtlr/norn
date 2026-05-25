use crate::offsets::ValueStyle;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum QuoteError {
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
        Value::String(s) => Ok(serialize_string_value(s, original_style)),
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
                out.push_str(&render_array_item(item)?);
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

/// Serialize an array as block-style YAML items for a brand-new field.
///
/// Output is the items portion only: `  - item1\n  - item2\n` (2-space
/// indent, trailing newline). Each item is quoted per scalar rules.
/// The caller emits `field:\n` before this output.
///
/// An empty array emits an empty string (the caller still emits `field:\n`).
pub fn serialize_array_block_for_new_field(items: &[Value]) -> Result<String, QuoteError> {
    serialize_array_block_items(items)
}

fn serialize_array_block_items(items: &[Value]) -> Result<String, QuoteError> {
    let mut out = String::new();
    for item in items {
        let rendered = render_array_item(item)?;
        out.push_str("  - ");
        out.push_str(&rendered);
        out.push('\n');
    }
    Ok(out)
}

/// Render a single array item as a YAML scalar string.
///
/// Strings are rendered via plain-style quoting (upgraded when necessary).
/// Numbers, booleans, and null are rendered without quotes.
/// Objects produce `QuoteError::NonScalarValue`.
fn render_array_item(item: &Value) -> Result<String, QuoteError> {
    match item {
        Value::String(s) => Ok(serialize_string_value(s, ValueStyle::Plain)),
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

fn serialize_string_value(s: &str, original_style: ValueStyle) -> String {
    let plain_safe = is_plain_safe(s);

    match original_style {
        ValueStyle::Plain | ValueStyle::EmptyValue => {
            if plain_safe {
                s.to_string()
            } else if !s.contains('\'') {
                format!("'{s}'")
            } else {
                format!("\"{}\"", escape_double_quoted(s))
            }
        }
        ValueStyle::SingleQuoted => {
            let escaped = s.replace('\'', "''");
            format!("'{escaped}'")
        }
        ValueStyle::DoubleQuoted => {
            format!("\"{}\"", escape_double_quoted(s))
        }
        _ => unreachable!(),
    }
}

fn escape_double_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
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
    fn serialize_array_block_for_new_field_emits_indented_items() {
        let items = vec![json!("foo"), json!("bar")];
        let out = serialize_array_block_for_new_field(&items).unwrap();
        assert_eq!(out, "  - foo\n  - bar\n");
    }

    #[test]
    fn serialize_array_block_for_new_field_empty_emits_empty_string() {
        let out = serialize_array_block_for_new_field(&[]).unwrap();
        assert_eq!(out, "");
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
}
