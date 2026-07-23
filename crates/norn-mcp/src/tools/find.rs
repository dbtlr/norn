//! `vault.find` — filter / sort / page the vault's documents.
//!
//! The param struct is a flat filter-args mirror (its schemars schema is the
//! published `inputSchema`); the handler maps it to a norn-wire [`FindParams`]
//! and routes to the owner, then projects each [`FindDoc`] into the full-facet
//! JSON `norn find --format json` emits, wrapped in the typed [`FindOutput`]
//! envelope. Paging is ZERO-indexed via `starts_at` (NRN-332): an omitted value
//! is the first record.

use norn_wire::{
    FilterParams, FindDoc, FindParams as WireFindParams, FindReport, SortPaginateParams,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Parameters for `vault.find` — mirrors `norn find`'s daily surface: the full
/// find-filter predicate set, the shared sort/paging knobs, and the column
/// request. `--format` / `--no-pager` are CLI-only (the MCP tool always returns
/// the structured envelope).
///
/// Paging convention: `starts_at` is ZERO-indexed — an omitted value is the
/// first record. `limit` defaults to the wire/verb default when omitted; pass
/// `no_limit: true` for the full match set.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct FindParams {
    // ── Filter predicates (mirrors FilterArgs) ──────────────────────────────
    /// Full-text body substring. Case-insensitive.
    #[serde(default)]
    pub text: Option<String>,
    /// Frontmatter equality predicates `field:value`. Repeatable; all must match.
    #[serde(default)]
    pub eq: Vec<String>,
    /// Frontmatter inequality predicates `field:value`. Repeatable.
    #[serde(default)]
    pub not_eq: Vec<String>,
    /// Frontmatter ANY-of predicates `field:V1,V2,...`. Repeatable.
    #[serde(default)]
    #[serde(rename = "in")]
    pub r#in: Vec<String>,
    /// Frontmatter NOT-in predicates `field:V1,V2,...`. Repeatable.
    #[serde(default)]
    pub not_in: Vec<String>,
    /// Frontmatter prefix predicates `field:VALUE`. Case-sensitive. Repeatable.
    #[serde(default)]
    pub starts_with: Vec<String>,
    /// Frontmatter suffix predicates `field:VALUE`. Case-sensitive. Repeatable.
    #[serde(default)]
    pub ends_with: Vec<String>,
    /// Frontmatter substring predicates `field:VALUE`. Case-sensitive. Repeatable.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Frontmatter fields that must be present (non-null). Repeatable.
    #[serde(default)]
    pub has: Vec<String>,
    /// Frontmatter fields that must be absent or null. Repeatable.
    #[serde(default)]
    pub missing: Vec<String>,
    /// Date-before predicates `field:DATE`. ISO 8601. Repeatable.
    #[serde(default)]
    pub before: Vec<String>,
    /// Date-after predicates `field:DATE`. ISO 8601. Repeatable.
    #[serde(default)]
    pub after: Vec<String>,
    /// Date-on predicates `field:DATE`. Accepts `today`. Repeatable.
    #[serde(default)]
    pub on: Vec<String>,
    /// Path glob patterns. Repeatable.
    #[serde(default)]
    pub path: Vec<String>,
    /// Documents whose outgoing links resolve to TARGET. Repeatable; AND'd.
    #[serde(default)]
    pub links_to: Vec<String>,
    /// Include only documents with at least one unresolved link.
    #[serde(default)]
    pub unresolved_links: bool,

    // ── Sort / limit / paging (mirrors SortPaginateArgs) ─────────────────────
    /// Sort by field (frontmatter key, `path`, or `stem`); ascending by default.
    #[serde(default)]
    pub sort: Option<String>,
    /// Sort descending (only meaningful with `sort`).
    #[serde(default)]
    pub desc: bool,
    /// Maximum documents to return. Absent → the verb default; use `no_limit`
    /// for every match.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Return every matching document, overriding `limit`.
    #[serde(default)]
    pub no_limit: bool,
    /// Zero-indexed starting offset for paging. Defaults to 0 (the first record).
    #[serde(default)]
    pub starts_at: usize,

    // ── Column projection (mirrors --col / --all-cols) ───────────────────────
    /// Optional column request, comma-separated, in `norn find --col` syntax.
    /// On the MCP surface this only controls whether the on-request facets
    /// (`.headings`, the three link sets, `.body`, `.document_hash`) are
    /// INCLUDED — it does not narrow the always-full record dump.
    #[serde(default)]
    pub col: Option<String>,
    /// Emit the full structured dump per match, including `.body` and the deep
    /// connection facets.
    #[serde(default)]
    pub all_cols: bool,
}

/// Structured output for `vault.find`. rmcp requires a root `type: object`; the
/// per-document payload stays generic `Value` (the flat projection of a
/// `norn find` record).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FindOutput {
    /// Total documents matching the predicates BEFORE limit/paging.
    pub total: usize,
    /// Number of documents actually returned (after limit/paging).
    pub returned: usize,
    /// `returned < total` — the full match set exceeds this page.
    pub truncated: bool,
    /// 1-based position of the first returned record (the wire echo of the
    /// zero-indexed `starts_at` offset), floored at 1.
    pub starts_at: usize,
    /// Whether the vault carries any error-severity diagnostic — scoped to the
    /// whole vault, not this query's matches. Lets an off-filesystem consumer
    /// reproduce the direct path's diagnostic-error signal.
    pub has_diagnostic_errors: bool,
    /// Matched documents, in sort order, after limit/paging.
    pub documents: Vec<Value>,
}

/// Whether the col request opts a facet into the record dump.
fn col_wants(col: &Option<String>, facet: &str) -> bool {
    col.as_deref()
        .map(|c| c.split(',').any(|t| t.trim() == facet))
        .unwrap_or(false)
}

/// Whether the request asks for any deep connection facet (so the wire request
/// must load connections).
fn wants_connections(col: &Option<String>, all_cols: bool) -> bool {
    all_cols
        || [
            ".headings",
            ".outgoing_links",
            ".unresolved_links",
            ".incoming_links",
        ]
        .iter()
        .any(|f| col_wants(col, f))
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: &FindParams) -> WireFindParams {
    WireFindParams {
        filter: FilterParams {
            text: p.text.clone(),
            eq: p.eq.clone(),
            not_eq: p.not_eq.clone(),
            r#in: p.r#in.clone(),
            not_in: p.not_in.clone(),
            starts_with: p.starts_with.clone(),
            ends_with: p.ends_with.clone(),
            contains: p.contains.clone(),
            has: p.has.clone(),
            missing: p.missing.clone(),
            before: p.before.clone(),
            after: p.after.clone(),
            on: p.on.clone(),
            path: p.path.clone(),
            links_to: p.links_to.clone(),
            unresolved_links: p.unresolved_links,
        },
        paging: SortPaginateParams {
            sort: p.sort.clone(),
            desc: p.desc,
            limit: p.limit,
            no_limit: p.no_limit,
            starts_at: p.starts_at,
        },
        with_connections: wants_connections(&p.col, p.all_cols),
        dynamic_keys: Vec::new(),
    }
}

/// Project one flat wire [`FindDoc`] into the record JSON: `{path, frontmatter}`
/// by default, with the deep facets / body / hash riding only when the col
/// request (or `all_cols`) asked.
fn doc_json(doc: &FindDoc, want_body: bool, want_hash: bool, want_conns: bool) -> Value {
    let mut m = Map::new();
    m.insert("path".into(), Value::String(doc.path.clone()));
    if let Some(fm) = &doc.frontmatter {
        m.insert("frontmatter".into(), fm.clone());
    }
    if want_conns {
        m.insert("headings".into(), Value::Array(doc.headings.clone()));
        m.insert(
            "outgoing_links".into(),
            Value::Array(doc.outgoing_links.clone()),
        );
        m.insert(
            "unresolved_links".into(),
            Value::Array(doc.unresolved_links.clone()),
        );
        m.insert(
            "incoming_links".into(),
            Value::Array(doc.incoming_links.clone()),
        );
    }
    if want_hash && !doc.hash.is_empty() {
        m.insert("document_hash".into(), Value::String(doc.hash.clone()));
    }
    if want_body {
        m.insert("body".into(), Value::String(doc.body_text.clone()));
    }
    Value::Object(m)
}

/// Project the wire report into the typed [`FindOutput`].
pub(crate) fn envelope(p: &FindParams, report: FindReport) -> FindOutput {
    let want_body = p.all_cols || col_wants(&p.col, ".body");
    let want_hash = col_wants(&p.col, ".document_hash");
    let want_conns = wants_connections(&p.col, p.all_cols);
    let documents = report
        .documents
        .iter()
        .map(|d| doc_json(d, want_body, want_hash, want_conns))
        .collect();
    FindOutput {
        total: report.total,
        returned: report.returned,
        truncated: report.truncated,
        starts_at: report.starts_at,
        has_diagnostic_errors: report.has_diagnostic_errors,
        documents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn find_doc() -> FindDoc {
        FindDoc {
            path: "notes/a.md".into(),
            stem: "a".into(),
            hash: "hh".into(),
            frontmatter: Some(json!({"type": "note"})),
            body_text: "body text".into(),
            headings: vec![json!({"text": "H"})],
            outgoing_links: vec![],
            unresolved_links: vec![],
            incoming_links: vec![],
        }
    }

    fn report(docs: Vec<FindDoc>) -> FindReport {
        let returned = docs.len();
        FindReport {
            documents: docs,
            total: returned,
            returned,
            starts_at: 1,
            truncated: false,
            has_diagnostic_errors: false,
        }
    }

    #[test]
    fn default_projection_is_path_and_frontmatter_only() {
        let out = envelope(&FindParams::default(), report(vec![find_doc()]));
        let d = &out.documents[0];
        assert_eq!(d["path"], json!("notes/a.md"));
        assert_eq!(d["frontmatter"], json!({"type": "note"}));
        assert!(d.get("headings").is_none(), "no deep facets by default");
        assert!(d.get("body").is_none());
        assert!(d.get("document_hash").is_none());
    }

    #[test]
    fn all_cols_rides_body_and_connections() {
        let p = FindParams {
            all_cols: true,
            ..Default::default()
        };
        let out = envelope(&p, report(vec![find_doc()]));
        let d = &out.documents[0];
        assert_eq!(d["body"], json!("body text"));
        assert!(d.get("headings").is_some());
    }

    #[test]
    fn hash_rides_only_when_col_asks() {
        let p = FindParams {
            col: Some(".document_hash".into()),
            ..Default::default()
        };
        let out = envelope(&p, report(vec![find_doc()]));
        assert_eq!(out.documents[0]["document_hash"], json!("hh"));
    }

    #[test]
    fn to_wire_sets_connections_for_deep_col() {
        let p = FindParams {
            col: Some(".outgoing_links".into()),
            eq: vec!["type:note".into()],
            starts_at: 2,
            ..Default::default()
        };
        let wire = to_wire(&p);
        assert!(wire.with_connections);
        assert_eq!(wire.filter.eq, vec!["type:note".to_string()]);
        assert_eq!(
            wire.paging.starts_at, 2,
            "zero-indexed offset passes through"
        );
    }

    #[test]
    fn envelope_carries_diagnostic_error_flag() {
        let mut r = report(vec![]);
        r.has_diagnostic_errors = true;
        let out = envelope(&FindParams::default(), r);
        assert!(out.has_diagnostic_errors);
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<FindParams>(json!({ "bogus": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("bogus"));
    }
}
