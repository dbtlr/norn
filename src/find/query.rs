//! CLI args → crate::cache::FindQuery translation.

use crate::cache::{FindQuery, SortClause, SortDirection};
use anyhow::Result;

use crate::cli::FindArgs;

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
}
