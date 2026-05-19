//! Predicate set for `Cache::documents_matching` and the JSON-path encoder
//! that translates frontmatter field names into safe SQLite JSON paths.

use serde_json::Value;

/// Predicate set for `Cache::documents_matching` and `Cache::find_documents`.
/// ANY-of within `path_globs` and within each `frontmatter_in` value list;
/// ALL-of across all flag-fields and across vectors.
#[derive(Default, Debug, Clone)]
pub struct DocumentQuery {
    /// Path glob patterns in `vault_graph::pattern_matches_path` syntax.
    /// ANY-of. Empty = no path narrowing. Applied as a Rust post-pass.
    pub path_globs: Vec<String>,
    /// Frontmatter equality predicates `(field, value)`. ALL-of.
    pub frontmatter_eq: Vec<(String, Value)>,
    /// Required-present fields. ALL-of. Match v1 filter_documents semantics
    /// for null-vs-missing — verified via round-trip property tests.
    pub frontmatter_has: Vec<String>,
    /// Required-absent fields. ALL-of.
    pub frontmatter_missing: Vec<String>,
    /// `(field, allowed_values)` — frontmatter field is one of the values
    /// (ANY-of within each entry; ALL-of across entries).
    pub frontmatter_in: Vec<(String, Vec<Value>)>,
    /// `(field, disallowed_values)` — frontmatter field is NOT one of the values.
    pub frontmatter_not_in: Vec<(String, Vec<Value>)>,
    /// `(field, date_string)` — `field` < `date_string` (lexical, ISO 8601).
    pub date_before: Vec<(String, String)>,
    /// `(field, date_string)` — `field` > `date_string`.
    pub date_after: Vec<(String, String)>,
    /// `(field, date_string)` — `field` = `date_string`.
    pub date_on: Vec<(String, String)>,
    /// Body-text substring; case-insensitive. v1: SQL LIKE. v4: FTS5.
    pub body_text_contains: Option<String>,
}

/// Encode a frontmatter field name as a single quoted JSON-path segment for
/// SQLite's `json_extract`. Returns the full path string `$."<escaped>"`.
///
/// SQLite parses the path at statement execution; binding this as a parameter
/// (not interpolating it) is what closes the SQL-injection vector and lets
/// frontmatter keys contain any character.
pub fn json_path_for(field: &str) -> String {
    let escaped = field.replace('\\', r"\\").replace('"', r#"\""#);
    format!(r#"$."{}""#, escaped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_field() {
        assert_eq!(json_path_for("type"), r#"$."type""#);
    }

    #[test]
    fn hyphenated_field() {
        assert_eq!(json_path_for("created-at"), r#"$."created-at""#);
    }

    #[test]
    fn dotted_field() {
        // Keys with dots are flat keys (single quoted segment), not nested paths.
        assert_eq!(json_path_for("schema.version"), r#"$."schema.version""#);
    }

    #[test]
    fn embedded_quote_is_escaped() {
        assert_eq!(json_path_for(r#"a"b"#), r#"$."a\"b""#);
    }

    #[test]
    fn embedded_backslash_is_escaped() {
        assert_eq!(json_path_for(r"a\b"), r#"$."a\\b""#);
    }
}
