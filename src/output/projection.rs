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

use camino::Utf8Path;
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
    "raw",
    "document_hash",
];

/// Read a document's source file verbatim from disk. `rel_path` is the
/// cache-stored path (relative to the vault root). Returns None if unreadable
/// (treated as an absent facet). `.raw` is read at query time from disk — in a
/// row mixing cache-served facets with `.raw`, the cache fields and `.raw` are
/// momentarily inconsistent if the file changed since the last cache build; the
/// pre-query cache refresh normally closes this (same assumption every read makes).
pub fn read_raw(vault_root: &Utf8Path, rel_path: &Utf8Path) -> Option<String> {
    std::fs::read_to_string(vault_root.join(rel_path)).ok()
}

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

/// Build the standard "unknown `--col` facet" warning message body (no severity
/// prefix; callers prepend their own `warn:`/`warning:` style). Single-sourced
/// so `find` and `get` can't drift on wording.
pub fn unknown_facet_message(facet: &str) -> String {
    let valid = KNOWN_FACETS
        .iter()
        .map(|f| format!(".{f}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("unknown --col facet '.{facet}' (valid facets: {valid}; bare names select frontmatter fields)")
}

// ---------------------------------------------------------------------------
// Per-facet display helpers (shared records-format leaf rendering)
// ---------------------------------------------------------------------------

/// Flatten frontmatter into `key: value\nkey: value\n…` lines.
///
/// For a JSON object, each key-value pair is one `key: value` line where the
/// value is displayed as its natural string form. For non-object JSON (rare),
/// falls back to the raw JSON string.
pub fn frontmatter_to_display(fm: &Value) -> String {
    match fm {
        Value::Object(obj) => {
            let lines: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{}: {}", k, json_value_inline(v)))
                .collect();
            lines.join("\n")
        }
        other => other.to_string(),
    }
}

/// Render headings as `#`-prefixed lines, one per heading.
pub fn headings_to_display(headings: &[crate::core::Heading]) -> String {
    headings
        .iter()
        .map(|h| {
            let prefix = "#".repeat(h.level as usize);
            format!("{} {}", prefix, h.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render outgoing (resolved) links: `target → resolved_path`.
pub fn outgoing_links_to_display(links: &[crate::core::Link]) -> String {
    links
        .iter()
        .map(|l| {
            if let Some(resolved) = &l.resolved_path {
                format!("{}  →  {}", l.target, resolved)
            } else {
                l.target.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render unresolved links: `target  (unresolved: reason)`.
pub fn unresolved_links_to_display(links: &[crate::core::Link]) -> String {
    links
        .iter()
        .map(|l| {
            let reason = l
                .unresolved_reason
                .as_ref()
                .map(|r| match r {
                    crate::core::UnresolvedReason::TargetMissing => "target-missing",
                    crate::core::UnresolvedReason::AnchorMissing => "anchor-missing",
                    crate::core::UnresolvedReason::BlockRefMissing => "block-ref-missing",
                    crate::core::UnresolvedReason::Ambiguous => "ambiguous",
                })
                .unwrap_or("unknown");
            format!("{}  (unresolved: {})", l.target, reason)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render incoming links: `source_path  raw_link_text`.
pub fn incoming_links_to_display(links: &[crate::cache::IncomingLink]) -> String {
    links
        .iter()
        .map(|il| format!("{}  {}", il.source_path, il.link.raw))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Warn (once) that `--col` has no effect with a format that ignores it.
/// `inert_format` is `Some("paths")`/`Some("markdown")` when the active format
/// disregards `--col` (the identity-only / whole-document formats), `None`
/// otherwise. Shared by both read commands so the message stays single-sourced
/// (the format enums differ per command, so the caller maps to the name).
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

/// Warn (once) that `get --section` has no effect with a format that ignores
/// it (`paths`/`markdown`) — `--section`'s sibling to [`warn_col_ignored`].
/// A separate function (rather than generalizing `warn_col_ignored`) because
/// `--section` is `get`-only; `find` never calls this.
pub fn warn_section_ignored(
    sections: &[String],
    inert_format: Option<&str>,
    stderr: &mut dyn Write,
) -> std::io::Result<()> {
    if let Some(fmt) = inert_format {
        if !sections.is_empty() {
            writeln!(stderr, "warning: --section is ignored with --format {fmt}")?;
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
