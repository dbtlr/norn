//! SQL-direct document query — `Cache::documents_matching` and
//! `Cache::document_by_path`.

use crate::core::DocumentSummary;
use crate::standards::path_match::PathPattern;
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;

use crate::cache::canonical::strip_wikilink_brackets;
use crate::cache::error::CacheError;
use crate::cache::query::{json_path_for, DocumentQuery};

impl crate::cache::Cache {
    /// Document summaries matching the predicate set. Empty predicate set
    /// returns every document. Result ordered by `path ASC`.
    ///
    /// Frontmatter predicates push into SQL via `json_extract` with the JSON
    /// path bound as a parameter; path globs post-filter via
    /// `crate::standards::path_match::PathPattern`.
    pub fn documents_matching(
        &self,
        query: &DocumentQuery,
    ) -> Result<Vec<DocumentSummary>, CacheError> {
        let (sql, binds) = build_documents_matching_sql(query);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(binds.iter()), |row| {
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

        let mut docs: Vec<DocumentSummary> = Vec::new();
        for row in rows {
            docs.push(row?);
        }

        if !query.path_globs.is_empty() {
            docs.retain(|doc| {
                query.path_globs.iter().any(|pattern| {
                    PathPattern::parse(pattern)
                        .ok()
                        .and_then(|p| p.match_path(doc.path.as_str()))
                        .is_some()
                })
            });
        }

        Ok(docs)
    }

    /// Single document by exact vault-relative path, fully populated with
    /// headings, block_ids, outgoing links, and diagnostics. Returns `None`
    /// if the path is not in the cache.
    ///
    /// Used by `docs inspect`. Callers wanting many documents should use
    /// `documents_matching` instead — looping `document_by_path` per row
    /// triggers per-document sub-queries against the join tables and
    /// defeats the purpose of the v2 narrowing.
    pub fn document_by_path(
        &self,
        path: &Utf8Path,
    ) -> Result<Option<crate::core::Document>, CacheError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, stem, hash, frontmatter_json, body_text \
             FROM documents WHERE path = ?",
        )?;
        let row = stmt
            .query_row([path.as_str()], |row| {
                let path: String = row.get(0)?;
                let stem: String = row.get(1)?;
                let hash: String = row.get(2)?;
                let frontmatter_json: Option<String> = row.get(3)?;
                let body_text: String = row.get(4)?;
                Ok((path, stem, hash, frontmatter_json, body_text))
            })
            .optional()?;

        let Some((path_str, stem, hash, fm_json, body_text)) = row else {
            return Ok(None);
        };

        let frontmatter: Option<serde_json::Value> = fm_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let path_buf = Utf8PathBuf::from(path_str);
        let headings = crate::cache::reader::load_headings(&self.conn, path_buf.as_str())?;
        let block_ids = crate::cache::reader::load_block_ids(&self.conn, path_buf.as_str())?;
        let links = crate::cache::reader::load_links(&self.conn, path_buf.as_str())?;
        let diagnostics = crate::cache::reader::load_diagnostics(&self.conn, path_buf.as_str())?;
        // Re-derive aliases on read against the cache's configured
        // `alias_field`. See `reader::load_documents` for the rationale.
        let (aliases, alias_malformed) = match self.alias_field.as_deref() {
            Some(field) => crate::graph::parse_aliases(frontmatter.as_ref(), field),
            None => (Vec::new(), Vec::new()),
        };

        Ok(Some(crate::core::Document {
            path: path_buf,
            stem,
            hash,
            frontmatter,
            body_text,
            headings,
            block_ids,
            links,
            diagnostics,
            aliases,
            alias_malformed,
        }))
    }
}

fn build_documents_matching_sql(query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let (where_sql, binds) = build_documents_matching_sql_parts(query);
    let sql = format!(
        "SELECT path, stem, hash, frontmatter_json, body_text \
         FROM documents{} ORDER BY path",
        where_sql
    );
    (sql, binds)
}

pub(crate) fn build_documents_matching_sql_parts(query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();

    for (field, value) in &query.frontmatter_eq {
        push_equality(
            &mut where_clauses,
            &mut binds,
            field,
            value,
            /* negate */ false,
        );
    }
    for (field, value) in &query.frontmatter_not_eq {
        push_equality(
            &mut where_clauses,
            &mut binds,
            field,
            value,
            /* negate */ true,
        );
    }
    for field in &query.frontmatter_has {
        where_clauses.push("json_extract(frontmatter_json, ?) IS NOT NULL".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
    }
    for field in &query.frontmatter_missing {
        where_clauses.push("json_extract(frontmatter_json, ?) IS NULL".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
    }

    // --in field:v1,v2,...
    for (field, values) in &query.frontmatter_in {
        if values.is_empty() {
            // `--in field:` with no values matches nothing.
            where_clauses.push("0".to_string());
            continue;
        }
        if values
            .iter()
            .all(|v| matches!(v, serde_json::Value::String(_)))
        {
            push_string_membership(
                &mut where_clauses,
                &mut binds,
                field,
                values,
                /* negate */ false,
            );
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        where_clauses.push(format!(
            "json_extract(frontmatter_json, ?) IN ({})",
            placeholders
        ));
        binds.push(SqlValue::Text(json_path_for(field)));
        for v in values {
            binds.push(json_value_to_sql(v));
        }
    }

    // --not-in field:v1,v2,...
    for (field, values) in &query.frontmatter_not_in {
        if values.is_empty() {
            // `--not-in field:` with no values is a no-op.
            continue;
        }
        if values
            .iter()
            .all(|v| matches!(v, serde_json::Value::String(_)))
        {
            push_string_membership(
                &mut where_clauses,
                &mut binds,
                field,
                values,
                /* negate */ true,
            );
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        where_clauses.push(format!(
            "json_extract(frontmatter_json, ?) NOT IN ({})",
            placeholders
        ));
        binds.push(SqlValue::Text(json_path_for(field)));
        for v in values {
            binds.push(json_value_to_sql(v));
        }
    }

    // --starts-with / --ends-with / --contains field:VALUE
    for (field, needle) in &query.frontmatter_starts_with {
        push_string_operator(
            &mut where_clauses,
            &mut binds,
            field,
            needle,
            StringOperator::StartsWith,
        );
    }
    for (field, needle) in &query.frontmatter_ends_with {
        push_string_operator(
            &mut where_clauses,
            &mut binds,
            field,
            needle,
            StringOperator::EndsWith,
        );
    }
    for (field, needle) in &query.frontmatter_contains {
        push_string_operator(
            &mut where_clauses,
            &mut binds,
            field,
            needle,
            StringOperator::Contains,
        );
    }

    // --before field:DATE
    for (field, date) in &query.date_before {
        where_clauses.push("json_extract(frontmatter_json, ?) < ?".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
        binds.push(SqlValue::Text(date.clone()));
    }

    // --after field:DATE
    for (field, date) in &query.date_after {
        where_clauses.push("json_extract(frontmatter_json, ?) > ?".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
        binds.push(SqlValue::Text(date.clone()));
    }

    // --on field:DATE
    for (field, date) in &query.date_on {
        where_clauses.push("json_extract(frontmatter_json, ?) = ?".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
        binds.push(SqlValue::Text(date.clone()));
    }

    // body_text_contains: case-insensitive substring on body_text.
    if let Some(needle) = &query.body_text_contains {
        where_clauses.push("LOWER(body_text) LIKE '%' || LOWER(?) || '%'".to_string());
        binds.push(SqlValue::Text(needle.clone()));
    }

    // --links-to TARGET (resolved-only). One non-correlated IN-subquery per
    // target — ALL-of, so a doc must link to every named target. Uses
    // `idx_links_resolved`.
    for target in &query.links_to {
        where_clauses
            .push("path IN (SELECT source_path FROM links WHERE resolved_path = ?)".to_string());
        binds.push(SqlValue::Text(target.as_str().to_string()));
    }

    // --unresolved-links: docs with ≥1 link whose status = 'unresolved'.
    // Non-correlated subquery, materialized once.
    if query.has_unresolved_links {
        where_clauses.push(
            "path IN (SELECT source_path FROM links WHERE status = 'unresolved')".to_string(),
        );
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    (where_sql, binds)
}

/// Build the WHERE clause for `--eq` (negate=false) or `--not-eq` (negate=true).
/// String values get the array-aware + bracket-stripped treatment (matches
/// scalar fields by equality AND array fields via `json_each`, with `[[...]]`
/// wrappers stripped from stored values). Non-string predicates (bool, number,
/// null) keep the simple scalar equality — JSON-typed comparisons there are
/// unambiguous.
fn push_equality(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    value: &serde_json::Value,
    negate: bool,
) {
    if let serde_json::Value::String(raw) = value {
        let stripped = strip_wikilink_brackets(raw);
        let array_exists = if negate { "NOT EXISTS" } else { "EXISTS" };
        let scalar_op = if negate { "!=" } else { "=" };
        push_array_aware_clause(
            where_clauses,
            binds,
            field,
            array_exists,
            &format!("{STRIPPED_ARRAY_ELEMENT} = ?"),
            &[SqlValue::Text(stripped.clone())],
            &format!("{STRIPPED_SCALAR} {scalar_op} ?"),
            &[
                SqlValue::Text(json_path_for(field)),
                SqlValue::Text(stripped),
            ],
        );
    } else {
        let op = if negate { "!=" } else { "=" };
        where_clauses.push(format!("json_extract(frontmatter_json, ?) {op} ?"));
        binds.push(SqlValue::Text(json_path_for(field)));
        binds.push(json_value_to_sql(value));
    }
}

/// Build the WHERE clause for `--in` (negate=false) or `--not-in` (negate=true)
/// when every value in the list is a string. Same array-aware + bracket-stripped
/// shape as the string `--eq` branch: matches scalar fields with a stripped
/// equality test, matches array fields by iterating elements via `json_each`.
fn push_string_membership(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    values: &[serde_json::Value],
    negate: bool,
) {
    let placeholders = std::iter::repeat_n("?", values.len())
        .collect::<Vec<_>>()
        .join(", ");
    let stripped: Vec<SqlValue> = values
        .iter()
        .filter_map(|v| v.as_str().map(strip_wikilink_brackets))
        .map(SqlValue::Text)
        .collect();
    let array_exists = if negate { "NOT EXISTS" } else { "EXISTS" };
    let scalar_op = if negate { "NOT IN" } else { "IN" };
    let mut scalar_binds = vec![SqlValue::Text(json_path_for(field))];
    scalar_binds.extend(stripped.iter().cloned());
    push_array_aware_clause(
        where_clauses,
        binds,
        field,
        array_exists,
        &format!("{STRIPPED_ARRAY_ELEMENT} IN ({placeholders})"),
        &stripped,
        &format!("{STRIPPED_SCALAR} {scalar_op} ({placeholders})"),
        &scalar_binds,
    );
}

/// Anchored string operators for `--starts-with` / `--ends-with` / `--contains`.
#[derive(Clone, Copy)]
enum StringOperator {
    StartsWith,
    EndsWith,
    Contains,
}

impl StringOperator {
    /// SQL predicate over the text expression `val`; every `?` binds the
    /// needle. `instr`/`substr` are used instead of `LIKE` deliberately:
    /// they are case-sensitive under BINARY collation and treat `%`/`_`
    /// literally, so no wildcard escaping is needed.
    fn test(self, val: &str) -> String {
        match self {
            StringOperator::StartsWith => format!("substr({val}, 1, length(?)) = ?"),
            StringOperator::EndsWith => format!("substr({val}, -length(?)) = ?"),
            StringOperator::Contains => format!("instr({val}, ?) > 0"),
        }
    }

    fn needle_binds(self) -> usize {
        match self {
            StringOperator::Contains => 1,
            _ => 2,
        }
    }
}

/// Build the WHERE clause for one anchored string operator. Uses the shared
/// array-aware + bracket-stripped compound shape: any array element may
/// satisfy the operator; scalar fields are tested directly. Both the stored
/// value and the needle are wikilink-bracket-collapsed. Non-string stored
/// values compare by their JSON text rendering: booleans as `true`/`false`
/// (via `json_type`, since SQLite extracts them as 1/0) and numbers in JSON's
/// canonical form (`2.50` is stored and rendered as `2.5`).
fn push_string_operator(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    needle: &str,
    op: StringOperator,
) {
    let stripped = strip_wikilink_brackets(needle);
    if stripped.is_empty() {
        // An empty needle has no meaningful anchored-match semantics (only
        // reachable via a bracket-only value like `[[]]` — the flag parser
        // already rejects empty values). Match nothing, deterministically.
        where_clauses.push("0".to_string());
        return;
    }
    // Render booleans as their JSON/YAML source text, not SQLite's 1/0.
    let element_text = format!(
        "CASE WHEN json_each.type IN ('true', 'false') THEN json_each.type \
          ELSE {STRIPPED_ARRAY_ELEMENT} END"
    );
    let scalar_text = "CASE WHEN json_type(frontmatter_json, ?) IN ('true', 'false') \
          THEN json_type(frontmatter_json, ?) \
          ELSE replace(replace(CAST(json_extract(frontmatter_json, ?) AS TEXT), '[[', ''), ']]', '') END";
    let path = json_path_for(field);
    let needle_bind = SqlValue::Text(stripped);
    let array_binds = vec![needle_bind.clone(); op.needle_binds()];
    let mut scalar_binds = vec![SqlValue::Text(path); 3];
    scalar_binds.extend(vec![needle_bind; op.needle_binds()]);
    push_array_aware_clause(
        where_clauses,
        binds,
        field,
        "EXISTS",
        &op.test(&element_text),
        &array_binds,
        &op.test(scalar_text),
        &scalar_binds,
    );
}

/// Bracket-stripped text of a `json_each` array element.
const STRIPPED_ARRAY_ELEMENT: &str = "replace(replace(value, '[[', ''), ']]', '')";

/// Bracket-stripped text of a scalar field; consumes one JSON-path bind.
const STRIPPED_SCALAR: &str =
    "replace(replace(json_extract(frontmatter_json, ?), '[[', ''), ']]', '')";

/// Push the compound array-aware WHERE skeleton shared by the string forms of
/// `--eq`/`--not-eq`, `--in`/`--not-in`, and the anchored string operators: an
/// array-valued field matches when any `json_each` element passes `array_test`
/// (quantified by `array_exists`), a non-array field when it passes
/// `scalar_test`. The skeleton's four `json_type` path binds are pushed here;
/// callers pass the binds their test expressions consume, in textual order
/// (any `?` inside the expression — e.g. `STRIPPED_SCALAR`'s JSON path —
/// before the comparison operands).
#[allow(clippy::too_many_arguments)]
fn push_array_aware_clause(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    array_exists: &str,
    array_test: &str,
    array_binds: &[SqlValue],
    scalar_test: &str,
    scalar_binds: &[SqlValue],
) {
    let path = json_path_for(field);
    where_clauses.push(format!(
        "((json_type(frontmatter_json, ?) = 'array' \
          AND {array_exists} (SELECT 1 FROM json_each(frontmatter_json, ?) \
                            WHERE {array_test})) \
          OR \
          ((json_type(frontmatter_json, ?) IS NULL OR json_type(frontmatter_json, ?) != 'array') \
           AND {scalar_test}))"
    ));
    binds.push(SqlValue::Text(path.clone()));
    binds.push(SqlValue::Text(path.clone()));
    binds.extend(array_binds.iter().cloned());
    binds.push(SqlValue::Text(path.clone()));
    binds.push(SqlValue::Text(path));
    binds.extend(scalar_binds.iter().cloned());
}

/// Convert a `serde_json::Value` scalar to the native SQLite type that
/// `json_extract` returns for that same value.  This lets the `= ?` predicate
/// compare apples-to-apples: `json_extract` strips JSON encoding and returns
/// TEXT for strings, INTEGER for integers/booleans, REAL for floats, and NULL
/// for JSON null.  Objects and arrays are left JSON-encoded (TEXT) because
/// `json_extract` on an object/array column also returns JSON text.
pub(crate) fn json_value_to_sql(v: &serde_json::Value) -> SqlValue {
    match v {
        serde_json::Value::Null => SqlValue::Null,
        serde_json::Value::Bool(b) => SqlValue::Integer(if *b { 1 } else { 0 }),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                SqlValue::Text(n.to_string())
            }
        }
        serde_json::Value::String(s) => SqlValue::Text(s.clone()),
        // Objects/arrays: json_extract returns JSON text for these.
        _ => SqlValue::Text(serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())),
    }
}
