//! Shared `--col` projection primitives for the read commands (`find` / `get`).
//!
//! Both commands select documents differently (predicate vs. identity) but
//! share one downstream output contract. This module is the leaf layer of that
//! contract: parsing `--col` tokens into facets + frontmatter fields, projecting
//! a frontmatter object down to named fields, and rendering a JSON value as a
//! concise display string. The command-specific renderers compose from these.
//!
//! The structural facets are addressed in `--col` with a leading dot (`.body`,
//! `.headings`, …); bare names are frontmatter field names (matching `find`).
//! The dot distinguishes the fixed structural facets so a frontmatter key named
//! e.g. `body` never collides with the `.body` facet.

use serde_json::Value;

/// The structural facets addressable via `--col` (dot-prefixed; dot stripped
/// here). Bare `--col` names are frontmatter field names instead.
pub const KNOWN_FACETS: &[&str] = &[
    "path",
    "frontmatter",
    "headings",
    "outgoing_links",
    "unresolved_links",
    "incoming_links",
    "body",
];

/// Partition `--col` tokens into structural facets (dot-prefixed, dot stripped)
/// and frontmatter field names (bare).
pub fn split_cols(cols: &[String]) -> (Vec<String>, Vec<String>) {
    let mut facets = Vec::new();
    let mut fields = Vec::new();
    for col in cols {
        match col.strip_prefix('.') {
            Some(facet) => facets.push(facet.to_string()),
            None => fields.push(col.clone()),
        }
    }
    (facets, fields)
}

/// Project a frontmatter object down to the named fields.
///
/// Empty `fields` returns the whole frontmatter (cloned, or `Null` when absent)
/// — the "dump everything" default. A non-object frontmatter returns an empty
/// object. Absent named fields are silently dropped (the warn path flags them).
pub fn filter_frontmatter(fm: Option<&Value>, fields: &[String]) -> Value {
    if fields.is_empty() {
        return fm.cloned().unwrap_or(Value::Null);
    }
    let Some(Value::Object(obj)) = fm else {
        return Value::Object(serde_json::Map::new());
    };
    let mut filtered = serde_json::Map::new();
    for field in fields {
        if let Some(v) = obj.get(field) {
            filtered.insert(field.clone(), v.clone());
        }
    }
    Value::Object(filtered)
}

/// Format a JSON value as a concise single-line string for display.
///
/// Strings render bare; arrays join with `, `; objects fall back to their JSON
/// form. Used for both per-field record rows and the consolidated frontmatter
/// block.
pub fn json_value_inline(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(arr) => arr
            .iter()
            .map(json_value_inline)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_cols_partitions_dot_facets_from_bare_fields() {
        let cols = vec![
            ".body".to_string(),
            "status".to_string(),
            ".headings".to_string(),
            "title".to_string(),
        ];
        let (facets, fields) = split_cols(&cols);
        assert_eq!(facets, vec!["body", "headings"]);
        assert_eq!(fields, vec!["status", "title"]);
    }

    #[test]
    fn filter_frontmatter_empty_fields_returns_whole_block() {
        let fm = json!({"type": "note", "status": "active"});
        assert_eq!(filter_frontmatter(Some(&fm), &[]), fm);
    }

    #[test]
    fn filter_frontmatter_absent_returns_null_when_no_fields() {
        assert_eq!(filter_frontmatter(None, &[]), Value::Null);
    }

    #[test]
    fn filter_frontmatter_narrows_to_named_fields() {
        let fm = json!({"type": "note", "status": "active", "title": "x"});
        let filtered = filter_frontmatter(Some(&fm), &["status".to_string()]);
        assert_eq!(filtered, json!({"status": "active"}));
    }

    #[test]
    fn filter_frontmatter_non_object_with_fields_is_empty_object() {
        let fm = json!("scalar");
        assert_eq!(
            filter_frontmatter(Some(&fm), &["status".to_string()]),
            json!({})
        );
    }

    #[test]
    fn json_value_inline_renders_scalars_and_arrays() {
        assert_eq!(json_value_inline(&json!("hi")), "hi");
        assert_eq!(json_value_inline(&json!(42)), "42");
        assert_eq!(json_value_inline(&json!(true)), "true");
        assert_eq!(json_value_inline(&json!(["a", "b"])), "a, b");
    }
}
