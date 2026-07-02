//! SQL-direct document query — `Cache::documents_matching` and
//! `Cache::document_by_path`.
//!
//! `build_documents_matching_sql_parts` is the query router (NRN-79): it
//! decides, per query, whether every predicate can be answered from the
//! derived `document_fields` EAV table (`build_documents_matching_sql_parts_eav`)
//! or must fall back to the original `json_extract` scan
//! (`build_documents_matching_sql_parts_scan`). See `eav_eligible_fields` for
//! the exact provable/fallback boundary and `scan_semantics_probe` for the
//! pinned scan-path truths that boundary is derived from.

use std::collections::BTreeSet;

use crate::core::DocumentSummary;
use crate::standards::path_match::PathPattern;
use camino::{Utf8Path, Utf8PathBuf};
use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;

use crate::cache::canonical::strip_wikilink_brackets;
use crate::cache::error::CacheError;
use crate::cache::query::{json_path_for, DocumentQuery};
use crate::cache::Cache;

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

fn build_documents_matching_sql(cache: &Cache, query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let (where_sql, binds) = build_documents_matching_sql_parts(cache, query);
    let sql = format!(
        "SELECT path, stem, hash, frontmatter_json, body_text \
         FROM documents{} ORDER BY path",
        where_sql
    );
    (sql, binds)
}

/// Query router: decide whether `query` can be answered entirely from the
/// `document_fields` derived index, or must fall back to the `json_extract`
/// scan path. All-or-nothing per query — see the module doc comment.
///
/// Routes to the EAV plan iff: the cache was opened authoritatively, every
/// frontmatter field referenced by an EAV-eligible predicate is in the
/// cache's declared index set, AND every predicate present in the query
/// belongs to an EAV-provable class (`eav_eligible_fields`). Any other
/// combination — non-authoritative open, an unindexed referenced field, or a
/// predicate class this router doesn't (yet) prove equivalent — falls back
/// to the unchanged scan path. Falling back because a field is simply
/// undeclared/unindexed warns (once, cold-scan only); falling back because
/// of a router limitation on an already-indexed field is silent (that's
/// norn's gap, not the caller's).
pub(crate) fn build_documents_matching_sql_parts(
    cache: &Cache,
    query: &DocumentQuery,
) -> (String, Vec<SqlValue>) {
    if cache.index_authoritative {
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
    }
    build_documents_matching_sql_parts_scan(query)
}

/// Emit the cold-scan warning when falling back to the scan path solely
/// because one or more referenced fields aren't in the declared index set.
/// Matches the `warning: ...` voice used by `find --col` (see
/// `crate::find::render::warn_unknown_cols`). Silent under the 1,000-row
/// threshold — this is a perf hint, not a correctness signal, and would be
/// noise on small vaults where the scan is already instant.
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

/// Every frontmatter field referenced by an EAV-provable predicate in
/// `query`, or `None` if the query contains any predicate class this router
/// doesn't (yet) prove equivalent to the scan path.
///
/// EAV-provable (see `scan_semantics_probe` for the pinned truths):
/// - `--eq`/`--not-eq` with a STRING value: the scan path's string branch is
///   already array-aware and bracket-stripped, exactly matching the
///   `document_fields` row-per-element writer.
/// - `--in`/`--not-in` where every value is a STRING: same array-aware form
///   (the scan path itself branches on this).
/// - `--has`/`--missing`: pure presence, driven by the absent-sentinel row.
///
/// NOT EAV-provable in this task (always falls back, per predicate class):
/// - `--eq`/`--not-eq`/`--in`/`--not-in` with any NON-STRING value: the scan
///   path's non-string branch is a bare scalar `json_extract(...) = ?` with
///   NO array-awareness, but `document_fields` stores one row per array
///   element regardless of type — an EAV compilation would (wrongly) match
///   inside arrays where the scan path never does. See
///   `eq_integer_is_not_array_aware` / `in_non_string_values_is_not_array_aware`.
/// - `--starts-with`/`--ends-with`/`--contains`: the scan path renders
///   booleans as literal `true`/`false` text for these operators
///   specifically, diverging from `canonicalize_scalar`'s INTEGER 0/1
///   (needed for `--eq` parity). One canonical stored value can't serve
///   both representations. See `string_op_renders_bool_as_source_text_not_integer`.
/// - `--before`/`--after`/`--on`: the scan path is scalar-only (no
///   array-awareness at all) and compares the WHOLE field's JSON text; for
///   an array-valued field that's the array's JSON-encoded text, not its
///   elements' dates. `document_fields` can't reconstruct that whole-value
///   text from its per-element rows. See
///   `date_before_on_array_field_compares_json_array_text_not_elements`.
fn eav_eligible_fields(query: &DocumentQuery) -> Option<BTreeSet<String>> {
    if !query.frontmatter_starts_with.is_empty()
        || !query.frontmatter_ends_with.is_empty()
        || !query.frontmatter_contains.is_empty()
        || !query.date_before.is_empty()
        || !query.date_after.is_empty()
        || !query.date_on.is_empty()
    {
        return None;
    }

    let mut fields: BTreeSet<String> = BTreeSet::new();

    for (field, value) in &query.frontmatter_eq {
        if !matches!(value, serde_json::Value::String(_)) {
            return None;
        }
        fields.insert(field.clone());
    }
    for (field, value) in &query.frontmatter_not_eq {
        if !matches!(value, serde_json::Value::String(_)) {
            return None;
        }
        fields.insert(field.clone());
    }
    fields.extend(query.frontmatter_has.iter().cloned());
    fields.extend(query.frontmatter_missing.iter().cloned());
    for (field, values) in &query.frontmatter_in {
        if values.is_empty() {
            // `--in field:` with no values compiles to a field-independent
            // "0" clause in both builders — no document_fields lookup, so
            // no index-membership requirement.
            continue;
        }
        if !values
            .iter()
            .all(|v| matches!(v, serde_json::Value::String(_)))
        {
            return None;
        }
        fields.insert(field.clone());
    }
    for (field, values) in &query.frontmatter_not_in {
        if values.is_empty() {
            // `--not-in field:` with no values is a no-op in both builders.
            continue;
        }
        if !values
            .iter()
            .all(|v| matches!(v, serde_json::Value::String(_)))
        {
            return None;
        }
        fields.insert(field.clone());
    }

    Some(fields)
}

/// EAV compilation of the provable predicate classes (see
/// `eav_eligible_fields`). Only called once the router has confirmed every
/// predicate present qualifies, so the non-string `.as_str()` unwraps below
/// are guaranteed to succeed. Non-field predicates
/// (starts-with/ends-with/contains/date ops) are guaranteed empty by the
/// same gate; `push_shared_tail_clauses` still runs unconditionally so the
/// two builders can never drift on the ones that DO always overlap
/// (body_text/links_to/unresolved-links).
fn build_documents_matching_sql_parts_eav(query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();

    // Positive equality: driving IN-list against the (key, value) index.
    for (field, value) in &query.frontmatter_eq {
        let raw = value.as_str().expect("eav-eligible --eq must be a string");
        where_clauses.push(
            "path IN (SELECT path FROM document_fields WHERE key = ? AND value = ?)".to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
        binds.push(SqlValue::Text(strip_wikilink_brackets(raw)));
    }

    // Negation: correlated probe against the (path, key) index. The
    // sentinel OR-term reproduces the scan path's "missing excludes from
    // --not-eq" behavior (see `not_eq_excludes_missing_field_by_default`).
    for (field, value) in &query.frontmatter_not_eq {
        let raw = value
            .as_str()
            .expect("eav-eligible --not-eq must be a string");
        where_clauses.push(
            "NOT EXISTS (SELECT 1 FROM document_fields f WHERE f.path = documents.path \
             AND f.key = ? AND (f.value = ? OR f.value = x'00'))"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
        binds.push(SqlValue::Text(strip_wikilink_brackets(raw)));
    }

    // Presence: any non-sentinel row for (path, key).
    for field in &query.frontmatter_has {
        where_clauses.push(
            "EXISTS (SELECT 1 FROM document_fields f WHERE f.path = documents.path \
             AND f.key = ? AND f.value IS NOT x'00')"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
    }

    // Absence: driving lookup for the sentinel row itself.
    for field in &query.frontmatter_missing {
        where_clauses.push(
            "path IN (SELECT path FROM document_fields WHERE key = ? AND value = x'00')"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
    }

    // Positive membership: driving IN-list, same shape as --eq generalized
    // to N values.
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
            let raw = v.as_str().expect("eav-eligible --in must be all strings");
            binds.push(SqlValue::Text(strip_wikilink_brackets(raw)));
        }
    }

    // Negative membership: correlated probe, same shape as --not-eq
    // generalized to N values.
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
            let raw = v
                .as_str()
                .expect("eav-eligible --not-in must be all strings");
            binds.push(SqlValue::Text(strip_wikilink_brackets(raw)));
        }
    }

    push_shared_tail_clauses(&mut where_clauses, &mut binds, query);

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };
    (where_sql, binds)
}

/// Original `json_extract` scan compilation, unchanged from pre-NRN-79
/// behavior. The router's universal fallback: every predicate class this
/// task doesn't prove EAV-equivalent, plus every query against a
/// non-authoritative cache or an unindexed field, runs through here byte-for-
/// byte as before.
pub(crate) fn build_documents_matching_sql_parts_scan(
    query: &DocumentQuery,
) -> (String, Vec<SqlValue>) {
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

    push_shared_tail_clauses(&mut where_clauses, &mut binds, query);

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    (where_sql, binds)
}

/// Predicate classes never gated by EAV eligibility: they either never
/// touch `document_fields` at all (body-text, links-to, unresolved-links),
/// or this task's router always routes them to the scan path
/// (starts-with/ends-with/contains, date ops — see `eav_eligible_fields`'s
/// doc comment for why). Shared verbatim between
/// `build_documents_matching_sql_parts_scan` and
/// `build_documents_matching_sql_parts_eav` so the two builders can't drift
/// on the pieces they both need unchanged; for the EAV builder these
/// starts-with/ends-with/contains/date vectors are guaranteed empty by the
/// eligibility gate, so this is a no-op there beyond body-text/links.
fn push_shared_tail_clauses(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    query: &DocumentQuery,
) {
    // --starts-with / --ends-with / --contains field:VALUE
    for (field, needle) in &query.frontmatter_starts_with {
        push_string_operator(
            where_clauses,
            binds,
            field,
            needle,
            StringOperator::StartsWith,
        );
    }
    for (field, needle) in &query.frontmatter_ends_with {
        push_string_operator(
            where_clauses,
            binds,
            field,
            needle,
            StringOperator::EndsWith,
        );
    }
    for (field, needle) in &query.frontmatter_contains {
        push_string_operator(
            where_clauses,
            binds,
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

#[cfg(test)]
mod router_tests {
    //! Router round-trips (EAV plan vs. scan plan must agree) and EXPLAIN
    //! QUERY PLAN guards proving the EAV-routed queries actually hit
    //! `idx_document_fields_kv`/`idx_document_fields_pk` rather than
    //! scanning `document_fields` or `documents` in full.

    use std::collections::BTreeSet;

    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    use super::build_documents_matching_sql_parts;
    use crate::cache::{Cache, DocumentQuery};
    use crate::core::DocumentSummary;

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
            root.join("a.md").as_std_path(),
            "---\nstatus: active\nkind: log\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.md").as_std_path(),
            "---\nstatus: backlog\nkind: log\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("c.md").as_std_path(),
            "---\nstatus: active\n---\n",
        )
        .unwrap();
        std::fs::write(root.join("d.md").as_std_path(), "---\nother: x\n---\n").unwrap();
        (tmp, root)
    }

    fn open_authoritative(root: &Utf8PathBuf, fields: &[&str]) -> Cache {
        let set = index_set(fields);
        let mut cache = Cache::open_with_index(root, None, &set, "test-hash").unwrap();
        cache.rebuild(root).unwrap();
        cache
    }

    fn paths(docs: &[DocumentSummary]) -> Vec<String> {
        let mut v: Vec<String> = docs.iter().map(|d| d.path.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn eav_and_scan_agree_on_eq_not_eq_has_missing_in_not_in() {
        let (_tmp, root) = synth_vault();
        let indexed = open_authoritative(&root, &["status", "kind"]);
        let scanned = Cache::open(&root).unwrap(); // non-authoritative: always scan

        let cases: Vec<DocumentQuery> = vec![
            DocumentQuery {
                frontmatter_eq: vec![("status".to_string(), serde_json::json!("active"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["kind".to_string()],
                frontmatter_not_eq: vec![("kind".to_string(), serde_json::json!("log"))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["status".to_string()],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_missing: vec!["kind".to_string()],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_in: vec![(
                    "status".to_string(),
                    vec![serde_json::json!("active"), serde_json::json!("backlog")],
                )],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_has: vec!["status".to_string()],
                frontmatter_not_in: vec![("status".to_string(), vec![serde_json::json!("active")])],
                ..Default::default()
            },
            // Two-predicate combo: positive eq + negation on a different field.
            DocumentQuery {
                frontmatter_eq: vec![("kind".to_string(), serde_json::json!("log"))],
                frontmatter_not_eq: vec![("status".to_string(), serde_json::json!("active"))],
                frontmatter_has: vec!["status".to_string()],
                ..Default::default()
            },
        ];

        for query in cases {
            assert_eq!(
                paths(&indexed.documents_matching(&query).unwrap()),
                paths(&scanned.documents_matching(&query).unwrap()),
                "EAV and scan routes diverged for {query:?}"
            );
        }
    }

    fn explain_plan(cache: &Cache, sql: &str, binds: &[rusqlite::types::Value]) -> Vec<String> {
        let full_sql = format!("EXPLAIN QUERY PLAN {sql}");
        let mut stmt = cache.conn().prepare(&full_sql).unwrap();
        stmt.query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    #[test]
    fn two_predicate_positive_query_plan_is_search_no_scan() {
        let (_tmp, root) = synth_vault();
        let cache = open_authoritative(&root, &["status", "kind"]);

        let query = DocumentQuery {
            frontmatter_eq: vec![
                ("status".to_string(), serde_json::json!("active")),
                ("kind".to_string(), serde_json::json!("log")),
            ],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(&cache, &sql, &binds);

        assert!(
            rows.iter().any(|r| r.contains("idx_document_fields_kv")),
            "expected a SEARCH on idx_document_fields_kv: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
    }

    #[test]
    fn negation_query_drives_on_positive_and_probes_via_pk_index() {
        let (_tmp, root) = synth_vault();
        let cache = open_authoritative(&root, &["status", "kind"]);

        let query = DocumentQuery {
            frontmatter_eq: vec![("kind".to_string(), serde_json::json!("log"))],
            frontmatter_has: vec!["status".to_string()],
            frontmatter_not_eq: vec![("status".to_string(), serde_json::json!("active"))],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(&cache, &sql, &binds);

        assert!(
            rows.iter().any(|r| r.contains("idx_document_fields_kv")),
            "expected the positive --eq to drive via idx_document_fields_kv: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("idx_document_fields_pk")),
            "expected the negation probe via idx_document_fields_pk: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
    }

    #[test]
    fn missing_query_plan_is_a_driving_search() {
        let (_tmp, root) = synth_vault();
        let cache = open_authoritative(&root, &["status", "kind"]);

        let query = DocumentQuery {
            frontmatter_missing: vec!["kind".to_string()],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(&cache, &sql, &binds);

        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "--missing must compile to a driving SEARCH on idx_document_fields_kv: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
    }

    #[test]
    fn unindexed_field_falls_back_to_scan_plan() {
        let (_tmp, root) = synth_vault();
        // `kind` is authoritatively declared but NOT in the index set.
        let cache = open_authoritative(&root, &["status"]);

        let query = DocumentQuery {
            frontmatter_eq: vec![("kind".to_string(), serde_json::json!("log"))],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            where_sql.contains("json_extract"),
            "unindexed field must fall back to the json_extract scan form: {where_sql}"
        );
    }

    #[test]
    fn non_authoritative_open_never_routes_even_with_matching_field_name() {
        let (_tmp, root) = synth_vault();
        // Non-authoritative open: `index_set` is the unconfigured default
        // (empty), so even a field name that would otherwise be eligible
        // must fall back — see `Cache::open`'s authoritativeness contract.
        let cache = Cache::open(&root).unwrap();
        let query = DocumentQuery {
            frontmatter_eq: vec![("status".to_string(), serde_json::json!("active"))],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            where_sql.contains("json_extract"),
            "non-authoritative open must never route: {where_sql}"
        );
    }
}
