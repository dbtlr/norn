//! `vault.find` — full-text + metadata document search.
//!
//! The pure handler reuses [`crate::find::query`], the shared selection/JSON
//! seam behind `norn find --format json`. It constructs a [`FindArgs`] with the
//! CLI's exact defaults and returns the same per-document JSON values the CLI
//! emits, so the MCP surface and the CLI can never drift on filter evaluation,
//! sort, limit, paging, or `--col` projection.
//!
//! **Output envelope:** rmcp 1.7.0 requires a tool's `outputSchema` root to be
//! `type: object`. The per-document payload is generic `serde_json::Value` (the
//! same trade-off `vault.get` makes — deriving `JsonSchema` across the core
//! types carrying `Utf8PathBuf` is a large, drift-prone change for no v1 gain),
//! so we wrap the document array in a typed [`FindOutput`] struct to get the
//! root-object schema.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{FindArgs, SortPaginateArgs};
use crate::filter_args::FilterArgs;
use crate::mcp::context::VaultContext;

/// Parameters for `vault.find`.
///
/// Mirrors the agent-relevant slice of `norn find`'s flags: the full find-filter
/// surface (text, eq, not_eq, in, not_in, starts_with, ends_with, contains, has,
/// missing, before, after, on, path, links_to, unresolved_links), the
/// sort/limit/paging knobs (sort, desc, limit, no_limit, starts_at), and the
/// column projection (col, all_cols). `--format` and `--no-pager` are CLI-only —
/// the MCP tool always returns the structured document array.
///
/// **Default fidelity:** an omitted `limit` defaults to 10, exactly like
/// `norn find` (constructed in [`handle`] via the shared `find::query` path).
/// An agent that wants the full result set passes `no_limit: true`; paging works
/// via `starts_at` (1-indexed, default 1).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct FindParams {
    // ── Filter predicates (mirrors FilterArgs) ──────────────────────────────
    /// Full-text body substring. Case-insensitive. Empty string is a no-op.
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

    /// Frontmatter prefix predicates `field:VALUE` — the field (or any array
    /// element) starts with VALUE. Case-sensitive. Repeatable; all must match.
    #[serde(default)]
    pub starts_with: Vec<String>,

    /// Frontmatter suffix predicates `field:VALUE` — the field (or any array
    /// element) ends with VALUE. Case-sensitive. Repeatable.
    #[serde(default)]
    pub ends_with: Vec<String>,

    /// Frontmatter substring predicates `field:VALUE` — the field (or any
    /// array element) contains VALUE. Case-sensitive. Repeatable.
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
    /// Sort by field (frontmatter key, `path`, or `stem`). Ascending by default.
    #[serde(default)]
    pub sort: Option<String>,

    /// Sort descending instead of ascending. Only meaningful with `sort`.
    #[serde(default)]
    pub desc: bool,

    /// Maximum documents to return. Omitted → 10 (matches `norn find`). Use
    /// `no_limit` to return every match.
    #[serde(default)]
    pub limit: Option<usize>,

    /// Return every matching document; overrides `limit`.
    #[serde(default)]
    pub no_limit: bool,

    /// 1-indexed starting offset for paging. Default 1.
    #[serde(default)]
    pub starts_at: Option<usize>,

    // ── Column projection (mirrors --col / --all-cols) ───────────────────────
    /// Columns to include, in `norn find --col` syntax: bare frontmatter fields
    /// (e.g. `status`, `title`) and dot-prefixed facets (`.body`, `.headings`,
    /// `.outgoing_links`, `.unresolved_links`, `.incoming_links`, `.raw`,
    /// `.stem`, `.frontmatter`). Default (empty): `{path, frontmatter}` per doc.
    #[serde(default)]
    pub col: Vec<String>,

    /// Emit the full structured dump per match: whole frontmatter plus every
    /// cache-served facet (`.headings`, the three link sets, `.body`). Excludes
    /// `.raw`. Mutually exclusive with `col`.
    #[serde(default)]
    pub all_cols: bool,

    /// PRIVATE norn-CLI↔norn-daemon channel (NRN-218), NOT public MCP surface —
    /// `#[schemars(skip)]` keeps it out of the published input schema and
    /// `tools/list`. Carries the field names the CLI's ADR 0010 forgiving-input
    /// pass desugared from `--<field> value` spellings into the `eq`/`in`
    /// predicates above, so the daemon can run the field-universe gate against
    /// its warm cache and refuse an unknown field byte-identically to the direct
    /// path. An off-filesystem MCP client filters with canonical predicates and
    /// leaves this empty (the gate is then a no-op).
    #[serde(default)]
    #[schemars(skip)]
    pub dynamic_keys: Vec<String>,
}

/// Structured output for `vault.find`.
///
/// rmcp requires a root `type: object` schema; this typed envelope provides it
/// while keeping the per-document payload generic `serde_json::Value` (each is
/// the JSON form of a `norn find --format json` document). See module docs.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FindOutput {
    /// Total documents matching the predicates BEFORE limit/paging. Lets a
    /// consumer know how many matches exist beyond the returned page (NRN-214).
    pub total: usize,
    /// Number of documents actually returned (after limit/paging/path-glob) —
    /// equals `documents.len()`.
    pub returned: usize,
    /// `returned < total` — the full match set exceeds this page's `returned`
    /// count. Mirrors the CLI's truncation note (which an off-filesystem client
    /// cannot see). NOT a page-forward terminator on its own: a page requested
    /// PAST the end has `returned: 0` yet `truncated: true` (`0 < total`). To page
    /// forward, advance `starts_at` and stop when `returned == 0` (or compare
    /// `starts_at - 1 + returned` against `total`).
    pub truncated: bool,
    /// 1-indexed paging offset of this page (floored at 1), matching the CLI
    /// `--format json` envelope's `starts_at`.
    pub starts_at: usize,
    /// Whether the vault carries any error-severity diagnostic (e.g. an
    /// unreadable document) — the signal `norn find` maps to exit 2 (NRN-222).
    /// Render-critical envelope state in the NRN-214 spirit: without it a routed
    /// find could not reproduce the direct path's exit code. Scoped to the whole
    /// vault, not this query's matches.
    pub has_diagnostic_errors: bool,
    /// Matched documents, in sort order, after limit/paging — the same
    /// per-document JSON `norn find --format json` emits. With no `col`/`all_cols`,
    /// each is `{path, frontmatter}`; projections add/narrow per the `col` syntax.
    pub documents: Vec<serde_json::Value>,

    /// PRIVATE norn-CLI↔norn-daemon channel (NRN-218), NOT public MCP surface.
    /// Set only when the daemon's field-universe gate refuses a `dynamic_keys`
    /// entry; the routed CLI re-emits `message` and exits 1, exactly as the direct
    /// gate would. `#[serde(skip_serializing_if)]` keeps the success envelope
    /// byte-identical (the key is simply absent), and `#[schemars(skip)]` keeps it
    /// out of the published output schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub dynamic_field_error: Option<crate::grammar::DynamicFieldRefusal>,
}

/// Pure handler for `vault.find`. Opens a fresh query cache (per-call freshness),
/// constructs [`FindArgs`] with `norn find`'s exact defaults (notably `limit`
/// → 10 when omitted), and runs the shared `find::query` seam.
pub fn handle(ctx: &VaultContext, p: FindParams) -> Result<FindOutput> {
    // The per-call served marker (routing proofs) is emitted by the server
    // layer (`McpServer::run_wrapped`), daemon-gated — never by this handler,
    // so a stdio `norn mcp` process writes no marker.
    let cache = ctx.query_cache()?;

    // NRN-218: run the ADR 0010 field-universe gate against the warm cache BEFORE
    // querying, exactly where the direct path gates (`find::run`). A refused
    // dynamic key crosses back as a `dynamic_field_error` the routed CLI re-emits
    // byte-identically; the gate is a no-op when the CLI sent no `dynamic_keys`
    // (the canonical / off-filesystem case), so it costs the happy path nothing.
    if let Some(refusal) = crate::grammar::gate_dynamic_refusal(
        &cache,
        &ctx.config(),
        &p.dynamic_keys,
        crate::grammar::QueryCmd::Find,
    )? {
        return Ok(FindOutput {
            total: 0,
            returned: 0,
            truncated: false,
            starts_at: p.starts_at.unwrap_or(1).max(1),
            has_diagnostic_errors: false,
            documents: Vec::new(),
            dynamic_field_error: Some(refusal),
        });
    }

    let args = FindArgs {
        filters: FilterArgs {
            text: p.text,
            eq: p.eq,
            not_eq: p.not_eq,
            r#in: p.r#in,
            not_in: p.not_in,
            starts_with: p.starts_with,
            ends_with: p.ends_with,
            contains: p.contains,
            has: p.has,
            missing: p.missing,
            before: p.before,
            after: p.after,
            on: p.on,
            path: p.path,
            links_to: p.links_to,
            unresolved_links: p.unresolved_links,
        },
        // `--all` is a CLI escape hatch gating the missing-predicate help page;
        // it does not affect query semantics. Set true so the MCP tool never
        // hits that CLI-only gate — `find::query` does not consult it anyway.
        all: true,
        paging: SortPaginateArgs {
            sort: p.sort,
            desc: p.desc,
            // limit: None here means `find::query` applies the CLI default of 10
            // (see find::query::build_find_query). Honor an explicit limit and
            // `no_limit` exactly as the CLI does.
            limit: p.limit,
            no_limit: p.no_limit,
            // starts_at default is 1 (1-indexed), matching the CLI flag default.
            starts_at: p.starts_at.unwrap_or(1),
        },
        // `--format` / `--no-pager` are CLI-only output knobs; the MCP tool
        // always returns the structured document array, so these are irrelevant
        // to `find::query` (which never renders).
        format: None,
        all_cols: p.all_cols,
        col: p.col,
        no_pager: false,
    };

    // The WIRE projection (NRN-222): identical per-document JSON to the CLI's
    // `--format json` except that a document with NO frontmatter block omits
    // the `frontmatter` key (an empty `---\n---` block keeps `"frontmatter":
    // null`), so the routed client can rebuild the exact direct-path state.
    let (documents, envelope) = crate::find::query::query_wire_with_envelope(&cache, &args)?;
    Ok(FindOutput {
        total: envelope.total,
        returned: envelope.returned,
        truncated: envelope.truncated,
        starts_at: envelope.starts_at,
        // The CLI's exit-2 signal (any error-severity diagnostic in the vault),
        // carried so a routed find reproduces the direct exit code (NRN-222).
        has_diagnostic_errors: cache.has_diagnostic_errors()?,
        documents,
        // No gate refusal on the success path (handled by the early return above).
        dynamic_field_error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// 3 docs: 2 `type: note`, 1 `type: task`.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-find-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("note1.md"),
            "---\ntype: note\ntitle: Note One\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("note2.md"),
            "---\ntype: note\ntitle: Note Two\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            root.join("task1.md"),
            "---\ntype: task\ntitle: Task One\n---\nbody\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn handle_eq_type_note_returns_seeded_notes() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                eq: vec!["type:note".into()],
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(
            out.documents.len(),
            2,
            "eq type:note should return 2 notes, got {}",
            out.documents.len()
        );
        // Default shape: each doc carries `path` + `frontmatter`.
        for doc in &out.documents {
            assert!(doc.get("path").is_some(), "each doc has a path: {doc}");
            assert_eq!(
                doc["frontmatter"]["type"], "note",
                "every returned doc is type:note: {doc}"
            );
        }
    }

    /// NRN-218: a KNOWN dynamic field passes the daemon-side gate — no refusal,
    /// and the query runs exactly as the canonical `eq` form would.
    #[test]
    fn handle_known_dynamic_field_passes_gate() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                eq: vec!["type:note".into()],
                dynamic_keys: vec!["type".into()],
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");

        assert!(
            out.dynamic_field_error.is_none(),
            "a known dynamic field must not be refused"
        );
        assert_eq!(out.documents.len(), 2, "known --type note returns 2 notes");
    }

    /// NRN-218: an UNKNOWN dynamic field is refused daemon-side with the exact
    /// gate message the direct path would emit, and no documents are returned.
    #[test]
    fn handle_unknown_dynamic_field_is_refused() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                eq: vec!["nonexistentfield:x".into()],
                dynamic_keys: vec!["nonexistentfield".into()],
                ..FindParams::default()
            },
        )
        .expect("a refusal is a structured envelope, not a handler Err");

        let refusal = out
            .dynamic_field_error
            .expect("an unknown dynamic field must be refused");
        assert_eq!(refusal.code, "unknown-field");
        assert!(
            refusal.message.contains("unknown field `nonexistentfield`"),
            "message: {}",
            refusal.message
        );
        assert!(out.documents.is_empty(), "a refusal returns no documents");
    }

    #[test]
    fn handle_starts_with_filters_by_prefix() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                starts_with: vec!["title:Note".into()],
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");

        assert_eq!(
            out.documents.len(),
            2,
            "starts_with title:Note should return the 2 notes, got {}",
            out.documents.len()
        );
    }

    #[test]
    fn handle_no_filter_dumps_with_default_limit() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        // No predicate, no limit → all 3 (well under the default-10 cap).
        let out = handle(&ctx, FindParams::default()).expect("handle should succeed");
        assert_eq!(
            out.documents.len(),
            3,
            "no filter should return all 3 seeded docs, got {}",
            out.documents.len()
        );
    }

    #[test]
    fn handle_limit_one_caps_results() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                limit: Some(1),
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");
        assert_eq!(
            out.documents.len(),
            1,
            "limit:1 should cap to a single doc, got {}",
            out.documents.len()
        );
        // NRN-214: the envelope tells a consumer the page is incomplete.
        assert_eq!(out.total, 3, "total counts all matches before the limit");
        assert_eq!(out.returned, 1, "returned equals the page length");
        assert!(out.truncated, "returned(1) < total(3) => truncated");
        assert_eq!(out.starts_at, 1, "default paging offset is 1");
    }

    /// NRN-214: a full result (no limit, no offset) reports `truncated: false`,
    /// `returned == total`, and the default `starts_at: 1`.
    #[test]
    fn handle_envelope_full_result_is_untruncated() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                no_limit: true,
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");
        assert_eq!(out.total, 3, "3 docs match");
        assert_eq!(out.returned, 3, "no_limit returns every match");
        assert!(!out.truncated, "returned == total => not truncated");
        assert_eq!(out.starts_at, 1, "default paging offset is 1");
    }

    /// NRN-214: a paging offset is echoed in `starts_at`, and dropping the leading
    /// page makes `returned < total` (so `truncated` is true) even without a limit.
    #[test]
    fn handle_envelope_echoes_paging_offset() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                no_limit: true,
                starts_at: Some(2),
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");
        assert_eq!(out.starts_at, 2, "explicit paging offset is echoed");
        assert_eq!(out.total, 3, "total is all matches, pre-offset");
        assert_eq!(out.returned, 2, "offset 2 drops the first of 3 docs");
        assert!(out.truncated, "returned(2) < total(3) => truncated");
    }

    /// NRN-214 (review fix): `truncated` mirrors the CLI's `returned < total`, so a
    /// page requested PAST the end returns zero docs but is STILL `truncated: true`
    /// (`0 < total`). Pins the documented contract that `truncated` is not a
    /// page-forward terminator — `returned == 0` is — so a paging consumer cannot
    /// be led into a non-terminating loop.
    #[test]
    fn handle_envelope_past_end_page_is_empty_but_truncated() {
        let (_tmp, root) = seeded_vault();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(
            &ctx,
            FindParams {
                no_limit: true,
                starts_at: Some(10), // well past the 3 seeded docs
                ..FindParams::default()
            },
        )
        .expect("handle should succeed");
        assert_eq!(out.returned, 0, "a past-the-end page returns nothing");
        assert!(out.documents.is_empty());
        assert_eq!(out.total, 3, "total still counts every match");
        assert!(
            out.truncated,
            "returned(0) < total(3) => truncated (CLI parity; NOT a terminator)"
        );
        assert_eq!(out.starts_at, 10, "the offset is echoed");
    }
}
