//! `vault find` cache-side types and entry point.
//!
//! `FindQuery` wraps `DocumentQuery` (the predicate set) with paging,
//! sort, and limit. `FindResult` returns matches plus the total count
//! (before limit/offset) so callers can emit accurate truncation
//! signals.

use crate::core::DocumentSummary;

use crate::cache::query::DocumentQuery;

/// Top-level query for `Cache::find_documents`. Predicates come from
/// `DocumentQuery`; this struct adds the presentation-layer concerns
/// (sort, limit, paging) that aren't query-shape predicates.
#[derive(Default, Debug, Clone)]
pub struct FindQuery {
    pub predicates: DocumentQuery,
    /// None = no sort (results in stable path order by default).
    pub sort: Option<SortClause>,
    /// None = no limit (every match returned). Default 10 lives in the
    /// CLI layer, not here — at the Cache layer, `None` means unlimited.
    pub limit: Option<usize>,
    /// 1-indexed. `1` = start from the first match. Values of 0 are
    /// treated like 1 by `offset()` (saturating_sub). Default-constructed
    /// FindQuery (`..Default::default()`) yields `starts_at = 0`, which
    /// `offset()` normalizes to 0 — the first match.
    pub starts_at: usize,
}

impl FindQuery {
    /// Effective starting offset (0-indexed) for SQL OFFSET.
    pub(crate) fn offset(&self) -> usize {
        self.starts_at.saturating_sub(1)
    }
}

#[derive(Debug, Clone)]
pub struct SortClause {
    /// Field name. `"path"` and `"stem"` are virtual (resolved to the
    /// corresponding columns); everything else is a frontmatter key
    /// resolved via `json_extract(frontmatter_json, ?)`.
    pub field: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// Result of `Cache::find_documents`. Carries the matched documents plus
/// the totals that the CLI layer needs to emit truncation signals
/// without re-querying.
#[derive(Debug, Clone)]
pub struct FindResult {
    pub matches: Vec<DocumentSummary>,
    /// Total docs matching the predicates, BEFORE limit/offset.
    pub total: usize,
    /// Actual length of `matches` (after limit/offset/path-glob post-pass).
    pub returned: usize,
    /// `returned < total`.
    pub truncated: bool,
}

impl crate::cache::Cache {
    /// SQL-direct find with ORDER BY / LIMIT / OFFSET / COUNT.
    /// Path globs are applied as a Rust post-pass after SQL narrowing —
    /// `total` reflects the post-pass count (same as `matches.len()`
    /// would if `limit` were `None`).
    pub fn find_documents(
        &self,
        query: &FindQuery,
    ) -> Result<FindResult, crate::cache::error::CacheError> {
        use crate::cache::query_documents::build_documents_matching_sql_parts;
        use crate::standards::path_match::PathPattern;
        use camino::Utf8PathBuf;
        use rusqlite::params_from_iter;
        use rusqlite::types::Value as SqlValue;

        let (where_sql, where_binds) = build_documents_matching_sql_parts(&query.predicates);

        let (order_by_sql, order_by_binds) = sort_clause_sql(query.sort.as_ref());

        let needs_post_pass = !query.predicates.path_globs.is_empty();
        let limit = query.limit.unwrap_or(usize::MAX);
        let offset = query.offset();

        let sql = if needs_post_pass {
            format!(
                "SELECT path, stem, hash, frontmatter_json, body_text \
                 FROM documents{} ORDER BY {}, path ASC",
                where_sql, order_by_sql
            )
        } else {
            format!(
                "SELECT path, stem, hash, frontmatter_json, body_text \
                 FROM documents{} ORDER BY {}, path ASC LIMIT ? OFFSET ?",
                where_sql, order_by_sql
            )
        };

        // Build the SELECT binds (where + order_by + optional LIMIT/OFFSET).
        // Keep `where_binds` separate so we can reuse it for COUNT(*).
        let mut select_binds: Vec<SqlValue> = where_binds.clone();
        select_binds.extend(order_by_binds);
        if !needs_post_pass {
            select_binds.push(SqlValue::Integer(limit as i64));
            select_binds.push(SqlValue::Integer(offset as i64));
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(select_binds.iter()), |row| {
            let path: String = row.get(0)?;
            let stem: String = row.get(1)?;
            let hash: String = row.get(2)?;
            let frontmatter_json: Option<String> = row.get(3)?;
            let body_text: String = row.get(4)?;
            let frontmatter = frontmatter_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            Ok(DocumentSummary {
                path: Utf8PathBuf::from(path),
                stem,
                hash,
                frontmatter,
                body_text,
            })
        })?;
        let mut all_matches: Vec<DocumentSummary> = Vec::new();
        for row in rows {
            all_matches.push(row?);
        }

        let (total, matches) = if needs_post_pass {
            // Path-glob post-pass: filter all results in Rust, then apply
            // OFFSET/LIMIT in Rust too. `total` = count after glob filtering.
            all_matches.retain(|doc| {
                query.predicates.path_globs.iter().any(|pattern| {
                    PathPattern::parse(pattern)
                        .ok()
                        .and_then(|p| p.match_path(doc.path.as_str()))
                        .is_some()
                })
            });
            let total = all_matches.len();
            let matches = all_matches.into_iter().skip(offset).take(limit).collect();
            (total, matches)
        } else {
            // SQL-paged path: issue a separate COUNT(*) query for the total.
            let count_sql = format!("SELECT COUNT(*) FROM documents{}", where_sql);
            let total: i64 =
                self.conn
                    .query_row(&count_sql, params_from_iter(where_binds.iter()), |row| {
                        row.get(0)
                    })?;
            (total as usize, all_matches)
        };

        let returned = matches.len();
        let truncated = returned < total;

        Ok(FindResult {
            matches,
            total,
            returned,
            truncated,
        })
    }
}

/// Resolve a `SortClause` into a SQL ORDER BY fragment + any binds.
/// Returns `"path ASC"` when no sort is specified (the default stable order).
fn sort_clause_sql(sort: Option<&SortClause>) -> (String, Vec<rusqlite::types::Value>) {
    use crate::cache::query::json_path_for;
    use rusqlite::types::Value as SqlValue;

    let Some(sort) = sort else {
        return ("path ASC".to_string(), Vec::new());
    };

    let direction = match sort.direction {
        SortDirection::Asc => "ASC",
        SortDirection::Desc => "DESC",
    };
    match sort.field.as_str() {
        "path" => (format!("path {}", direction), Vec::new()),
        "stem" => (format!("stem {}", direction), Vec::new()),
        field => (
            format!("json_extract(frontmatter_json, ?) {}", direction),
            vec![SqlValue::Text(json_path_for(field))],
        ),
    }
}

#[cfg(test)]
mod tests {
    //! Round-trip property tests for `Cache::find_documents`: verify
    //! ORDER BY / LIMIT / OFFSET / COUNT semantics across realistic vaults.

    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    use crate::cache::{Cache, DocumentQuery, FindQuery, FindResult, SortClause, SortDirection};

    fn synth_paged_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        // 25 docs: doc-01.md through doc-25.md, each with `priority: <N>`.
        for i in 1..=25 {
            let priority = (i * 7) % 100;
            std::fs::write(
                root.join(format!("doc-{:02}.md", i)).as_std_path(),
                format!("---\ntype: note\npriority: {}\n---\nbody-{}\n", priority, i),
            )
            .unwrap();
        }
        (tmp, root)
    }

    fn paths(result: &FindResult) -> Vec<&str> {
        result.matches.iter().map(|d| d.path.as_str()).collect()
    }

    #[test]
    fn empty_query_default_paging() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            starts_at: 1,
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(result.total, 25);
        assert_eq!(result.returned, 25);
        assert!(!result.truncated);
        assert_eq!(result.matches[0].path.as_str(), "doc-01.md");
        assert_eq!(result.matches[24].path.as_str(), "doc-25.md");
    }

    #[test]
    fn limit_truncates_signals_total() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            starts_at: 1,
            limit: Some(10),
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(result.total, 25);
        assert_eq!(result.returned, 10);
        assert!(result.truncated);
        assert_eq!(paths(&result)[0], "doc-01.md");
        assert_eq!(paths(&result)[9], "doc-10.md");
    }

    #[test]
    fn paging_starts_at_returns_window() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            starts_at: 11,
            limit: Some(10),
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(result.total, 25);
        assert_eq!(result.returned, 10);
        assert_eq!(paths(&result)[0], "doc-11.md");
        assert_eq!(paths(&result)[9], "doc-20.md");
    }

    #[test]
    fn paging_beyond_end_returns_empty() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            starts_at: 100,
            limit: Some(10),
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(result.total, 25);
        assert_eq!(result.returned, 0);
        assert!(result.truncated); // 0 returned but 25 total → truncated
    }

    #[test]
    fn sort_by_frontmatter_field_asc() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            starts_at: 1,
            limit: Some(3),
            sort: Some(SortClause {
                field: "priority".to_string(),
                direction: SortDirection::Asc,
            }),
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        let prios: Vec<i64> = result
            .matches
            .iter()
            .map(|d| {
                d.frontmatter.as_ref().unwrap()["priority"]
                    .as_i64()
                    .unwrap()
            })
            .collect();
        let mut sorted_prios = prios.clone();
        sorted_prios.sort();
        assert_eq!(prios, sorted_prios);
    }

    #[test]
    fn sort_by_path_desc() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            starts_at: 1,
            limit: Some(3),
            sort: Some(SortClause {
                field: "path".to_string(),
                direction: SortDirection::Desc,
            }),
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(paths(&result), vec!["doc-25.md", "doc-24.md", "doc-23.md"]);
    }

    #[test]
    fn predicate_narrows_then_sort_and_limit() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let q = FindQuery {
            predicates: DocumentQuery {
                frontmatter_eq: vec![("type".to_string(), serde_json::json!("note"))],
                ..Default::default()
            },
            starts_at: 1,
            limit: Some(5),
            sort: Some(SortClause {
                field: "path".to_string(),
                direction: SortDirection::Asc,
            }),
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(result.total, 25);
        assert_eq!(result.returned, 5);
        assert!(result.truncated);
        assert_eq!(paths(&result)[0], "doc-01.md");
        assert_eq!(paths(&result)[4], "doc-05.md");
    }

    #[test]
    fn path_glob_post_pass_affects_total_and_matches() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        // Glob matches docs ending in 0 (doc-10.md, doc-20.md). doc-01..09 don't.
        let q = FindQuery {
            predicates: DocumentQuery {
                path_globs: vec!["doc-*0.md".to_string()],
                ..Default::default()
            },
            starts_at: 1,
            limit: Some(100),
            ..Default::default()
        };
        let result = cache.find_documents(&q).unwrap();

        assert_eq!(result.total, 2);
        assert_eq!(result.returned, 2);
        assert!(!result.truncated);
    }

    #[test]
    fn find_documents_query_plan_is_single_select() {
        let (_tmp, root) = synth_paged_vault();
        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let conn = cache.conn();
        let mut stmt = conn
            .prepare(
                "EXPLAIN QUERY PLAN \
             SELECT path, stem, hash, frontmatter_json, body_text \
             FROM documents WHERE json_extract(frontmatter_json, ?) = ? \
             ORDER BY json_extract(frontmatter_json, ?) ASC, path ASC \
             LIMIT ? OFFSET ?",
            )
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map(
                rusqlite::params![r#"$."type""#, "note", r#"$."priority""#, 10i64, 0i64],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            !rows.iter().any(|r| r.contains("SUBQUERY")),
            "find_documents plan contains a subquery: {:?}",
            rows
        );
        assert!(
            rows.iter().any(|r| r.contains("documents")),
            "plan does not mention documents table: {:?}",
            rows
        );
    }
}
