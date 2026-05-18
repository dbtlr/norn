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
}

/// Returns the YAML bytes that replace the `value_range` of an original property
/// when applying a `set_frontmatter` change. Preserves quote style when the new
/// value can be represented in the original style; upgrades to a stricter style
/// otherwise. Never downgrades.
///
/// Currently supports only scalar string, number, boolean, and null values
/// (matching repair's `set_frontmatter` action shape). Returns
/// `QuoteError::NonScalarValue` for arrays and objects.
pub fn serialize_value_preserving_style(
    new_value: &Value,
    original_style: ValueStyle,
) -> Result<String, QuoteError> {
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
    fn array_value_returns_non_scalar_error() {
        let err =
            serialize_value_preserving_style(&json!([1, 2]), ValueStyle::Plain).unwrap_err();
        assert!(matches!(err, QuoteError::NonScalarValue));
    }

    #[test]
    fn object_value_returns_non_scalar_error() {
        let err = serialize_value_preserving_style(&json!({"a": 1}), ValueStyle::Plain)
            .unwrap_err();
        assert!(matches!(err, QuoteError::NonScalarValue));
    }

    #[test]
    fn block_sequence_original_style_returns_structured_error() {
        let err = serialize_value_preserving_style(&json!("anything"), ValueStyle::BlockSequence)
            .unwrap_err();
        assert!(matches!(err, QuoteError::StructuredOriginalStyle(_)));
    }
}
