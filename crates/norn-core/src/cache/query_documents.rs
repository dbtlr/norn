//! SQL-direct document query — `Cache::documents_matching` and
//! `Cache::document_by_path`, plus the predicate SQL emission ported from the
//! donor query layer (ADR 0004 EAV routing).
//!
//! `build_documents_matching_sql_parts` is the query router: it decides, per
//! query, whether every predicate can be answered from the derived
//! `document_fields` EAV table (`build_documents_matching_sql_parts_eav`) or must
//! fall back to the original `json_extract` scan
//! (`build_documents_matching_sql_parts_scan`). See `eav_eligible_fields` for
//! the exact provable/fallback boundary.
//!
//! The SQL-agnostic [`DocumentQuery`](crate::query::DocumentQuery) predicate
//! model lives in `crate::query`; this module is the run side (SQL emission) —
//! the deliberate seam left behind by NRN-342.

use std::collections::BTreeSet;

use crate::domain::DocumentSummary;
use crate::query::DocumentQuery;
use crate::standards::path_match::PathPattern;
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;

use crate::cache::canonical::{canonicalize_scalar, strip_wikilink_brackets};
use crate::cache::error::CacheError;
use crate::cache::Cache;

/// Encode a frontmatter field name as a single quoted JSON-path segment for
/// SQLite's `json_extract`. Returns the full path string `$."<escaped>"`.
///
/// SQLite parses the path at statement execution; binding this as a parameter
/// (not interpolating it) is what closes the SQL-injection vector and lets
/// frontmatter keys contain any character.
pub(crate) fn json_path_for(field: &str) -> String {
    let escaped = field.replace('\\', r"\\").replace('"', r#"\""#);
    format!(r#"$."{escaped}""#)
}

impl crate::cache::Cache {
    /// Document summaries matching the predicate set. Empty predicate set returns
    /// every document. Result ordered by `path ASC`.
    pub fn documents_matching(
        &self,
        query: &DocumentQuery,
    ) -> Result<Vec<DocumentSummary>, CacheError> {
        let (sql, binds) = build_documents_matching_sql(self, query);
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
    /// headings, block_ids, outgoing links, and diagnostics. Returns `None` if
    /// the path is not in the cache.
    pub fn document_by_path(
        &self,
        path: &Utf8Path,
    ) -> Result<Option<crate::domain::Document>, CacheError> {
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
        let (aliases, alias_malformed) = match self.alias_field.as_deref() {
            Some(field) => crate::graph::parse_aliases(frontmatter.as_ref(), field),
            None => (Vec::new(), Vec::new()),
        };

        Ok(Some(crate::domain::Document {
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

fn build_documents_matching_sql(cache: &Cache, query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let (where_sql, binds) = build_documents_matching_sql_parts(cache, query);
    let sql = format!(
        "SELECT path, stem, hash, frontmatter_json, body_text \
         FROM documents{where_sql} ORDER BY path"
    );
    (sql, binds)
}

/// Query router: decide whether `query` can be answered entirely from the
/// `document_fields` derived index, or must fall back to the `json_extract` scan
/// path. All-or-nothing per query (ADR 0004).
///
/// Routes to the EAV plan iff every frontmatter field referenced by an
/// EAV-eligible predicate is in the cache's declared index set AND every
/// predicate present belongs to an EAV-provable class (`eav_eligible_fields`).
/// Any other combination falls back to the scan path. Falling back because a
/// field is simply undeclared/unindexed warns (once, cold-scan only). The engine
/// is always authoritative (the db is created under a known index set at summon),
/// so there is no non-authoritative open to suppress routing.
pub(crate) fn build_documents_matching_sql_parts(
    cache: &Cache,
    query: &DocumentQuery,
) -> (String, Vec<SqlValue>) {
    if let Some(fields) = eav_eligible_fields(query) {
        if !fields.is_empty() {
            let unindexed: BTreeSet<String> = fields
                .into_iter()
                .filter(|f| !cache.index_set.contains(f))
                .collect();
            if unindexed.is_empty() {
                return build_documents_matching_sql_parts_eav(query);
            }
            warn_cold_scan(cache, &unindexed);
        }
    }
    build_documents_matching_sql_parts_scan(query)
}

/// Emit the cold-scan warning when falling back to the scan path solely because
/// one or more referenced fields aren't in the declared index set. Silent under
/// the 1,000-row threshold — this is a perf hint, not a correctness signal.
fn warn_cold_scan(cache: &Cache, unindexed_fields: &BTreeSet<String>) {
    if unindexed_fields.is_empty() {
        return;
    }
    let count: i64 = cache
        .conn
        .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
        .unwrap_or(0);
    if count < 1000 {
        return;
    }
    let joined = unindexed_fields
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "warning: scanned {count} documents on unindexed field(s) {joined}; \
         declare indexed: true (or a bounded type) to accelerate"
    );
}

/// Every frontmatter field referenced by an EAV-provable predicate in `query`,
/// or `None` if the query contains any predicate class this router doesn't (yet)
/// prove equivalent to the scan path.
fn eav_eligible_fields(query: &DocumentQuery) -> Option<BTreeSet<String>> {
    let eav_scalar = |v: &serde_json::Value| {
        matches!(
            v,
            serde_json::Value::String(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_)
        )
    };
    let mut fields: BTreeSet<String> = BTreeSet::new();

    fields.extend(query.frontmatter_starts_with.iter().map(|(f, _)| f.clone()));
    fields.extend(query.frontmatter_ends_with.iter().map(|(f, _)| f.clone()));
    fields.extend(query.frontmatter_contains.iter().map(|(f, _)| f.clone()));
    fields.extend(query.date_before.iter().map(|(f, _)| f.clone()));
    fields.extend(query.date_after.iter().map(|(f, _)| f.clone()));
    fields.extend(query.date_on.iter().map(|(f, _)| f.clone()));

    for (field, value) in &query.frontmatter_eq {
        if !eav_scalar(value) {
            return None;
        }
        fields.insert(field.clone());
    }
    for (field, value) in &query.frontmatter_not_eq {
        if !eav_scalar(value) {
            return None;
        }
        fields.insert(field.clone());
    }
    fields.extend(query.frontmatter_has.iter().cloned());
    fields.extend(query.frontmatter_missing.iter().cloned());
    for (field, values) in &query.frontmatter_in {
        if values.is_empty() {
            continue;
        }
        if !values.iter().all(&eav_scalar) {
            return None;
        }
        fields.insert(field.clone());
    }
    for (field, values) in &query.frontmatter_not_in {
        if values.is_empty() {
            continue;
        }
        if !values.iter().all(&eav_scalar) {
            return None;
        }
        fields.insert(field.clone());
    }

    Some(fields)
}

/// EAV compilation of the provable predicate classes (see `eav_eligible_fields`).
fn build_documents_matching_sql_parts_eav(query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();

    for (field, value) in &query.frontmatter_eq {
        where_clauses.push(
            "path IN (SELECT path FROM document_fields WHERE key = ? AND value = ?)".to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
        binds.push(canonicalize_scalar(value));
    }

    for (field, value) in &query.frontmatter_not_eq {
        where_clauses.push(
            "NOT EXISTS (SELECT 1 FROM document_fields f WHERE f.path = documents.path \
             AND f.key = ? AND (f.value = ? OR f.value = x'00'))"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
        binds.push(canonicalize_scalar(value));
    }

    for field in &query.frontmatter_has {
        where_clauses.push(
            "EXISTS (SELECT 1 FROM document_fields f WHERE f.path = documents.path \
             AND f.key = ? AND f.value IS NOT x'00')"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
    }

    for field in &query.frontmatter_missing {
        where_clauses.push(
            "path IN (SELECT path FROM document_fields WHERE key = ? AND value = x'00')"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
    }

    for (field, values) in &query.frontmatter_in {
        if values.is_empty() {
            where_clauses.push("0".to_string());
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        where_clauses.push(format!(
            "path IN (SELECT path FROM document_fields WHERE key = ? AND value IN ({placeholders}))"
        ));
        binds.push(SqlValue::Text(field.clone()));
        for v in values {
            binds.push(canonicalize_scalar(v));
        }
    }

    for (field, values) in &query.frontmatter_not_in {
        if values.is_empty() {
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        where_clauses.push(format!(
            "NOT EXISTS (SELECT 1 FROM document_fields f WHERE f.path = documents.path \
             AND f.key = ? AND (f.value IN ({placeholders}) OR f.value = x'00'))"
        ));
        binds.push(SqlValue::Text(field.clone()));
        for v in values {
            binds.push(canonicalize_scalar(v));
        }
    }

    for (field, needle) in &query.frontmatter_starts_with {
        push_string_operator_eav(
            &mut where_clauses,
            &mut binds,
            field,
            needle,
            StringOperator::StartsWith,
        );
    }
    for (field, needle) in &query.frontmatter_ends_with {
        push_string_operator_eav(
            &mut where_clauses,
            &mut binds,
            field,
            needle,
            StringOperator::EndsWith,
        );
    }
    for (field, needle) in &query.frontmatter_contains {
        push_string_operator_eav(
            &mut where_clauses,
            &mut binds,
            field,
            needle,
            StringOperator::Contains,
        );
    }

    for (field, date) in &query.date_before {
        push_date_op_eav(&mut where_clauses, &mut binds, field, date, "<");
    }
    for (field, date) in &query.date_after {
        push_date_op_eav(&mut where_clauses, &mut binds, field, date, ">");
    }
    for (field, date) in &query.date_on {
        push_date_op_eav(&mut where_clauses, &mut binds, field, date, "=");
    }

    push_shared_tail_clauses(&mut where_clauses, &mut binds, query);

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };
    (where_sql, binds)
}

/// Original `json_extract` scan compilation — the router's universal fallback.
pub(crate) fn build_documents_matching_sql_parts_scan(
    query: &DocumentQuery,
) -> (String, Vec<SqlValue>) {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();

    for (field, value) in &query.frontmatter_eq {
        push_equality(&mut where_clauses, &mut binds, field, value, false);
    }
    for (field, value) in &query.frontmatter_not_eq {
        push_equality(&mut where_clauses, &mut binds, field, value, true);
    }
    for field in &query.frontmatter_has {
        where_clauses.push("json_extract(frontmatter_json, ?) IS NOT NULL".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
    }
    for field in &query.frontmatter_missing {
        where_clauses.push("json_extract(frontmatter_json, ?) IS NULL".to_string());
        binds.push(SqlValue::Text(json_path_for(field)));
    }

    for (field, values) in &query.frontmatter_in {
        if values.is_empty() {
            where_clauses.push("0".to_string());
            continue;
        }
        if all_scalar_values(values) {
            push_scalar_membership(&mut where_clauses, &mut binds, field, values, false);
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        where_clauses.push(format!(
            "json_extract(frontmatter_json, ?) IN ({placeholders})"
        ));
        binds.push(SqlValue::Text(json_path_for(field)));
        for v in values {
            binds.push(json_value_to_sql(v));
        }
    }

    for (field, values) in &query.frontmatter_not_in {
        if values.is_empty() {
            continue;
        }
        if all_scalar_values(values) {
            push_scalar_membership(&mut where_clauses, &mut binds, field, values, true);
            continue;
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        where_clauses.push(format!(
            "json_extract(frontmatter_json, ?) NOT IN ({placeholders})"
        ));
        binds.push(SqlValue::Text(json_path_for(field)));
        for v in values {
            binds.push(json_value_to_sql(v));
        }
    }

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

    for (field, date) in &query.date_before {
        push_date_op(&mut where_clauses, &mut binds, field, date, "<");
    }
    for (field, date) in &query.date_after {
        push_date_op(&mut where_clauses, &mut binds, field, date, ">");
    }
    for (field, date) in &query.date_on {
        push_date_op(&mut where_clauses, &mut binds, field, date, "=");
    }

    push_shared_tail_clauses(&mut where_clauses, &mut binds, query);

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    (where_sql, binds)
}

/// Predicate classes that never touch `document_fields` (body-text, links-to,
/// unresolved-links), shared verbatim between the scan and EAV builders.
fn push_shared_tail_clauses(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    query: &DocumentQuery,
) {
    if let Some(needle) = &query.body_text_contains {
        where_clauses.push("LOWER(body_text) LIKE '%' || LOWER(?) || '%'".to_string());
        binds.push(SqlValue::Text(needle.clone()));
    }

    for target in &query.links_to {
        where_clauses
            .push("path IN (SELECT source_path FROM links WHERE resolved_path = ?)".to_string());
        binds.push(SqlValue::Text(target.as_str().to_string()));
    }

    if query.has_unresolved_links {
        where_clauses.push(
            "path IN (SELECT source_path FROM links WHERE status = 'unresolved')".to_string(),
        );
    }
}

/// Build the WHERE clause for one date operator (array-aware EXISTS-any over the
/// same skeleton as string `--eq`, comparing bracket-stripped text on both
/// sides).
fn push_date_op(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    date: &str,
    op: &str,
) {
    let stripped = strip_wikilink_brackets(date);
    push_array_aware_clause(
        where_clauses,
        binds,
        field,
        "EXISTS",
        &format!("{STRIPPED_ARRAY_ELEMENT} {op} ?"),
        &[SqlValue::Text(stripped.clone())],
        &format!("{STRIPPED_SCALAR} {op} ?"),
        &[
            SqlValue::Text(json_path_for(field)),
            SqlValue::Text(stripped),
        ],
    );
}

/// EAV compilation of one date operator: a two-branch UNION inside a single
/// driving subquery, mirroring `push_string_operator_eav`.
fn push_date_op_eav(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    date: &str,
    op: &str,
) {
    let stripped = strip_wikilink_brackets(date);

    let mut scan_clauses: Vec<String> = Vec::new();
    let mut scan_binds: Vec<SqlValue> = Vec::new();
    push_date_op(&mut scan_clauses, &mut scan_binds, field, date, op);
    let scan_fragment = scan_clauses
        .pop()
        .expect("push_date_op pushes exactly one clause");

    where_clauses.push(format!(
        "path IN ( \
            SELECT path FROM document_fields \
             WHERE key = ? AND typeof(value) = 'text' AND value {op} ? \
            UNION ALL \
            SELECT f.path FROM document_fields f JOIN documents dd ON dd.path = f.path \
             WHERE f.key = ? AND typeof(f.value) NOT IN ('text', 'blob', 'null') AND ({scan_fragment}) \
         )"
    ));
    binds.push(SqlValue::Text(field.to_string()));
    binds.push(SqlValue::Text(stripped));
    binds.push(SqlValue::Text(field.to_string()));
    binds.extend(scan_binds);
}

/// Build the WHERE clause for `--eq` (negate=false) or `--not-eq` (negate=true).
fn push_equality(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    value: &serde_json::Value,
    negate: bool,
) {
    let array_exists = if negate { "NOT EXISTS" } else { "EXISTS" };
    let scalar_op = if negate { "!=" } else { "=" };
    match value {
        serde_json::Value::String(raw) => {
            let stripped = strip_wikilink_brackets(raw);
            let path = json_path_for(field);
            let array_test = format!(
                "(json_each.type IN {STRING_MATCHABLE_TYPES} AND {STRIPPED_ARRAY_ELEMENT} = ?)"
            );
            let (scalar_test, scalar_binds): (String, Vec<SqlValue>) = if negate {
                (
                    format!(
                        "(json_type(frontmatter_json, ?) NOT IN ('null') \
                         AND NOT (json_type(frontmatter_json, ?) IN {STRING_MATCHABLE_TYPES} \
                                  AND {STRIPPED_SCALAR} = ?))"
                    ),
                    vec![
                        SqlValue::Text(path.clone()),
                        SqlValue::Text(path.clone()),
                        SqlValue::Text(path),
                        SqlValue::Text(stripped.clone()),
                    ],
                )
            } else {
                (
                    format!(
                        "(json_type(frontmatter_json, ?) IN {STRING_MATCHABLE_TYPES} \
                         AND {STRIPPED_SCALAR} {scalar_op} ?)"
                    ),
                    vec![
                        SqlValue::Text(path.clone()),
                        SqlValue::Text(path),
                        SqlValue::Text(stripped.clone()),
                    ],
                )
            };
            push_array_aware_clause(
                where_clauses,
                binds,
                field,
                array_exists,
                &array_test,
                &[SqlValue::Text(stripped)],
                &scalar_test,
                &scalar_binds,
            );
        }
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
            let typed = json_value_to_sql(value);
            push_array_aware_clause(
                where_clauses,
                binds,
                field,
                array_exists,
                "value = ?",
                std::slice::from_ref(&typed),
                &format!("json_extract(frontmatter_json, ?) {scalar_op} ?"),
                &[SqlValue::Text(json_path_for(field)), typed.clone()],
            );
        }
        _ => {
            let op = if negate { "!=" } else { "=" };
            where_clauses.push(format!("json_extract(frontmatter_json, ?) {op} ?"));
            binds.push(SqlValue::Text(json_path_for(field)));
            binds.push(json_value_to_sql(value));
        }
    }
}

/// True when a `--in`/`--not-in` value list qualifies for the array-aware
/// membership compile: every value is a scalar (string, number, or bool).
fn all_scalar_values(values: &[serde_json::Value]) -> bool {
    values.iter().all(|v| {
        matches!(
            v,
            serde_json::Value::String(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_)
        )
    })
}

/// Build the WHERE clause for `--in` (negate=false) or `--not-in` (negate=true)
/// when every value in the list is a scalar (`all_scalar_values`).
fn push_scalar_membership(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    values: &[serde_json::Value],
    negate: bool,
) {
    let stripped: Vec<SqlValue> = values
        .iter()
        .filter_map(|v| v.as_str().map(strip_wikilink_brackets))
        .map(SqlValue::Text)
        .collect();
    let typed: Vec<SqlValue> = values
        .iter()
        .filter(|v| !v.is_string())
        .map(json_value_to_sql)
        .collect();
    let ph = |n: usize| std::iter::repeat_n("?", n).collect::<Vec<_>>().join(", ");
    let (s_ph, t_ph) = (ph(stripped.len()), ph(typed.len()));

    let array_exists = if negate { "NOT EXISTS" } else { "EXISTS" };
    let scalar_op = if negate { "NOT IN" } else { "IN" };
    let path = SqlValue::Text(json_path_for(field));

    let (array_test, array_binds, scalar_test, scalar_binds): (
        String,
        Vec<SqlValue>,
        String,
        Vec<SqlValue>,
    ) = match (stripped.is_empty(), typed.is_empty()) {
        (false, true) => {
            let positive_scalar = format!(
                "(json_type(frontmatter_json, ?) IN {STRING_MATCHABLE_TYPES} \
                 AND {STRIPPED_SCALAR} IN ({s_ph}))"
            );
            let (scalar_test, mut scalar_binds) = if negate {
                (
                    format!(
                        "(json_type(frontmatter_json, ?) NOT IN ('null') AND NOT {positive_scalar})"
                    ),
                    vec![path.clone(), path.clone(), path],
                )
            } else {
                (positive_scalar, vec![path.clone(), path])
            };
            scalar_binds.extend(stripped.iter().cloned());
            (
                format!(
                    "(json_each.type IN {STRING_MATCHABLE_TYPES} \
                     AND {STRIPPED_ARRAY_ELEMENT} IN ({s_ph}))"
                ),
                stripped,
                scalar_test,
                scalar_binds,
            )
        }
        (true, false) => (
            format!("value IN ({t_ph})"),
            typed.clone(),
            format!("json_extract(frontmatter_json, ?) {scalar_op} ({t_ph})"),
            std::iter::once(path).chain(typed).collect(),
        ),
        (false, false) => {
            let positive_scalar = format!(
                "((json_type(frontmatter_json, ?) IN {STRING_MATCHABLE_TYPES} \
                   AND {STRIPPED_SCALAR} IN ({s_ph})) \
                  OR json_extract(frontmatter_json, ?) IN ({t_ph}))"
            );
            let (scalar_test, mut scalar_binds) = if negate {
                (
                    format!(
                        "(json_type(frontmatter_json, ?) NOT IN ('null') AND NOT {positive_scalar})"
                    ),
                    vec![path.clone(), path.clone(), path.clone()],
                )
            } else {
                (positive_scalar, vec![path.clone(), path.clone()])
            };
            scalar_binds.extend(stripped.iter().cloned());
            scalar_binds.push(path);
            scalar_binds.extend(typed.iter().cloned());
            (
                format!(
                    "((json_each.type IN {STRING_MATCHABLE_TYPES} \
                       AND {STRIPPED_ARRAY_ELEMENT} IN ({s_ph})) \
                      OR value IN ({t_ph}))"
                ),
                stripped.iter().cloned().chain(typed).collect(),
                scalar_test,
                scalar_binds,
            )
        }
        (true, true) => unreachable!("caller guarantees a non-empty value list"),
    };

    push_array_aware_clause(
        where_clauses,
        binds,
        field,
        array_exists,
        &array_test,
        &array_binds,
        &scalar_test,
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

/// Build the WHERE clause for one anchored string operator (scan path).
fn push_string_operator(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    needle: &str,
    op: StringOperator,
) {
    let stripped = strip_wikilink_brackets(needle);
    if stripped.is_empty() {
        where_clauses.push("0".to_string());
        return;
    }
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

/// EAV compilation of one anchored string operator: a two-branch UNION inside a
/// single driving subquery.
fn push_string_operator_eav(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    needle: &str,
    op: StringOperator,
) {
    let stripped = strip_wikilink_brackets(needle);
    if stripped.is_empty() {
        where_clauses.push("0".to_string());
        return;
    }

    let (branch1_sql, branch1_binds): (String, Vec<SqlValue>) = match op {
        StringOperator::StartsWith => match prefix_upper_bound(&stripped) {
            Some(upper) => (
                "SELECT path FROM document_fields \
                 WHERE key = ? AND typeof(value) = 'text' AND value >= ? AND value < ?"
                    .to_string(),
                vec![
                    SqlValue::Text(field.to_string()),
                    SqlValue::Text(stripped.clone()),
                    SqlValue::Text(upper),
                ],
            ),
            None => (
                "SELECT path FROM document_fields \
                 WHERE key = ? AND typeof(value) = 'text' AND value >= ? AND value < x'00'"
                    .to_string(),
                vec![
                    SqlValue::Text(field.to_string()),
                    SqlValue::Text(stripped.clone()),
                ],
            ),
        },
        StringOperator::EndsWith | StringOperator::Contains => {
            let mut b = vec![SqlValue::Text(field.to_string())];
            b.extend(std::iter::repeat_n(
                SqlValue::Text(stripped.clone()),
                op.needle_binds(),
            ));
            (
                format!(
                    "SELECT path FROM document_fields \
                     WHERE key = ? AND typeof(value) = 'text' AND {}",
                    op.test("value")
                ),
                b,
            )
        }
    };

    let mut scan_clauses: Vec<String> = Vec::new();
    let mut scan_binds: Vec<SqlValue> = Vec::new();
    push_string_operator(&mut scan_clauses, &mut scan_binds, field, needle, op);
    let scan_fragment = scan_clauses
        .pop()
        .expect("push_string_operator pushes exactly one clause for a non-empty needle");

    where_clauses.push(format!(
        "path IN ( \
            {branch1_sql} \
            UNION \
            SELECT f.path FROM document_fields f JOIN documents dd ON dd.path = f.path \
             WHERE f.key = ? AND typeof(f.value) NOT IN ('text', 'blob', 'null') AND ({scan_fragment}) \
         )"
    ));
    binds.extend(branch1_binds);
    binds.push(SqlValue::Text(field.to_string()));
    binds.extend(scan_binds);
}

/// Exclusive upper bound, in BINARY-collation TEXT order, for every string that
/// has `prefix` as a byte prefix. Bumps the trailing Unicode scalar value rather
/// than the trailing byte, so it never produces an invalid UTF-8 string.
fn prefix_upper_bound(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        if let Some(bumped) = bump_char(last) {
            chars.push(bumped);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// The next Unicode scalar value after `c`, skipping the unencodable surrogate
/// range, or `None` if `c` is already the maximum representable scalar value.
fn bump_char(c: char) -> Option<char> {
    let next = (c as u32).checked_add(1)?;
    let next = if (0xD800..=0xDFFF).contains(&next) {
        0xE000
    } else {
        next
    };
    char::from_u32(next)
}

/// Storage types a STRING query value may match (NRN-85 typed strictness).
const STRING_MATCHABLE_TYPES: &str = "('text', 'object', 'array')";

/// Bracket-stripped text of a `json_each` array element.
const STRIPPED_ARRAY_ELEMENT: &str = "replace(replace(value, '[[', ''), ']]', '')";

/// Bracket-stripped text of a scalar field; consumes one JSON-path bind.
const STRIPPED_SCALAR: &str =
    "replace(replace(json_extract(frontmatter_json, ?), '[[', ''), ']]', '')";

/// Push the compound array-aware WHERE skeleton shared by the string forms of
/// `--eq`/`--not-eq`, `--in`/`--not-in`, and the anchored string operators.
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
/// `json_extract` returns for that same value.
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
        _ => SqlValue::Text(serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    fn index_set(fields: &[&str]) -> BTreeSet<String> {
        fields.iter().map(|s| s.to_string()).collect()
    }

    fn open_authoritative(root: &Utf8PathBuf, fields: &[&str]) -> Cache {
        let mut cache =
            Cache::open_with_index(root, None, &[], index_set(fields), "hash-1").unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn paths(docs: &[DocumentSummary]) -> Vec<String> {
        let mut p: Vec<String> = docs.iter().map(|d| d.path.to_string()).collect();
        p.sort();
        p
    }

    fn explain_plan(cache: &Cache, sql: &str, binds: &[SqlValue]) -> Vec<String> {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut stmt = cache.conn().prepare(&explain).unwrap();
        stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn plan_for(cache: &Cache, query: &DocumentQuery) -> (String, Vec<String>) {
        let (where_sql, binds) = build_documents_matching_sql_parts(cache, query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        (where_sql.clone(), explain_plan(cache, &sql, &binds))
    }

    /// The EAV planner-guard invariant: no full SCAN of either `documents` or
    /// `document_fields`. A per-row degradation of the EAV subquery would show as
    /// `SCAN document_fields`, which the disarmed `!SCAN documents`-only check
    /// missed entirely.
    fn no_scan(rows: &[String]) {
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields (per-row subquery degradation): {rows:?}"
        );
    }

    fn drives_kv(rows: &[String]) {
        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "must drive via a SEARCH on idx_document_fields_kv: {rows:?}"
        );
    }

    fn matched(cache: &Cache, query: &DocumentQuery) -> Vec<String> {
        paths(&cache.documents_matching(query).unwrap())
    }

    // ── A realistic multi-shape fixture (ported from the donor mimir vault) ──
    fn mimir_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        let docs: &[(&str, &str)] = &[
            (
                "t-100.md",
                "---\nproject: NRN\nlifecycle: active\ntype: task\n\
                 depends_on: [\"[[T-123]]\"]\ntags: [\"release:v0.40\", \"area:cache\"]\n\
                 anchor: \"[[NRN-80-anchor]]\"\n---\nbody\n",
            ),
            (
                "t-101.md",
                "---\nproject: NRN\nlifecycle: done\ntype: task\n\
                 depends_on: []\ntags: [\"release:v0.39\"]\nanchor: \"[[NRN-79-anchor]]\"\n---\nbody\n",
            ),
            (
                "t-102.md",
                "---\nproject: NRN\nlifecycle: abandoned\ntype: task\ntags: [\"type:note\"]\n---\nbody\n",
            ),
            (
                "t-103.md",
                "---\nproject: NRN\nlifecycle: backlog\ntype: task\n\
                 depends_on: [\"[[T-123]]\", \"[[T-999]]\"]\ntags: [\"release:v0.41\"]\n---\nbody\n",
            ),
            (
                "t-104.md",
                "---\nproject: ATLAS\nlifecycle: active\ntype: task\ntags: [\"area:vault\"]\n---\nbody\n",
            ),
            (
                "t-105.md",
                "---\nproject: NRN\nlifecycle: active\ntype: task\n---\nbody\n",
            ),
        ];
        for (name, body) in docs {
            std::fs::write(root.join(name).as_std_path(), body).unwrap();
        }
        (tmp, root)
    }

    fn open_mimir(root: &Utf8PathBuf) -> Cache {
        open_authoritative(
            root,
            &[
                "project",
                "lifecycle",
                "type",
                "depends_on",
                "tags",
                "anchor",
            ],
        )
    }

    // ── R3: --eq project:NRN --not-in lifecycle:done,abandoned ──────────────
    #[test]
    fn r3_positive_eq_drives_kv_negation_probes_pk() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);
        let query = DocumentQuery {
            frontmatter_eq: vec![("project".into(), serde_json::json!("NRN"))],
            frontmatter_not_in: vec![(
                "lifecycle".into(),
                vec![serde_json::json!("done"), serde_json::json!("abandoned")],
            )],
            ..Default::default()
        };
        let (_where_sql, rows) = plan_for(&cache, &query);
        drives_kv(&rows);
        assert!(
            rows.iter().any(|r| r.contains("idx_document_fields_pk")),
            "negated --not-in must probe via idx_document_fields_pk: {rows:?}"
        );
        no_scan(&rows);
        assert_eq!(
            matched(&cache, &query),
            vec!["t-100.md", "t-103.md", "t-105.md"]
        );
    }

    // ── R5: --eq depends_on:T-123 (reverse dependency, array wikilink) ──────
    #[test]
    fn r5_reverse_dependency_array_wikilink_field_drives_search() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);
        let query = DocumentQuery {
            frontmatter_eq: vec![("depends_on".into(), serde_json::json!("T-123"))],
            ..Default::default()
        };
        let (_where_sql, rows) = plan_for(&cache, &query);
        drives_kv(&rows);
        no_scan(&rows);
        assert_eq!(matched(&cache, &query), vec!["t-100.md", "t-103.md"]);
    }

    // ── --missing anchor: sentinel-driven SEARCH ────────────────────────────
    #[test]
    fn missing_anchor_drives_search_with_sentinel_bind() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);
        let query = DocumentQuery {
            frontmatter_missing: vec!["anchor".into()],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            where_sql.contains("x'00'"),
            "--missing must reference the x'00' absent-sentinel: {where_sql}"
        );
        let (_where_sql, rows) = plan_for(&cache, &query);
        drives_kv(&rows);
        no_scan(&rows);
        assert_eq!(
            matched(&cache, &query),
            vec!["t-102.md", "t-103.md", "t-104.md", "t-105.md"]
        );
    }

    // ── B1/R6: --starts-with tags:release: is a range SEARCH ────────────────
    #[test]
    fn prefix_tags_release_namespace_is_range_search() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);
        let query = DocumentQuery {
            frontmatter_starts_with: vec![("tags".into(), "release:".into())],
            ..Default::default()
        };
        let (where_sql, rows) = plan_for(&cache, &query);
        assert!(
            where_sql.contains("value >= ?") && where_sql.contains("value < ?"),
            "starts-with's text branch must compile to a value range test: {where_sql}"
        );
        assert!(
            where_sql.contains("UNION"),
            "starts-with must compile to the two-branch union: {where_sql}"
        );
        drives_kv(&rows);
        no_scan(&rows);
        assert_eq!(
            matched(&cache, &query),
            vec!["t-100.md", "t-101.md", "t-103.md"]
        );
    }

    // ── Adversarial: ~50%-selective --eq must still SEARCH, never scan ──────
    #[test]
    fn adversarial_non_selective_eq_still_no_scan() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        for i in 0..200 {
            let ty = if i % 2 == 0 { "note" } else { "log" };
            std::fs::write(
                root.join(format!("doc{i:04}.md")).as_std_path(),
                format!("---\ntype: {ty}\n---\nbody\n"),
            )
            .unwrap();
        }
        let cache = open_authoritative(&root, &["type"]);
        let query = DocumentQuery {
            frontmatter_eq: vec![("type".into(), serde_json::json!("note"))],
            ..Default::default()
        };
        let (_where_sql, rows) = plan_for(&cache, &query);
        drives_kv(&rows);
        no_scan(&rows);
        assert_eq!(cache.documents_matching(&query).unwrap().len(), 100);
    }

    // ── Unfiltered doc-set load is a single-row plan (no per-row subquery) ──
    #[test]
    fn default_query_is_single_scan_no_per_row_subquery() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);
        let (_where_sql, rows) = plan_for(&cache, &DocumentQuery::default());
        assert_eq!(
            rows.len(),
            1,
            "the doc-set load should be a single plan row (no join/subquery): {rows:?}"
        );
        assert!(
            rows[0].contains("SCAN documents"),
            "no predicate narrows the set, so a single full scan is the only plan: {rows:?}"
        );
    }

    #[test]
    fn unindexed_field_falls_back_to_scan_plan() {
        let (_tmp, root) = mimir_vault();
        // `type` indexed but `kind` is not → the referenced unindexed field
        // forces the whole query onto the json_extract scan path.
        let cache = open_authoritative(&root, &["type"]);
        let query = DocumentQuery {
            frontmatter_eq: vec![("kind".into(), serde_json::json!("log"))],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            where_sql.contains("json_extract"),
            "unindexed field must fall back to json_extract scan: {where_sql}"
        );
    }

    // ── EAV/scan agreement, per value shape (parity is the hard invariant) ──
    //
    // Each case is run BOTH routed (via `documents_matching` over an authoritative
    // cache with the fields indexed) and via the scan builder directly, and the
    // two result sets must be identical for every shape.
    fn assert_routed_matches_scan(cache: &Cache, query: &DocumentQuery) {
        let routed = matched(cache, query);
        let (scan_sql, scan_binds) = build_documents_matching_sql_parts_scan(query);
        let sql = format!(
            "SELECT path, stem, hash, frontmatter_json, body_text FROM documents{scan_sql} ORDER BY path"
        );
        let mut stmt = cache.conn().prepare(&sql).unwrap();
        let mut scan: Vec<String> = stmt
            .query_map(params_from_iter(scan_binds.iter()), |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        scan.sort();
        assert_eq!(routed, scan, "EAV route diverged from scan for {query:?}");
    }

    #[test]
    fn agree_eq_not_eq_has_missing() {
        let (_tmp, root) = mimir_vault();
        let cache = open_mimir(&root);
        for q in [
            DocumentQuery {
                frontmatter_eq: vec![("project".into(), serde_json::json!("NRN"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["project".into()],
                frontmatter_not_eq: vec![("lifecycle".into(), serde_json::json!("active"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["anchor".into()],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_missing: vec!["anchor".into()],
                ..Default::default()
            },
        ] {
            assert_routed_matches_scan(&cache, &q);
        }
    }

    #[test]
    fn agree_typed_eq_and_in_across_number_bool() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("n1.md").as_std_path(),
            "---\npriority: 5\nflag: true\nscore: 2.5\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("n2.md").as_std_path(),
            "---\npriority: 10\nflag: false\nscore: 2.5\n---\n",
        )
        .unwrap();
        let cache = open_authoritative(&root, &["priority", "flag", "score"]);
        for q in [
            DocumentQuery {
                frontmatter_eq: vec![("priority".into(), serde_json::json!(5))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("flag".into(), serde_json::json!(true))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("score".into(), serde_json::json!(2.5))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_in: vec![(
                    "priority".into(),
                    vec![serde_json::json!(5), serde_json::json!(10)],
                )],
                ..Default::default()
            },
        ] {
            assert_routed_matches_scan(&cache, &q);
        }
    }

    // ── NRN-426: dual-eq routes to a mixed string+number membership. At scale
    //    the shape drives the kv index (no per-row `document_fields` scan) — the
    //    fallback `--eq zip:07030` stays sane, and the routed result equals the
    //    scan result for both stored representations. ────────────────────────
    #[test]
    fn dual_type_membership_string_and_number_no_scan() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        // A representative table (SQLite picks a full scan for a handful of rows
        // regardless of the index, so scale past that cost threshold to observe
        // the real plan the shape supports).
        for i in 0..200 {
            let body = match i % 3 {
                0 => "---\nzip: \"07030\"\n---\n".to_string(), // stored as string
                1 => "---\nzip: 7030\n---\n".to_string(),      // stored as number
                _ => format!("---\nzip: \"{:05}\"\n---\n", 10000 + i), // unrelated
            };
            std::fs::write(root.join(format!("d{i:04}.md")).as_std_path(), body).unwrap();
        }
        let cache = open_authoritative(&root, &["zip"]);
        // The exact shape `build_document_query` emits for the fallback
        // `--eq zip:07030`: a two-value membership over the string and number.
        let query = DocumentQuery {
            frontmatter_in: vec![(
                "zip".into(),
                vec![serde_json::json!("07030"), serde_json::json!(7030)],
            )],
            ..Default::default()
        };
        let (_where_sql, rows) = plan_for(&cache, &query);
        drives_kv(&rows);
        no_scan(&rows);
        // 67 string "07030" (i%3==0) + 67 numeric 7030 (i%3==1) = 134 matches.
        assert_eq!(cache.documents_matching(&query).unwrap().len(), 134);
        assert_routed_matches_scan(&cache, &query);
    }

    #[test]
    fn agree_date_ops_before_after_on() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("old.md").as_std_path(),
            "---\ncreated: 2025-01-15\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("mid.md").as_std_path(),
            "---\ncreated: 2026-05-19\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("new.md").as_std_path(),
            "---\ncreated: 2026-12-01\n---\n",
        )
        .unwrap();
        let cache = open_authoritative(&root, &["created"]);
        for q in [
            DocumentQuery {
                date_before: vec![("created".into(), "2026-01-01".into())],
                ..Default::default()
            },
            DocumentQuery {
                date_after: vec![("created".into(), "2026-01-01".into())],
                ..Default::default()
            },
            DocumentQuery {
                date_on: vec![("created".into(), "2026-05-19".into())],
                ..Default::default()
            },
        ] {
            assert_routed_matches_scan(&cache, &q);
        }
    }

    #[test]
    fn agree_unicode_multibyte_prefix() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("cafe.md").as_std_path(),
            "---\nname: café-mocha\n---\n",
        )
        .unwrap();
        std::fs::write(root.join("cake.md").as_std_path(), "---\nname: cake\n---\n").unwrap();
        let cache = open_authoritative(&root, &["name"]);
        let q = DocumentQuery {
            frontmatter_starts_with: vec![("name".into(), "café".into())],
            ..Default::default()
        };
        assert_routed_matches_scan(&cache, &q);
        assert_eq!(matched(&cache, &q), vec!["cafe.md"]);
    }

    #[test]
    fn agree_embedded_wikilink_object_and_array_element() {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("scalar.md").as_std_path(),
            "---\nworkspace: \"[[norn]]\"\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("arr.md").as_std_path(),
            "---\nsource_notes:\n  - \"[[seed-note]]\"\n  - \"[[other]]\"\n---\n",
        )
        .unwrap();
        let cache = open_authoritative(&root, &["workspace", "source_notes"]);
        for q in [
            DocumentQuery {
                frontmatter_eq: vec![("workspace".into(), serde_json::json!("norn"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("source_notes".into(), serde_json::json!("seed-note"))],
                ..Default::default()
            },
        ] {
            assert_routed_matches_scan(&cache, &q);
        }
    }
}
