//! Value canonicalization shared between the `document_fields` EAV writer
//! and (from the query-routing task on) the query layer's stored-value
//! comparisons. Both sides must agree byte-for-byte with
//! `query_documents.rs`'s SQL-side wikilink-bracket collapse
//! (`replace(replace(x, '[[', ''), ']]', '')`) and `json_value_to_sql`'s
//! scalar type mapping — that parity is a decided invariant, not a
//! convenience.

use rusqlite::types::Value as SqlValue;

use crate::cache::query_documents::json_value_to_sql;

/// Collapse Obsidian-style `[[...]]` wikilink brackets by removing every
/// occurrence of `[[` and `]]` anywhere in the string. This is an
/// unconditional remove-all (not a balanced-pair strip), matching the SQL
/// `replace(replace(x, '[[', ''), ']]', '')` form the query layer's
/// stored-side comparisons use.
pub(crate) fn strip_wikilink_brackets(s: &str) -> String {
    s.replace("[[", "").replace("]]", "")
}

/// Canonicalize one non-array, non-null frontmatter scalar the way the
/// query layer treats stored values: strings get wikilink brackets
/// collapsed; every other JSON scalar goes through `json_value_to_sql`
/// (INTEGER for integers, REAL for floats, TEXT for JSON-encoded
/// objects/arrays, etc — see that function's docs for the exact mapping).
pub(crate) fn canonicalize_scalar(v: &serde_json::Value) -> SqlValue {
    match v {
        serde_json::Value::String(s) => SqlValue::Text(strip_wikilink_brackets(s)),
        other => json_value_to_sql(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn strip_wikilink_brackets_matches_sql_replace_all_form() {
        let conn = Connection::open_in_memory().unwrap();
        let cases = [
            "plain",
            "[[norn]]",
            "[[a]] and [[b]]",
            "[[[nested]]]",
            "no brackets at all",
            "[[",
            "]]",
            "[[]]",
            "",
            "a[[b]]c[[d]]e",
            "[[[[double-open",
        ];
        for case in cases {
            let sql_result: String = conn
                .query_row(
                    "SELECT replace(replace(?, '[[', ''), ']]', '')",
                    [case],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                strip_wikilink_brackets(case),
                sql_result,
                "mismatch for input {case:?}"
            );
        }
    }

    #[test]
    fn canonicalize_scalar_strips_brackets_from_strings() {
        match canonicalize_scalar(&serde_json::json!("[[norn]]")) {
            SqlValue::Text(s) => assert_eq!(s, "norn"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn canonicalize_scalar_plain_string_is_unchanged() {
        match canonicalize_scalar(&serde_json::json!("plain")) {
            SqlValue::Text(s) => assert_eq!(s, "plain"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn canonicalize_scalar_integer_round_trips_as_integer() {
        assert!(matches!(
            canonicalize_scalar(&serde_json::json!(42)),
            SqlValue::Integer(42)
        ));
    }

    #[test]
    fn canonicalize_scalar_float_round_trips_as_real() {
        match canonicalize_scalar(&serde_json::json!(2.5)) {
            SqlValue::Real(f) => assert_eq!(f, 2.5),
            other => panic!("expected Real, got {other:?}"),
        }
    }

    #[test]
    fn canonicalize_scalar_bool_round_trips_as_integer() {
        assert!(matches!(
            canonicalize_scalar(&serde_json::json!(true)),
            SqlValue::Integer(1)
        ));
        assert!(matches!(
            canonicalize_scalar(&serde_json::json!(false)),
            SqlValue::Integer(0)
        ));
    }

    #[test]
    fn canonicalize_scalar_object_is_json_encoded_text() {
        match canonicalize_scalar(&serde_json::json!({"a": 1})) {
            SqlValue::Text(s) => assert_eq!(s, r#"{"a":1}"#),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
