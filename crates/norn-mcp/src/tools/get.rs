//! `vault.get` — structured document fetch (and exact-source markdown).
//!
//! The param struct mirrors `norn get`'s daily surface; the handler routes to the
//! owner and projects each wire [`GetRecord`] into the same full-facet JSON the
//! CLI's `--format json` emits, wrapped in the typed [`GetOutput`] envelope. A
//! requested target that did not resolve (an `error`-severity [`Note`]) maps to
//! `isError: true` while still returning every good target's records — the bit is
//! read from the note's typed severity, never from its message text.

use norn_wire::{GetParams as WireGetParams, GetRecord, GetReport, Note, SortPaginateParams};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.get`. Mirrors `norn get`: one or more targets, the
/// optional column request, the shared sort/paging knobs, the repeatable
/// `--section` heading slice, and `--all-cols`. `format: "markdown"` selects the
/// exact-source envelope.
///
/// Paging convention: `starts_at` is ZERO-indexed — an omitted value is the
/// first record.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct GetParams {
    /// One or more document targets (stem or path), as `norn get` accepts.
    pub targets: Vec<String>,

    /// Response representation. `structured` (the default) returns document
    /// records; `markdown` returns one exact on-disk document and refuses unless
    /// exactly one document is selected.
    #[serde(default)]
    pub format: GetRepresentation,

    /// Optional column request, comma-separated, in `norn get --col` syntax. On
    /// the MCP surface this only controls whether the on-request facets (`.body`,
    /// `.document_hash`) are INCLUDED — it does not narrow the always-full record
    /// dump (a known, tracked divergence from the CLI's narrowing `--col`).
    #[serde(default)]
    pub col: Option<String>,

    /// Sort by field (frontmatter key, `path`, or `stem`); ascending by default.
    #[serde(default)]
    pub sort: Option<String>,
    /// Sort descending (only meaningful with `sort`).
    #[serde(default)]
    pub desc: bool,
    /// Maximum number of records to return. Absent → every named target.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Return every named target regardless of `limit`.
    #[serde(default)]
    pub no_limit: bool,
    /// Zero-indexed starting offset for paging. Defaults to 0 (the first record).
    #[serde(default)]
    pub starts_at: usize,

    /// Named sections to read, by exact heading text. Repeatable.
    #[serde(default)]
    pub section: Vec<String>,

    /// Emit the full structured dump including `.body` for each record.
    #[serde(default)]
    pub all_cols: bool,
}

/// Representation returned by `vault.get`.
#[derive(
    Debug, Clone, Copy, Default, Deserialize, Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum GetRepresentation {
    /// Parsed document records and their graph connections.
    #[default]
    Structured,
    /// One byte-faithful UTF-8 Markdown document read from the vault file.
    Markdown,
}

/// Targets for which `--section` was requested but NONE of the requested headings
/// resolved.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SectionFailure {
    /// The all-missing target's vault-relative path.
    pub path: String,
    /// The requested headings (deduped, request order) that all failed to resolve.
    pub requested_headings: Vec<String>,
}

/// Structured output for `vault.get`. rmcp requires a root `type: object`; the
/// per-record payload stays generic `Value` (the record's core types carry a path
/// type with no `JsonSchema` impl).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GetOutput {
    /// One entry per resolved document, in resolution order — the full-facet JSON
    /// of a `norn get` record.
    pub records: Vec<Value>,
    /// Targets whose `--section` request resolved no headings. Empty when no
    /// `--section` was requested.
    pub section_failures: Vec<SectionFailure>,
    /// Non-fatal diagnostics as typed `{severity, code, message}` notes:
    /// ambiguous-stem warnings and missing-target errors. An `error`-severity
    /// note is the signal this tool maps to `isError: true`; a consumer branches
    /// on `severity` / `code`, never on the message text.
    pub notes: Vec<Note>,
    /// Exact source representation, present only for `format: "markdown"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownOutput>,
}

/// Format-specific response for `vault.get { format: "markdown" }`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MarkdownOutput {
    /// Resolved vault-relative document path.
    pub path: String,
    /// Exact UTF-8 file content, with no newline normalization.
    pub content: String,
}

/// Whether the col request opts a facet into the record dump.
fn col_wants(col: &Option<String>, facet: &str) -> bool {
    col.as_deref()
        .map(|c| c.split(',').any(|t| t.trim() == facet))
        .unwrap_or(false)
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: &GetParams) -> WireGetParams {
    let with_body = p.all_cols || col_wants(&p.col, ".body");
    WireGetParams {
        targets: p.targets.clone(),
        paging: SortPaginateParams {
            sort: p.sort.clone(),
            desc: p.desc,
            limit: p.limit,
            no_limit: p.no_limit,
            starts_at: p.starts_at,
        },
        sections: p.section.clone(),
        with_body,
        markdown: p.format == GetRepresentation::Markdown,
    }
}

/// Project one wire record into the full-facet JSON the CLI's `--format json`
/// emits (MCP semantics: the record is always fully dumped; `body`/`document_hash`
/// ride only when the col request asked). A record with NO frontmatter block
/// omits the `frontmatter` key; an empty `---`/`---` block keeps `null`.
fn record_json(rec: &GetRecord, want_body: bool, want_hash: bool) -> Value {
    let mut m = Map::new();
    m.insert("path".into(), Value::String(rec.path.clone()));
    if let Some(fm) = &rec.frontmatter {
        m.insert("frontmatter".into(), fm.clone());
    }
    m.insert("headings".into(), Value::Array(rec.headings.clone()));
    m.insert(
        "outgoing_links".into(),
        Value::Array(rec.outgoing_links.clone()),
    );
    m.insert(
        "unresolved_links".into(),
        Value::Array(rec.unresolved_links.clone()),
    );
    m.insert(
        "incoming_links".into(),
        Value::Array(rec.incoming_links.clone()),
    );
    if want_hash && !rec.hash.is_empty() {
        m.insert("document_hash".into(), Value::String(rec.hash.clone()));
    }
    if want_body {
        if let Some(body) = &rec.body {
            m.insert("body".into(), Value::String(body.clone()));
        }
    }
    if let Some(sections) = &rec.sections {
        let mut obj = Map::with_capacity(sections.len());
        for (heading, content) in sections {
            obj.insert(heading.clone(), Value::String(content.clone()));
        }
        m.insert("sections".into(), Value::Object(obj));
    }
    Value::Object(m)
}

/// Project the wire report into the typed [`GetOutput`], deriving the `isError`
/// bit from the report's `error:`-prefixed notes (a target that did not resolve).
pub(crate) fn envelope(p: &GetParams, report: GetReport) -> MutationResult<GetOutput> {
    let want_body = p.all_cols || col_wants(&p.col, ".body");
    let want_hash = col_wants(&p.col, ".document_hash");
    let records = report
        .records
        .iter()
        .map(|r| record_json(r, want_body, want_hash))
        .collect();
    let markdown = if p.format == GetRepresentation::Markdown {
        report.markdown_content.as_ref().and_then(|content| {
            report.records.first().map(|r| MarkdownOutput {
                path: r.path.clone(),
                content: content.clone(),
            })
        })
    } else {
        None
    };
    let is_error = report.notes.iter().any(Note::is_error);
    let output = GetOutput {
        records,
        // The wire GetReport does not carry section failures; a default get (no
        // `--section`) has none. See the crate notes for the tracked wire gap.
        section_failures: Vec::new(),
        notes: report.notes,
        markdown,
    };
    MutationResult::from_flag(output, is_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(frontmatter: Option<Value>) -> GetRecord {
        GetRecord {
            path: "notes/alpha.md".into(),
            stem: "alpha".into(),
            hash: "deadbeef".into(),
            frontmatter,
            headings: vec![json!({"level":1,"text":"Alpha"})],
            outgoing_links: vec![],
            unresolved_links: vec![],
            incoming_links: vec![],
            body: Some("body".into()),
            sections: None,
        }
    }

    #[test]
    fn default_record_is_full_facets_without_stem_hash_or_body() {
        // The default dump omits stem/hash/body and keeps path + frontmatter +
        // the facet arrays.
        let v = record_json(&record(Some(json!({"title":"Alpha"}))), false, false);
        assert_eq!(v["path"], json!("notes/alpha.md"));
        assert_eq!(v["frontmatter"], json!({"title":"Alpha"}));
        assert!(v.get("stem").is_none(), "stem is never in the dump");
        assert!(v.get("document_hash").is_none());
        assert!(v.get("body").is_none());
        assert!(v.get("headings").is_some());
    }

    #[test]
    fn absent_frontmatter_block_omits_the_key() {
        // A source with no `---` block (frontmatter None) omits the key entirely,
        // preserving the absent-vs-null distinction.
        let v = record_json(&record(None), false, false);
        assert!(v.get("frontmatter").is_none());
    }

    #[test]
    fn body_and_hash_ride_only_when_the_col_asks() {
        let v = record_json(&record(Some(json!({}))), true, true);
        assert_eq!(v["body"], json!("body"));
        assert_eq!(v["document_hash"], json!("deadbeef"));
    }

    #[test]
    fn col_wants_matches_dotted_facets() {
        assert!(col_wants(&Some(".body,.document_hash".into()), ".body"));
        assert!(col_wants(&Some(".document_hash".into()), ".document_hash"));
        assert!(!col_wants(&Some("status".into()), ".body"));
        assert!(!col_wants(&None, ".body"));
    }
}
