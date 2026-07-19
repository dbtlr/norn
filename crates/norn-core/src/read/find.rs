//! The `find` verb's execute seam (the 0016 Params/execute/Report vocabulary).
//!
//! Ported from the donor `src/find/`: build the predicate query from the wire
//! [`FindParams`], resolve `--links-to` targets against the warm cache, run the
//! sorted/paged [`Cache::find_documents`](crate::cache::Cache::find_documents),
//! and project each match into the flat [`FindDoc`] the CLI renders.
//!
//! # Error split (owner exit-to-heal boundary)
//!
//! [`execute`] returns `anyhow::Result<Result<FindReport, String>>`:
//!
//! - `Ok(Ok(report))` — the query ran.
//! - `Ok(Err(message))` — a **user** error (a malformed predicate, an
//!   unresolvable/ambiguous `--links-to` target). The owner is healthy; it
//!   reports the message and keeps serving.
//! - `Err(_)` — a **cache** error surfaced by the read. The owner treats it as
//!   exit-to-heal (the db is disposable derivation).
//!
//! # Find-only limit default
//!
//! An absent `--limit` defaults to **10** here (the donor's `build_find_query`
//! divergence), unless `--no-limit` is set. The wire carries the absent limit
//! verbatim; the default is the verb's, applied at execute.

use anyhow::Result;

use norn_wire::{FindDoc, FindParams, FindReport};

use crate::cache::{Cache, FindQuery, SortClause, SortDirection};
use crate::domain::DocumentSummary;
use crate::query::filter_args::build_document_query;
use crate::read::{connection_values, ConnectionValues};

/// The find-only default limit applied when `--limit` is absent and
/// `--no-limit` is not set (donor parity).
const DEFAULT_LIMIT: usize = 10;

/// Run a `find` request against the warm cache. See the module docs for the
/// `Ok(Ok)` / `Ok(Err)` / `Err` contract.
pub fn execute(
    cache: &Cache,
    params: &FindParams,
    today: &str,
) -> Result<Result<FindReport, String>> {
    // Predicate build — a malformed predicate token is a user error.
    let mut predicates = match build_document_query(&params.filter, today) {
        Ok(q) => q,
        Err(e) => return Ok(Err(e.to_string())),
    };

    // `--links-to` resolution needs the warm graph (a target string → a vault
    // path). None/ambiguous is a user error; a cache read failure propagates.
    if !params.filter.links_to.is_empty() {
        let index = cache.load_graph_index()?;
        for target in &params.filter.links_to {
            match crate::target::resolve_target_path(&index, target) {
                Ok(path) => predicates.links_to.push(path),
                Err(e) => return Ok(Err(e.to_string())),
            }
        }
    }

    let paging = &params.paging;
    let sort = paging.sort.as_ref().map(|field| SortClause {
        field: field.clone(),
        direction: if paging.desc {
            SortDirection::Desc
        } else {
            SortDirection::Asc
        },
    });
    // `no_limit` wins over `limit` when both arrive — the CLI already resolves
    // the last-wins competition (NRN-331), and this is the wire precedence a raw
    // MCP caller relies on (see `SortPaginateParams` docs).
    let limit = if paging.no_limit {
        None
    } else {
        Some(paging.limit.unwrap_or(DEFAULT_LIMIT))
    };
    // `starts_at` is a zero-indexed offset (NRN-332): used verbatim, no floor.
    let offset = paging.starts_at;

    let query = FindQuery {
        predicates,
        sort,
        limit,
        starts_at: offset,
    };

    let result = cache.find_documents(&query)?;

    let documents = result
        .matches
        .into_iter()
        .map(|doc| project_match(cache, doc, params.with_connections))
        .collect::<Result<Vec<_>>>()?;

    Ok(Ok(FindReport {
        documents,
        total: result.total,
        returned: result.returned,
        // The report echoes the 1-based position of the first returned record
        // (offset + 1) — the "showing N–M" human line and the JSON envelope stay
        // 1-based, so a default (offset 0) query is unchanged from the oracle.
        starts_at: offset.saturating_add(1),
        truncated: result.truncated,
    }))
}

/// Project one matched summary into the flat wire [`FindDoc`], loading the deep
/// connection facets (headings + link sets) only when `with_connections` is set
/// — a plain `find` never pays the per-match connection load.
fn project_match(cache: &Cache, doc: DocumentSummary, with_connections: bool) -> Result<FindDoc> {
    let conns = if with_connections {
        // A match resolved to a path missing from the deep read is a torn cache
        // read; treat empty connections as the honest answer rather than failing
        // the whole query (the row still exists in the summary scan).
        match cache.document_with_connections(doc.path.as_path(), false)? {
            Some(deep) => connection_values(&deep)?,
            None => ConnectionValues::empty(),
        }
    } else {
        ConnectionValues::empty()
    };
    Ok(FindDoc {
        path: doc.path.to_string(),
        stem: doc.stem,
        hash: doc.hash,
        frontmatter: doc.frontmatter,
        body_text: doc.body_text,
        headings: conns.headings,
        outgoing_links: conns.outgoing_links,
        unresolved_links: conns.unresolved_links,
        incoming_links: conns.incoming_links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use norn_wire::{FilterParams, SortPaginateParams};
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-18";

    fn vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        for (name, ty) in [("a", "note"), ("b", "task"), ("c", "note")] {
            std::fs::write(
                root.join(format!("{name}.md")).as_std_path(),
                format!("---\ntype: {ty}\ntitle: {name}\n---\nbody of {name}\n"),
            )
            .unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    #[test]
    fn find_all_defaults_limit_to_ten_and_returns_every_doc() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let report = execute(&cache, &FindParams::default(), TODAY)
            .unwrap()
            .unwrap();
        // 3 docs, under the default-10 limit → all returned, not truncated.
        assert_eq!(report.total, 3);
        assert_eq!(report.returned, 3);
        assert!(!report.truncated);
    }

    #[test]
    fn eq_predicate_narrows() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = FindParams {
            filter: FilterParams {
                eq: vec!["type:note".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        assert_eq!(report.total, 2);
        let paths: Vec<&str> = report.documents.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(paths, vec!["a.md", "c.md"]);
    }

    #[test]
    fn limit_truncates_and_signals_total() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = FindParams {
            paging: SortPaginateParams {
                limit: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        assert_eq!(report.total, 3);
        assert_eq!(report.returned, 1);
        assert!(report.truncated);
    }

    #[test]
    fn default_offset_starts_at_first_record_and_reports_one() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let report = execute(&cache, &FindParams::default(), TODAY)
            .unwrap()
            .unwrap();
        // Zero-indexed default offset 0 → first record, echoed 1-based.
        assert_eq!(report.starts_at, 1);
        assert_eq!(report.documents[0].path, "a.md");
    }

    #[test]
    fn starts_at_one_skips_the_first_record() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        // `--sort path` for a stable order; offset 1 skips a.md (NRN-332).
        let params = FindParams {
            paging: SortPaginateParams {
                sort: Some("path".into()),
                starts_at: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        assert_eq!(report.documents[0].path, "b.md", "offset 1 = second record");
        assert_eq!(report.starts_at, 2, "1-based echo of offset 1");
        assert_eq!(report.total, 3);
    }

    #[test]
    fn no_limit_wins_over_limit_when_both_present() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        // A raw caller sends both; `no_limit` must win (wire precedence, NRN-331).
        let params = FindParams {
            paging: SortPaginateParams {
                limit: Some(1),
                no_limit: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let report = execute(&cache, &params, TODAY).unwrap().unwrap();
        assert_eq!(report.returned, 3, "no_limit overrides the limit of 1");
        assert!(!report.truncated);
    }

    #[test]
    fn bad_predicate_is_a_user_error_not_a_cache_error() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = FindParams {
            filter: FilterParams {
                eq: vec!["nocolon".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let outcome = execute(&cache, &params, TODAY).unwrap();
        assert!(outcome.is_err(), "malformed --eq must be a user error");
    }

    #[test]
    fn connections_absent_by_default_present_when_requested() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("a.md").as_std_path(),
            "---\ntype: note\n---\n# A heading\n[[b]]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\ntype: note\n---\n# B heading\n[[a]]\n",
        )
        .unwrap();
        let mut cache = Cache::open(&root).unwrap();
        cache.full_build(&root).unwrap();

        // Default: no connection facets loaded.
        let plain = execute(&cache, &FindParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert!(plain
            .documents
            .iter()
            .all(|d| d.headings.is_empty() && d.outgoing_links.is_empty()));

        // with_connections: each match carries its deep facets.
        let params = FindParams {
            with_connections: true,
            ..Default::default()
        };
        let deep = execute(&cache, &params, TODAY).unwrap().unwrap();
        let a = deep
            .documents
            .iter()
            .find(|d| d.path == "a.md")
            .expect("a.md present");
        assert!(
            a.headings.iter().any(|h| h["text"] == "A heading"),
            "expected the 'A heading' value for a.md, got {:?}",
            a.headings
        );
        assert!(
            a.outgoing_links.iter().any(|l| l["target"] == "b"),
            "expected outgoing link to b, got {:?}",
            a.outgoing_links
        );
        assert!(
            a.incoming_links.iter().any(|l| l["source_path"] == "b.md"),
            "expected incoming link from b.md, got {:?}",
            a.incoming_links
        );
    }

    #[test]
    fn unresolvable_links_to_is_a_user_error() {
        let (_tmp, root) = vault();
        let cache = built(&root);
        let params = FindParams {
            filter: FilterParams {
                links_to: vec!["does-not-exist".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let outcome = execute(&cache, &params, TODAY).unwrap();
        assert!(outcome.is_err());
    }
}
