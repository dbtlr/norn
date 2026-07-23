//! `--col` projection primitives for the read commands.
//!
//! Parses `--col` tokens into structural facets (dot-prefixed) and frontmatter
//! field names (bare), projects a frontmatter object to named fields, and
//! renders a JSON value as a concise display string.
//!
//! Deep facets (`.headings`, `.outgoing_links`, `.unresolved_links`,
//! `.incoming_links`) are carried on the wire as pre-serialized JSON values (the
//! cache's own `Heading` / `Link` / `IncomingLink` serialization). The JSON
//! output emits them verbatim; the records renderer folds them to one-line
//! display strings via [`headings_to_display`] and the link helpers.

use serde_json::{Map, Value};
use std::collections::HashSet;

use crate::display::Conversation;

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
///
/// When the typo is a near-miss of a real facet, lead with a did-you-mean via
/// the shared `closest` heuristic (NRN-361) before the full valid-facets list —
/// the same soft-landing the dynamic-field gate gives a mistyped predicate.
pub fn unknown_facet_message(facet: &str) -> String {
    let valid = KNOWN_FACETS
        .iter()
        .map(|f| format!(".{f}"))
        .collect::<Vec<_>>()
        .join(", ");
    let candidates: Vec<String> = KNOWN_FACETS.iter().map(|f| f.to_string()).collect();
    let suggestion = match norn_core::grammar::closest(facet, &candidates) {
        Some(s) => format!(" — did you mean '.{s}'?"),
        None => String::new(),
    };
    format!("unknown --col facet '.{facet}'{suggestion} (valid facets: {valid}; bare names select frontmatter fields)")
}

/// Render serialized `Heading` values as `# text` lines, one per heading.
/// Each value is `{ level, text, ... }`; a missing/negative level renders no
/// `#` prefix.
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
/// `target` when unresolved).
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

/// Render serialized unresolved `Link` values as `target  (unresolved: reason)`.
/// The reason is the kebab-case
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
/// `source_path  raw`.
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
    conv: &mut Conversation<'_>,
) -> std::io::Result<()> {
    if let Some(fmt) = inert_format {
        if !cols.is_empty() {
            conv.warning(&format!("--col is ignored with --format {fmt}"))?;
        }
    }
    Ok(())
}

/// Warn (once) that `--section` has no effect with a format that ignores it
/// (`paths` / `markdown`).
pub fn warn_section_ignored(
    sections: &[String],
    inert_format: Option<&str>,
    conv: &mut Conversation<'_>,
) -> std::io::Result<()> {
    if let Some(fmt) = inert_format {
        if !sections.is_empty() {
            conv.warning(&format!("--section is ignored with --format {fmt}"))?;
        }
    }
    Ok(())
}

/// Build the `sections` JSON value: a plain object keyed by heading text
/// (`{heading: content, ...}`). Keys land in a
/// `serde_json::Map` (sorted), so the object is alphabetically keyed regardless
/// of the request order the records renderer preserves.
pub fn sections_to_json_object(sections: &[(String, String)]) -> Value {
    let mut obj = serde_json::Map::with_capacity(sections.len());
    for (heading, content) in sections {
        obj.insert(heading.clone(), Value::String(content.clone()));
    }
    Value::Object(obj)
}

/// A read verb's document view for `--col` projection — the shared shape `find`
/// and `get` project through, so the split/facet-folding ladder exists once
/// (NRN-370). Both verbs build a `DocView` over their wire record and hand it to
/// [`project_json`] / [`project_pairs`]; the per-verb differences (find's
/// always-present body vs get's optional body, get's `--section` spans) ride on
/// the view fields, not on forked projection code.
pub struct DocView<'a> {
    pub path: &'a str,
    pub stem: &'a str,
    pub hash: &'a str,
    pub frontmatter: Option<&'a Value>,
    pub headings: &'a [Value],
    pub outgoing_links: &'a [Value],
    pub unresolved_links: &'a [Value],
    pub incoming_links: &'a [Value],
    /// The body text. `find` always carries it (its wire record's body is a plain
    /// `String`); `get` carries it only when the owner returned one. `None` renders
    /// as JSON `null` under `--col .body`; `Some("")` renders as an empty string.
    pub body: Option<&'a str>,
    /// Resolved `--section` spans (get only), request order. `None` for `find`.
    pub sections: Option<&'a [(String, String)]>,
}

/// The no-`--col` default projection: `find` shows `path` + frontmatter only;
/// `get` shows the full facet set (frontmatter + headings + the three link sets +
/// body-if-carried). This is the one place the two verbs' defaults genuinely
/// differ; the `--col` ladder below is shared verbatim.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DefaultCols {
    /// `find`: `{ path, frontmatter }` only.
    FrontmatterOnly,
    /// `get`: frontmatter + headings + the three link sets + body-if-carried.
    FullFacets,
}

/// Project a document view to its `--col` JSON object (the shared
/// `doc_to_json` / `record_to_json`). `path` is always present; the no-`--col`
/// default is chosen by `default`; the explicit-`--col` ladder is verb-neutral.
/// `all_cols` is `find`'s `--all-cols` (get always passes `false` — its
/// full-dump default is the no-`--col` `FullFacets` branch).
pub fn project_json(
    view: &DocView<'_>,
    cols: &[String],
    all_cols: bool,
    default: DefaultCols,
) -> Value {
    let mut map = Map::new();
    map.insert("path".into(), Value::String(view.path.to_string()));

    if cols.is_empty() && !all_cols {
        match default {
            DefaultCols::FrontmatterOnly => {
                map.insert(
                    "frontmatter".into(),
                    view.frontmatter.cloned().unwrap_or(Value::Null),
                );
            }
            DefaultCols::FullFacets => {
                map.insert(
                    "frontmatter".into(),
                    view.frontmatter.cloned().unwrap_or(Value::Null),
                );
                map.insert("headings".into(), Value::Array(view.headings.to_vec()));
                map.insert(
                    "outgoing_links".into(),
                    Value::Array(view.outgoing_links.to_vec()),
                );
                map.insert(
                    "unresolved_links".into(),
                    Value::Array(view.unresolved_links.to_vec()),
                );
                map.insert(
                    "incoming_links".into(),
                    Value::Array(view.incoming_links.to_vec()),
                );
                // Body appears only when the owner carried it (`--all-cols` /
                // `.body`); hash/stem never appear in the no-`--col` dump.
                if let Some(body) = view.body {
                    map.insert("body".into(), Value::String(body.to_string()));
                }
            }
        }
        insert_sections_json(&mut map, view);
        return Value::Object(map);
    }

    let (facets, fields) = split_cols(cols);
    let allow: HashSet<&str> = facets.iter().map(String::as_str).collect();

    if allow.contains("stem") {
        map.insert("stem".into(), Value::String(view.stem.to_string()));
    }
    if allow.contains("document_hash") && !view.hash.is_empty() {
        map.insert("document_hash".into(), Value::String(view.hash.to_string()));
    }
    if all_cols || allow.contains("frontmatter") {
        map.insert(
            "frontmatter".into(),
            view.frontmatter.cloned().unwrap_or(Value::Null),
        );
    } else if !fields.is_empty() {
        map.insert(
            "frontmatter".into(),
            filter_frontmatter(view.frontmatter, &fields),
        );
    }
    // Deep facets: emit the pre-serialized values verbatim, populated only when
    // connections were loaded (`--all-cols` or a deep `--col` facet).
    for (facet, values) in [
        ("headings", view.headings),
        ("outgoing_links", view.outgoing_links),
        ("unresolved_links", view.unresolved_links),
        ("incoming_links", view.incoming_links),
    ] {
        if all_cols || allow.contains(facet) {
            map.insert(facet.into(), Value::Array(values.to_vec()));
        }
    }
    if all_cols || allow.contains("body") {
        map.insert(
            "body".into(),
            view.body
                .map(|b| Value::String(b.to_string()))
                .unwrap_or(Value::Null),
        );
    }
    insert_sections_json(&mut map, view);
    Value::Object(map)
}

/// `--section` is orthogonal to `--col`/`--all-cols`: inserted whenever the view
/// carries it (get only; `find`'s view has `None`).
fn insert_sections_json(map: &mut Map<String, Value>, view: &DocView<'_>) {
    if let Some(sections) = view.sections {
        map.insert("sections".into(), sections_to_json_object(sections));
    }
}

/// Project a document view to its ordered `(label, value)` record rows (the
/// shared `build_record_pairs` / `build_text_fields`). The no-`--col` &&
/// `!all_cols` case is `find`'s frontmatter-fields-only default; `get` passes
/// `all_cols = cols.is_empty()`, so its no-`--col` case runs the ladder's
/// `all_cols` branch (full facets) and never reaches the frontmatter-only path.
pub fn project_pairs(view: &DocView<'_>, cols: &[String], all_cols: bool) -> Vec<(String, String)> {
    let fm_object = view.frontmatter.and_then(Value::as_object);

    if cols.is_empty() && !all_cols {
        let mut pairs = Vec::new();
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                pairs.push((key.clone(), json_value_inline(value)));
            }
        }
        return pairs;
    }

    let (facets, fields) = split_cols(cols);
    let facet_set: HashSet<&str> = facets.iter().map(String::as_str).collect();
    let mut pairs = Vec::new();

    if facet_set.contains("stem") {
        pairs.push(("stem".into(), view.stem.to_string()));
    }
    if facet_set.contains("document_hash") && !view.hash.is_empty() {
        pairs.push(("document_hash".into(), view.hash.to_string()));
    }
    // `--all-cols` and bare `--col` fields are mutually exclusive at the grammar
    // (`overrides_with`), so at most one of these two blocks contributes.
    if all_cols {
        if let Some(obj) = fm_object {
            for (key, value) in obj {
                pairs.push((key.clone(), json_value_inline(value)));
            }
        }
    }
    for field in &fields {
        if let Some(value) = fm_object.and_then(|obj| obj.get(field)) {
            pairs.push((field.clone(), json_value_inline(value)));
        }
    }
    if facet_set.contains("frontmatter") {
        if let Some(fm) = view.frontmatter {
            let value = frontmatter_to_display(fm);
            if !value.is_empty() {
                pairs.push(("frontmatter".into(), value));
            }
        }
    }
    if (all_cols || facet_set.contains("headings")) && !view.headings.is_empty() {
        pairs.push(("headings".into(), headings_to_display(view.headings)));
    }
    if (all_cols || facet_set.contains("outgoing_links")) && !view.outgoing_links.is_empty() {
        pairs.push((
            "outgoing_links".into(),
            outgoing_links_to_display(view.outgoing_links),
        ));
    }
    if (all_cols || facet_set.contains("unresolved_links")) && !view.unresolved_links.is_empty() {
        pairs.push((
            "unresolved_links".into(),
            unresolved_links_to_display(view.unresolved_links),
        ));
    }
    if (all_cols || facet_set.contains("incoming_links")) && !view.incoming_links.is_empty() {
        pairs.push((
            "incoming_links".into(),
            incoming_links_to_display(view.incoming_links),
        ));
    }
    if all_cols || facet_set.contains("body") {
        if let Some(body) = view.body {
            let trimmed = body.trim();
            if !trimmed.is_empty() {
                pairs.push(("body".into(), trimmed.to_string()));
            }
        }
    }
    // `--section` (get only): one labeled block per requested heading, request
    // order, the verbatim span (identical to `--format json`).
    if let Some(sections) = view.sections {
        for (heading, content) in sections {
            pairs.push((heading.clone(), content.clone()));
        }
    }
    pairs
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

    #[test]
    fn unknown_facet_near_miss_gets_a_did_you_mean() {
        // A one-edit typo of a real facet leads with the suggestion (NRN-361).
        let msg = unknown_facet_message("healings");
        assert!(
            msg.contains("did you mean '.headings'?"),
            "expected a did-you-mean for a near-miss, got: {msg}"
        );
        assert!(msg.contains("valid facets:"), "still lists valid facets");
    }

    #[test]
    fn unknown_facet_far_miss_omits_the_did_you_mean() {
        // A token nowhere near any facet gets the plain message — no dash, no
        // spurious suggestion.
        let msg = unknown_facet_message("zzzzzzzz");
        assert!(
            !msg.contains("did you mean"),
            "a far miss must not invent a suggestion, got: {msg}"
        );
        assert_eq!(
            msg,
            "unknown --col facet '.zzzzzzzz' (valid facets: .path, .stem, .frontmatter, \
             .headings, .outgoing_links, .unresolved_links, .incoming_links, .body, \
             .document_hash; bare names select frontmatter fields)"
        );
    }

    // ── The unified projection (NRN-370): find via FrontmatterOnly, get via
    //    FullFacets — the ladder shared, the default per verb. ──────────────

    fn view<'a>(
        fm: Option<&'a Value>,
        headings: &'a [Value],
        outgoing: &'a [Value],
        body: Option<&'a str>,
    ) -> DocView<'a> {
        DocView {
            path: "a.md",
            stem: "a",
            hash: "deadbeef",
            frontmatter: fm,
            headings,
            outgoing_links: outgoing,
            unresolved_links: &[],
            incoming_links: &[],
            body,
            sections: None,
        }
    }

    #[test]
    fn find_json_no_col_is_path_and_frontmatter_sorted() {
        let fm = json!({"title": "A", "type": "note"});
        let v = project_json(
            &view(Some(&fm), &[], &[], Some("body")),
            &[],
            false,
            DefaultCols::FrontmatterOnly,
        );
        // serde_json Value is a sorted map: frontmatter, path (body excluded).
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"frontmatter":{"title":"A","type":"note"},"path":"a.md"}"#
        );
    }

    #[test]
    fn find_json_col_bare_field_narrows_frontmatter() {
        let fm = json!({"title": "A", "type": "note"});
        let v = project_json(
            &view(Some(&fm), &[], &[], Some("body")),
            &["title".to_string()],
            false,
            DefaultCols::FrontmatterOnly,
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"frontmatter":{"title":"A"},"path":"a.md"}"#
        );
    }

    #[test]
    fn find_json_absent_frontmatter_is_null() {
        let v = project_json(
            &view(None, &[], &[], Some("body")),
            &[],
            false,
            DefaultCols::FrontmatterOnly,
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"frontmatter":null,"path":"a.md"}"#
        );
    }

    #[test]
    fn json_deep_facet_emits_serialized_values_verbatim() {
        let headings = vec![json!({"level": 2, "text": "Sec", "slug": "sec"})];
        let fm = json!({"type": "note"});
        let v = project_json(
            &view(Some(&fm), &headings, &[], Some("body")),
            &[".headings".to_string()],
            false,
            DefaultCols::FrontmatterOnly,
        );
        assert_eq!(
            v["headings"],
            json!([{"level": 2, "text": "Sec", "slug": "sec"}])
        );
    }

    #[test]
    fn records_deep_facet_folds_to_display_string() {
        let headings = vec![json!({"level": 2, "text": "Sec", "slug": "sec"})];
        let fm = json!({"type": "note"});
        let pairs = project_pairs(
            &view(Some(&fm), &headings, &[], Some("body")),
            &[".headings".to_string()],
            false,
        );
        assert!(
            pairs.iter().any(|(k, v)| k == "headings" && v == "## Sec"),
            "expected a headings row rendered as '## Sec', got {pairs:?}"
        );
    }

    #[test]
    fn get_json_full_facets_default_emits_facets_no_body() {
        let fm = json!({"title": "A"});
        let headings = vec![json!({"level": 2, "text": "Sec", "slug": "sec"})];
        let outgoing = vec![json!({"target": "b", "resolved_path": "b.md"})];
        let v = project_json(
            &view(Some(&fm), &headings, &outgoing, None),
            &[],
            false,
            DefaultCols::FullFacets,
        );
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("path"));
        assert!(obj.contains_key("frontmatter"));
        assert!(obj.contains_key("headings"));
        assert!(obj.contains_key("outgoing_links"));
        assert!(obj.contains_key("incoming_links"));
        // No body/hash/stem in the default dump (body was None).
        assert!(!obj.contains_key("body"));
        assert!(!obj.contains_key("document_hash"));
        assert!(!obj.contains_key("stem"));
    }

    #[test]
    fn get_json_col_narrows_to_requested_facets_only() {
        let fm = json!({"title": "A"});
        let headings = vec![json!({"level": 2, "text": "Sec", "slug": "sec"})];
        let outgoing = vec![json!({"target": "b", "resolved_path": "b.md"})];
        let v = project_json(
            &view(Some(&fm), &headings, &outgoing, None),
            &[".headings".to_string()],
            false,
            DefaultCols::FullFacets,
        );
        let obj = v.as_object().unwrap();
        assert!(obj.contains_key("headings"));
        assert!(obj.contains_key("path"));
        assert!(!obj.contains_key("frontmatter"));
        assert!(!obj.contains_key("outgoing_links"));
    }

    #[test]
    fn get_json_document_hash_is_opt_in() {
        let fm = json!({"title": "A"});
        let v = project_json(
            &view(Some(&fm), &[], &[], None),
            &[".document_hash".to_string()],
            false,
            DefaultCols::FullFacets,
        );
        assert_eq!(v["document_hash"], json!("deadbeef"));
    }
}
