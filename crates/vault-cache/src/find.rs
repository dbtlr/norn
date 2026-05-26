//! `vault find` cache-side types and entry point.
//!
//! `FindQuery` wraps `DocumentQuery` (the predicate set) with paging,
//! sort, and limit. `FindResult` returns matches plus the total count
//! (before limit/offset) so callers can emit accurate truncation
//! signals.

use vault_core::DocumentSummary;

use crate::query::DocumentQuery;

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

impl crate::Cache {
    /// SQL-direct find with ORDER BY / LIMIT / OFFSET / COUNT.
    /// Path globs are applied as a Rust post-pass after SQL narrowing —
    /// `total` reflects the post-pass count (same as `matches.len()`
    /// would if `limit` were `None`).
    pub fn find_documents(
        &self,
        query: &FindQuery,
    ) -> Result<FindResult, crate::error::CacheError> {
        use crate::query_documents::build_documents_matching_sql_parts;
        use camino::Utf8PathBuf;
        use rusqlite::params_from_iter;
        use rusqlite::types::Value as SqlValue;
        use vault_standards::path_match::PathPattern;

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
    use crate::query::json_path_for;
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
