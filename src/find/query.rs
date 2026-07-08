//! CLI args → crate::cache::FindQuery translation, plus the shared
//! selection/JSON seam that both `find::run` (print path) and the
//! `vault.find` MCP tool consume.

use crate::cache::{Cache, DocumentDeep, FindQuery, FindResult, SortClause, SortDirection};
use anyhow::Result;

use crate::cli::FindArgs;

/// The full result of running a find query against an open cache: the matched
/// documents plus the optional deep-fetch (headings + link sets) and `.raw`
/// disk reads, indexed parallel to `result.matches`.
///
/// This is the single selection path shared by the CLI print renderers
/// (`find::run`) and the JSON/MCP seam (`find::query`), so the two can never
/// drift on which documents match, the deep-fetch trigger, or the raw read.
pub struct Selection {
    pub result: FindResult,
    /// Deep records (headings + link sets), parallel to `result.matches`.
    /// Empty when no deep facet was requested (the cheap-facet path).
    pub deep: Vec<Option<DocumentDeep>>,
    /// `.raw` whole-file contents, parallel to `result.matches`. Empty when
    /// `.raw` was not requested.
    pub raw: Vec<Option<String>>,
}

/// Run the find query against an already-open `cache` and gather everything the
/// renderers need: matched documents, deep fetches, and raw reads.
///
/// Behavior is identical to the inline selection that previously lived in
/// `find::run` — `--links-to` resolves against the cache, the deep fetch fires
/// only for join-backed facets (or `--all-cols`), and `.raw` reads from
/// `cache.vault_root` (the canonical root, which resolves to the same files the
/// CLI's `cwd` did, so the bytes are unchanged).
pub fn select(cache: &Cache, args: &FindArgs) -> Result<Selection> {
    let mut query = build_find_query(args)?;
    // `--links-to` targets resolve against the cache (stem/alias lookup), so
    // resolution happens here rather than in the pure query builder.
    query.predicates.links_to =
        crate::filter_args::resolve_links_to(cache, &args.filters.links_to)?;
    let result = cache.find_documents(&query)?;

    // Join-backed facets (`.headings` and the link sets) require a per-doc deep
    // fetch; the cheap facets (`.frontmatter`, `.body`, `.path`) are already on
    // each `DocumentSummary`. Only pay the join cost when a deep facet is asked
    // for — the default frontmatter-only path stays at zero extra queries.
    let (facets, _fields) = crate::output::projection::split_cols(&args.col);
    let deep: Vec<Option<DocumentDeep>> = if needs_deep(&facets, args.all_cols) {
        let mut out = Vec::with_capacity(result.matches.len());
        for doc in &result.matches {
            // body not needed — find already carries `body_text`.
            out.push(cache.document_with_connections(doc.path.as_path(), false)?);
        }
        out
    } else {
        Vec::new()
    };

    // `.raw` self-loads its per-match disk read only when requested — the
    // default path does zero disk reads.
    let raw: Vec<Option<String>> = if wants_raw(&facets) {
        result
            .matches
            .iter()
            .map(|doc| crate::output::projection::read_raw(&cache.vault_root, &doc.path))
            .collect()
    } else {
        Vec::new()
    };

    Ok(Selection { result, deep, raw })
}

/// Whether a `--col` facet set (from `split_cols`) requires the per-document
/// deep fetch: `--all-cols` (which dumps every cache-served facet) or any
/// join-backed facet (`.headings` and the three link sets). Body comes from
/// `DocumentSummary.body_text`; raw is excluded by design, so `--all-cols`
/// never triggers a disk read.
///
/// Shared by [`select`] (the direct fetch decision) and the NRN-222 routed
/// reconstruction (`find::route`), so the two cannot drift on which facets
/// populate the parallel `deep` vector.
pub fn needs_deep(facets: &[String], all_cols: bool) -> bool {
    all_cols
        || facets.iter().any(|f| {
            matches!(
                f.as_str(),
                "headings" | "outgoing_links" | "unresolved_links" | "incoming_links"
            )
        })
}

/// Whether a `--col` facet set requires the per-document `.raw` disk read.
/// Shared by [`select`] and the routed reconstruction, like [`needs_deep`].
pub fn wants_raw(facets: &[String]) -> bool {
    facets.iter().any(|f| f == "raw")
}

/// The result-count envelope around a find query — the totals the CLI renderers
/// compute and emit (`render_json`, the truncation stderr note) that an MCP
/// `vault.find` consumer otherwise could not know (NRN-214). `truncated` is
/// carried explicitly (the CLI JSON omits it as `returned < total`-derivable, but
/// the MCP surface exposes it so a consumer branches on it without re-deriving).
#[derive(Debug, Clone, Copy)]
pub struct FindEnvelope {
    /// Total docs matching the predicates, BEFORE limit/offset.
    pub total: usize,
    /// Actual number returned, after limit/offset/path-glob.
    pub returned: usize,
    /// `returned < total`.
    pub truncated: bool,
    /// 1-indexed paging offset (floored at 1), as the CLI JSON envelope reports.
    pub starts_at: usize,
}

/// The JSON/MCP query seam: run the find query and map each matched document to
/// its JSON value, identical to what `find --format json` emits per document
/// (same `doc_to_json` mapping, same `--col` / `--all-cols` projection).
///
/// Returns the per-document JSON values PLUS the count envelope, so a caller can
/// surface the same `total`/`returned`/`truncated`/`starts_at` the CLI renderers
/// compute. `alias_field` is accepted for signature symmetry with the other open
/// paths; the link resolution it would inform is already baked into the passed-in
/// `cache`.
pub fn query_with_envelope(
    cache: &Cache,
    args: &FindArgs,
    _alias_field: Option<&str>,
) -> Result<(Vec<serde_json::Value>, FindEnvelope)> {
    let selection = select(cache, args)?;
    let documents = selection
        .result
        .matches
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            crate::find::render::doc_to_json(
                doc,
                selection.deep.get(i).and_then(|d| d.as_ref()),
                selection.raw.get(i).and_then(|r| r.as_deref()),
                &args.col,
                args.all_cols,
            )
        })
        .collect();
    let envelope = FindEnvelope {
        total: selection.result.total,
        returned: selection.result.returned,
        truncated: selection.result.truncated,
        starts_at: args.paging.starts_at.max(1),
    };
    Ok((documents, envelope))
}

/// The per-document JSON values without the envelope — a thin wrapper over
/// [`query_with_envelope`] for callers (e.g. `describe`) that only need the docs.
pub fn query(
    cache: &Cache,
    args: &FindArgs,
    alias_field: Option<&str>,
) -> Result<Vec<serde_json::Value>> {
    Ok(query_with_envelope(cache, args, alias_field)?.0)
}

/// Convert clap-parsed FindArgs into the cache-layer FindQuery.
pub fn build_find_query(args: &FindArgs) -> Result<FindQuery> {
    let predicates = crate::filter_args::build_document_query(&args.filters)?;

    let sort = args.paging.sort.as_ref().map(|field| SortClause {
        field: field.clone(),
        direction: if args.paging.desc {
            SortDirection::Desc
        } else {
            SortDirection::Asc
        },
    });
    // find's divergence: an absent --limit defaults to 10 (get returns all).
    let limit = if args.paging.no_limit {
        None
    } else {
        Some(args.paging.limit.unwrap_or(10))
    };

    Ok(FindQuery {
        predicates,
        sort,
        limit,
        starts_at: args.paging.starts_at.max(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_args() -> FindArgs {
        FindArgs {
            filters: crate::filter_args::FilterArgs::default(),
            paging: crate::cli::SortPaginateArgs {
                sort: None,
                desc: false,
                limit: None,
                no_limit: false,
                starts_at: 1,
            },
            format: None,
            all_cols: false,
            col: vec![],
            no_pager: false,
            all: false,
        }
    }

    #[test]
    fn empty_text_is_no_predicate() {
        let mut args = empty_args();
        args.filters.text = Some(String::new());
        let q = build_find_query(&args).unwrap();
        assert!(q.predicates.body_text_contains.is_none());
    }

    #[test]
    fn text_substring_passes_through() {
        let mut args = empty_args();
        args.filters.text = Some("SQLite".to_string());
        let q = build_find_query(&args).unwrap();
        assert_eq!(q.predicates.body_text_contains.as_deref(), Some("SQLite"));
    }

    #[test]
    fn no_limit_overrides_limit() {
        let mut args = empty_args();
        args.paging.no_limit = true;
        args.paging.limit = Some(42);
        let q = build_find_query(&args).unwrap();
        assert!(q.limit.is_none());
    }

    #[test]
    fn sort_desc_flag() {
        let mut args = empty_args();
        args.paging.sort = Some("created".to_string());
        args.paging.desc = true;
        let q = build_find_query(&args).unwrap();
        let sort = q.sort.unwrap();
        assert_eq!(sort.field, "created");
        assert_eq!(sort.direction, SortDirection::Desc);
    }

    #[test]
    fn starts_at_floors_at_one() {
        let mut args = empty_args();
        args.paging.starts_at = 0;
        let q = build_find_query(&args).unwrap();
        assert_eq!(q.starts_at, 1);
    }

    // ── seam fidelity: `query()` == the per-document JSON the print path emits ──

    use camino::{Utf8Path, Utf8PathBuf};
    use tempfile::TempDir;

    /// 3 docs: 2 `type: note`, 1 `type: task`, with bodies and a wikilink so the
    /// deep/raw facets have something to project.
    fn seeded_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-find-seam-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("note1.md"),
            "---\ntype: note\ntitle: Note One\n---\n# Heading One\nbody one [[note2]]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("note2.md"),
            "---\ntype: note\ntitle: Note Two\n---\nbody two\n",
        )
        .unwrap();
        std::fs::write(
            root.join("task1.md"),
            "---\ntype: task\ntitle: Task One\n---\nbody task\n",
        )
        .unwrap();
        (tmp, root)
    }

    /// Extract the `documents` array the CURRENT print path emits, by running the
    /// real `render_json` renderer (the golden source of truth).
    fn print_path_documents(cache: &Cache, args: &FindArgs) -> Vec<serde_json::Value> {
        let Selection { result, deep, raw } = select(cache, args).unwrap();
        let query = build_find_query(args).unwrap();
        let mut buf = Vec::new();
        crate::find::render::render(
            &result,
            &deep,
            &raw,
            args,
            crate::cli::FindFormat::Json,
            None,
            None,
            query.starts_at,
            &crate::output::palette::Palette::off(),
            &mut buf,
            &mut std::io::sink(),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        v["documents"].as_array().unwrap().clone()
    }

    fn open(root: &Utf8Path) -> Cache {
        crate::cache_cmd::open_for_query(root, &crate::graph::IndexOptions::default(), false)
            .unwrap()
    }

    #[test]
    fn query_matches_print_path_default_shape() {
        let (_tmp, root) = seeded_vault();
        let cache = open(&root);
        let mut args = empty_args();
        args.filters.eq = vec!["type:note".to_string()];

        let golden = print_path_documents(&cache, &args);
        let seam = query(&cache, &args, None).unwrap();

        assert_eq!(seam, golden, "seam must equal the print-path documents");
        assert_eq!(seam.len(), 2, "two type:note docs expected");
        assert!(seam[0].get("path").is_some());
        assert!(seam[0].get("frontmatter").is_some());
    }

    #[test]
    fn query_matches_print_path_with_col_facets() {
        let (_tmp, root) = seeded_vault();
        let cache = open(&root);
        let mut args = empty_args();
        // Exercise a deep facet, a cheap facet, and the raw disk read together.
        args.col = vec![
            "title".to_string(),
            ".headings".to_string(),
            ".outgoing_links".to_string(),
            ".body".to_string(),
            ".raw".to_string(),
        ];

        let golden = print_path_documents(&cache, &args);
        let seam = query(&cache, &args, None).unwrap();

        assert_eq!(
            seam, golden,
            "seam must equal the print-path documents under --col facets"
        );
    }

    #[test]
    fn query_matches_print_path_all_cols() {
        let (_tmp, root) = seeded_vault();
        let cache = open(&root);
        let mut args = empty_args();
        args.all_cols = true;

        let golden = print_path_documents(&cache, &args);
        let seam = query(&cache, &args, None).unwrap();

        assert_eq!(
            seam, golden,
            "seam must equal --all-cols print-path documents"
        );
    }
}
