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
}

/// Structured output for `vault.find`.
///
/// rmcp requires a root `type: object` schema; this typed envelope provides it
/// while keeping the per-document payload generic `serde_json::Value` (each is
/// the JSON form of a `norn find --format json` document). See module docs.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FindOutput {
    /// Matched documents, in sort order, after limit/paging — the same
    /// per-document JSON `norn find --format json` emits. With no `col`/`all_cols`,
    /// each is `{path, frontmatter}`; projections add/narrow per the `col` syntax.
    pub documents: Vec<serde_json::Value>,
}

/// Pure handler for `vault.find`. Opens a fresh query cache (per-call freshness),
/// constructs [`FindArgs`] with `norn find`'s exact defaults (notably `limit`
/// → 10 when omitted), and runs the shared `find::query` seam.
pub fn handle(ctx: &VaultContext, p: FindParams) -> Result<FindOutput> {
    let cache = ctx.query_cache()?;

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

    let documents = crate::find::query::query(&cache, &args, None)?;
    Ok(FindOutput { documents })
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
    }
}
