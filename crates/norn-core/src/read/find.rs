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
use crate::query::filter_args::build_document_query;

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
    let limit = if paging.no_limit {
        None
    } else {
        Some(paging.limit.unwrap_or(DEFAULT_LIMIT))
    };
    let starts_at = paging.starts_at.max(1);

    let query = FindQuery {
        predicates,
        sort,
        limit,
        starts_at,
    };

    let result = cache.find_documents(&query)?;

    let documents = result
        .matches
        .into_iter()
        .map(|doc| FindDoc {
            path: doc.path.to_string(),
            stem: doc.stem,
            hash: doc.hash,
            frontmatter: doc.frontmatter,
            body_text: doc.body_text,
        })
        .collect();

    Ok(Ok(FindReport {
        documents,
        total: result.total,
        returned: result.returned,
        starts_at,
        truncated: result.truncated,
    }))
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
