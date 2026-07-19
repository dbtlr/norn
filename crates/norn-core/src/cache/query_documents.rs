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

    fn synth_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("note-a.md").as_std_path(),
            "---\ntype: note\nkind: log\n---\nbody a\n",
        )
        .unwrap();
        std::fs::write(
            root.join("note-b.md").as_std_path(),
            "---\ntype: note\nkind: meeting\n---\nbody b\n",
        )
        .unwrap();
        std::fs::write(
            root.join("workspace.md").as_std_path(),
            "---\ntype: workspace\n---\nbody w\n",
        )
        .unwrap();
        std::fs::write(root.join("untyped.md").as_std_path(), "no frontmatter\n").unwrap();
        (tmp, root)
    }

    fn open_authoritative(root: &Utf8PathBuf, fields: &[&str]) -> Cache {
        let mut cache =
            Cache::open_with_index(root, None, &[], index_set(fields), "hash-1").unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn paths(docs: &[DocumentSummary]) -> Vec<String> {
        docs.iter().map(|d| d.path.to_string()).collect()
    }

    #[test]
    fn eav_and_scan_agree_on_eq_not_eq_has_missing_in_not_in() {
        let (_tmp, root) = synth_vault();
        let cache = open_authoritative(&root, &["type", "kind"]);

        let cases: Vec<DocumentQuery> = vec![
            DocumentQuery {
                frontmatter_eq: vec![("type".into(), serde_json::json!("note"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["type".into()],
                frontmatter_not_eq: vec![("type".into(), serde_json::json!("note"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["kind".into()],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_missing: vec!["kind".into()],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_in: vec![(
                    "kind".into(),
                    vec![serde_json::json!("log"), serde_json::json!("meeting")],
                )],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_not_in: vec![("type".into(), vec![serde_json::json!("workspace")])],
                ..Default::default()
            },
        ];

        for query in &cases {
            // EAV routed (fields indexed) vs scan (explicit).
            let routed = cache.documents_matching(query).unwrap();
            let (scan_sql, scan_binds) = build_documents_matching_sql_parts_scan(query);
            let scan_full = format!(
                "SELECT path, stem, hash, frontmatter_json, body_text FROM documents{scan_sql} ORDER BY path"
            );
            let mut stmt = cache.conn().prepare(&scan_full).unwrap();
            let scan_paths: Vec<String> = stmt
                .query_map(params_from_iter(scan_binds.iter()), |r| {
                    r.get::<_, String>(0)
                })
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(paths(&routed), scan_paths, "mismatch for {query:?}");
        }
    }

    #[test]
    fn two_predicate_positive_query_plan_is_search_no_scan() {
        let (_tmp, root) = synth_vault();
        let cache = open_authoritative(&root, &["type", "kind"]);
        let query = DocumentQuery {
            frontmatter_eq: vec![
                ("type".into(), serde_json::json!("note")),
                ("kind".into(), serde_json::json!("log")),
            ],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        let plan = explain_plan(&cache, &sql, &binds);
        assert!(
            !plan.iter().any(|r| r.contains("SCAN documents")),
            "positive EAV query should not full-scan documents: {plan:?}"
        );
    }

    #[test]
    fn missing_query_plan_is_a_driving_search() {
        let (_tmp, root) = synth_vault();
        let cache = open_authoritative(&root, &["kind"]);
        let query = DocumentQuery {
            frontmatter_missing: vec!["kind".into()],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        let plan = explain_plan(&cache, &sql, &binds);
        assert!(
            plan.iter().any(|r| r.contains("document_fields")),
            "--missing should drive on document_fields: {plan:?}"
        );
    }

    #[test]
    fn unindexed_field_falls_back_to_scan_plan() {
        let (_tmp, root) = synth_vault();
        // Only `type` indexed; query filters an unindexed field → scan.
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

    fn explain_plan(cache: &Cache, sql: &str, binds: &[SqlValue]) -> Vec<String> {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut stmt = cache.conn().prepare(&explain).unwrap();
        stmt.query_map(params_from_iter(binds.iter()), |r| r.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }
}
