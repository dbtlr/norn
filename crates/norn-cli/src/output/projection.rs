//! `--col` projection primitives for the read commands, ported from the donor
//! `src/output/projection.rs` (retired tree).
//!
//! Parses `--col` tokens into structural facets (dot-prefixed) and frontmatter
//! field names (bare), projects a frontmatter object to named fields, and
//! renders a JSON value as a concise display string.
//!
//! Deep facets (`.headings`, `.outgoing_links`, `.unresolved_links`,
//! `.incoming_links`) are carried on the wire as pre-serialized JSON values (the
//! cache's own `Heading` / `Link` / `IncomingLink` serialization). The JSON
//! output emits them verbatim; the records renderer folds them to the donor's
//! one-line display strings via [`headings_to_display`] and the link helpers.

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

/// Render serialized `Heading` values as `# text` lines, one per heading
/// (donor `headings_to_display`). Each value is `{ level, text, ... }`; a
/// missing/negative level renders no `#` prefix.
pub fn headings_to_display(headings: &[Value]) -> String {
    headings
        .iter()
        .map(|h| {
            let level = h.get("level").and_then(Value::as_u64).unwrap_or(0) as usize;
            let text = h.get("text").and_then(Value::as_str).unwrap_or("");
            format!("{} {}", "#".repeat(level), text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render serialized resolved `Link` values as `target  →  resolved` (or bare
/// `target` when unresolved) — donor `outgoing_links_to_display`.
pub fn outgoing_links_to_display(links: &[Value]) -> String {
    links
        .iter()
        .map(|l| {
            let target = l.get("target").and_then(Value::as_str).unwrap_or("");
            match l.get("resolved_path").and_then(Value::as_str) {
                Some(resolved) => format!("{target}  →  {resolved}"),
                None => target.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render serialized unresolved `Link` values as `target  (unresolved: reason)`
/// (donor `unresolved_links_to_display`). The reason is the kebab-case
/// `unresolved_reason` serialization the cache already emits.
pub fn unresolved_links_to_display(links: &[Value]) -> String {
    links
        .iter()
        .map(|l| {
            let target = l.get("target").and_then(Value::as_str).unwrap_or("");
            let reason = l
                .get("unresolved_reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!("{target}  (unresolved: {reason})")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render serialized `IncomingLink` values (`{ source_path, link }`) as
/// `source_path  raw` (donor `incoming_links_to_display`).
pub fn incoming_links_to_display(links: &[Value]) -> String {
    links
        .iter()
        .map(|il| {
            let source = il.get("source_path").and_then(Value::as_str).unwrap_or("");
            let raw = il
                .get("link")
                .and_then(|l| l.get("raw"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("{source}  {raw}")
        })
        .collect::<Vec<_>>()
        .join("\n")
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
