//! `--col` projection primitives for the read commands, ported from the donor
//! `src/output/projection.rs` (retired tree).
//!
//! Parses `--col` tokens into structural facets (dot-prefixed) and frontmatter
//! field names (bare), projects a frontmatter object to named fields, and
//! renders a JSON value as a concise display string.
//!
//! Deep facets (`.headings`, `.outgoing_links`, `.unresolved_links`,
//! `.incoming_links`) are recognized here (so they never warn as unknown) but
//! their per-doc display data is not yet carried on the flat wire projection —
//! the deep-fetch port lands them. Until then they render empty.

use serde_json::Value;
use std::io::Write;

/// The structural facets addressable via `--col` (dot-prefixed; dot stripped
/// here). Bare `--col` names are frontmatter field names instead.
pub const KNOWN_FACETS: &[&str] = &[
    "path",
    "stem",
    "frontmatter",
    "headings",
    "outgoing_links",
    "unresolved_links",
    "incoming_links",
    "body",
    "document_hash",
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
/// Empty `fields` returns the whole frontmatter (cloned, or `Null` when absent).
/// A non-object frontmatter returns an empty object. Absent named fields are
/// silently dropped (the warn path flags them).
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

/// The "unknown `--col` facet" warning message body (no severity prefix).
pub fn unknown_facet_message(facet: &str) -> String {
    let valid = KNOWN_FACETS
        .iter()
        .map(|f| format!(".{f}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("unknown --col facet '.{facet}' (valid facets: {valid}; bare names select frontmatter fields)")
}

/// Flatten a frontmatter object into `key: value` lines (records `.frontmatter`).
pub fn frontmatter_to_display(fm: &Value) -> String {
    match fm {
        Value::Object(obj) => obj
            .iter()
            .map(|(k, v)| format!("{}: {}", k, json_value_inline(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Warn (once) that `--col` has no effect with a format that ignores it.
pub fn warn_col_ignored(
    cols: &[String],
    inert_format: Option<&str>,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    if let Some(fmt) = inert_format {
        if !cols.is_empty() {
            writeln!(stderr, "warning: --col is ignored with --format {fmt}")?;
        }
    }
    Ok(())
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
    fn json_value_inline_renders_scalars_and_arrays() {
        assert_eq!(json_value_inline(&json!("hi")), "hi");
        assert_eq!(json_value_inline(&json!(42)), "42");
        assert_eq!(json_value_inline(&json!(["a", "b"])), "a, b");
    }
}
