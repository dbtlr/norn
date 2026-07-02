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

use crate::cache::canonical::{canonicalize_scalar, strip_wikilink_brackets};
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
    // A non-authoritative open never warns here even on a large vault with an
    // unindexed field — it has no `index_set` to compare against (the
    // `Cache::open` default is an unconfigured empty set, not "nothing is
    // indexed"), so there is no index concept to advise the caller toward.
    // Accepted as designed, not a gap: advice would be noise with nothing
    // behind it.
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
/// - `--eq`/`--not-eq` with a NUMBER/BOOL value (NRN-81): the scan path's
///   typed branch is array-aware with SQLite's typed comparison, and
///   `canonicalize_scalar` stores exactly the `json_value_to_sql` value the
///   scan side binds — INTEGER/REAL rows compare numerically in both paths,
///   and a typed bind can never compare true against ANY text, so the
///   writer's bracket-strip on text rows is unobservable to it.
/// - `--in`/`--not-in` where every value is a STRING, or every value is a
///   NUMBER/BOOL: the corresponding single-bucket array-aware form
///   (`push_scalar_membership`), N-value generalizations of the two `--eq`
///   branches above with the same provability arguments.
/// - `--has`/`--missing`: pure presence, driven by the absent-sentinel row.
/// - `--starts-with`/`--ends-with`/`--contains`: compiled as a two-branch
///   union (`push_string_operator_eav`) rather than a single index lookup —
///   text-typed `document_fields` rows are tested directly (a RANGE scan for
///   `--starts-with`), and any row holding a non-text value for the field
///   (the shapes that render differently between the index and
///   `frontmatter_json` — see `string_op_renders_bool_as_source_text_not_integer`)
///   falls through to the EXACT scan-path expression evaluated against the
///   joined document. That second branch is what makes this provable: it
///   reproduces the scan path byte-for-byte for every value shape by
///   construction, not by argument, so string ops no longer need to force a
///   whole-query fallback.
///
/// NOT EAV-provable in this task (always falls back, per predicate class):
/// - `--in`/`--not-in` lists MIXING string and number/bool values: the scan
///   path's string bucket compares `replace(replace(value, ...), ...)`, and
///   SQLite's `replace()` casts INTEGER/REAL stored values to TEXT — so a
///   string bind like `"5"` (reachable via `--in n:[[5]],9`; bare `5`
///   coerces to a number) matches a stored number `5` on scan, while the
///   EAV `value IN (...)` on the no-affinity column never cross-matches
///   TEXT binds against numeric rows. The same seam exists for pure-string
///   values against numeric storage on BOTH `--eq` and all-string `--in`
///   (pre-existing, tracked as NRN-85); this gate just avoids widening it.
///   Lift when NRN-85 settles the canonical semantics.
/// - `--eq`/`--not-eq`/`--in`/`--not-in` with a NULL/OBJECT/ARRAY value:
///   the scan path keeps the legacy bare scalar form for these (see
///   `push_equality` / `all_scalar_values` — unreachable from the CLI/MCP
///   parsers), which is not array-aware, while `document_fields` stores one
///   row per array element — an EAV compilation would (wrongly) match
///   inside arrays where the scan path never does. Objects/arrays
///   additionally diverge on the writer's bracket-strip of their JSON text.
/// - `--before`/`--after`/`--on`: per-element semantics (NRN-82) are now
///   EAV-shaped in principle, but the scan side compares the
///   bracket-STRIPPED text of every element regardless of type, while
///   `document_fields` rows keep typed INTEGER/REAL values whose text
///   renderings the index can't range-compare without a per-row cast — a
///   provable-parity compile needs the two-branch union treatment string
///   operators got (`push_string_operator_eav`). Deferred; see NRN-84.
fn eav_eligible_fields(query: &DocumentQuery) -> Option<BTreeSet<String>> {
    if !query.date_before.is_empty() || !query.date_after.is_empty() || !query.date_on.is_empty() {
        return None;
    }

    let eav_scalar = |v: &serde_json::Value| {
        matches!(
            v,
            serde_json::Value::String(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_)
        )
    };
    // Provable membership lists are single-bucket: all strings or all
    // number/bool — a mixed list's string bucket can cross-match numeric
    // rows on the scan side (see the mixed-list NOT-provable entry above).
    let eav_list = |values: &[serde_json::Value]| {
        values.iter().all(|v| v.is_string())
            || values
                .iter()
                .all(|v| matches!(v, serde_json::Value::Number(_) | serde_json::Value::Bool(_)))
    };

    let mut fields: BTreeSet<String> = BTreeSet::new();

    fields.extend(query.frontmatter_starts_with.iter().map(|(f, _)| f.clone()));
    fields.extend(query.frontmatter_ends_with.iter().map(|(f, _)| f.clone()));
    fields.extend(query.frontmatter_contains.iter().map(|(f, _)| f.clone()));

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
            // `--in field:` with no values compiles to a field-independent
            // "0" clause in both builders — no document_fields lookup, so
            // no index-membership requirement.
            continue;
        }
        if !eav_list(values) {
            return None;
        }
        fields.insert(field.clone());
    }
    for (field, values) in &query.frontmatter_not_in {
        if values.is_empty() {
            // `--not-in field:` with no values is a no-op in both builders.
            continue;
        }
        if !eav_list(values) {
            return None;
        }
        fields.insert(field.clone());
    }

    Some(fields)
}

/// EAV compilation of the provable predicate classes (see
/// `eav_eligible_fields`). Only called once the router has confirmed every
/// predicate present qualifies (scalar-typed values only). Date-op vectors
/// are guaranteed empty by the same gate; `push_shared_tail_clauses` still
/// runs unconditionally so the two builders can never drift on the ones that DO always overlap
/// (body_text/links_to/unresolved-links).
fn build_documents_matching_sql_parts_eav(query: &DocumentQuery) -> (String, Vec<SqlValue>) {
    let mut where_clauses: Vec<String> = Vec::new();
    let mut binds: Vec<SqlValue> = Vec::new();

    // Positive equality: driving IN-list against the (key, value) index.
    // `canonicalize_scalar` is the document_fields writer's own value
    // canonicalizer — binding through it is what keeps query-side values
    // byte-identical to stored rows for every eligible type (strings
    // bracket-stripped TEXT, numbers INTEGER/REAL, bools INTEGER).
    for (field, value) in &query.frontmatter_eq {
        where_clauses.push(
            "path IN (SELECT path FROM document_fields WHERE key = ? AND value = ?)".to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
        binds.push(canonicalize_scalar(value));
    }

    // Negation: correlated probe against the (path, key) index. The
    // sentinel OR-term reproduces the scan path's "missing excludes from
    // --not-eq" behavior (see `not_eq_excludes_missing_field_by_default`).
    for (field, value) in &query.frontmatter_not_eq {
        where_clauses.push(
            "NOT EXISTS (SELECT 1 FROM document_fields f WHERE f.path = documents.path \
             AND f.key = ? AND (f.value = ? OR f.value = x'00'))"
                .to_string(),
        );
        binds.push(SqlValue::Text(field.clone()));
        binds.push(canonicalize_scalar(value));
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
            binds.push(canonicalize_scalar(v));
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
            binds.push(canonicalize_scalar(v));
        }
    }

    // Anchored string operators: two-branch union per predicate (see
    // `push_string_operator_eav`). Provable per `eav_eligible_fields`'s doc
    // comment — the non-text branch re-evaluates the exact scan expression,
    // so this can never drift from `push_string_operator`'s scan form.
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
        if all_scalar_values(values) {
            push_scalar_membership(
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
        if all_scalar_values(values) {
            push_scalar_membership(
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

    push_shared_tail_clauses(&mut where_clauses, &mut binds, query);

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    (where_sql, binds)
}

/// Predicate classes never gated by EAV eligibility: they either never touch
/// `document_fields` at all (body-text, links-to, unresolved-links), or this
/// router always falls the whole query back to scan when they're present
/// (date ops — see `eav_eligible_fields`'s doc comment for why). Shared
/// verbatim between `build_documents_matching_sql_parts_scan` and
/// `build_documents_matching_sql_parts_eav` so the two builders can't drift
/// on the pieces they both need unchanged; for the EAV builder the date
/// vectors are guaranteed empty by the eligibility gate, so this is a no-op
/// there beyond body-text/links. Anchored string operators
/// (starts-with/ends-with/contains) are NOT here — they're EAV-eligible now,
/// so each builder compiles them its own way: the scan builder via
/// `push_string_operator` (see `push_string_operators_scan_form`), the EAV
/// builder via `push_string_operator_eav`.
fn push_shared_tail_clauses(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    query: &DocumentQuery,
) {
    // --before / --after / --on field:DATE — array-aware, bracket-stripped
    // text comparison on both sides (NRN-82): an array-valued field matches
    // when ANY element's stripped text satisfies the comparison, a scalar
    // field when its own stripped text does. The strip makes Obsidian
    // daily-note links (`created: "[[2026-01-01]]"`) compare as their
    // dates; ISO date strings order lexically = chronologically. See
    // `scan_semantics_probe`'s date section for the pinned truths.
    for (field, date) in &query.date_before {
        push_date_op(&mut *where_clauses, &mut *binds, field, date, "<");
    }
    for (field, date) in &query.date_after {
        push_date_op(&mut *where_clauses, &mut *binds, field, date, ">");
    }
    for (field, date) in &query.date_on {
        push_date_op(&mut *where_clauses, &mut *binds, field, date, "=");
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

/// Build the WHERE clause for one date operator (`--before` `<`, `--after`
/// `>`, `--on` `=`). Array-aware EXISTS-any over the same skeleton as
/// string `--eq`, comparing bracket-stripped text on both sides — SQLite's
/// `replace()` casts non-text values to their text rendering, so numeric
/// stored values compare lexically as text (see
/// `date_ops_on_non_string_scalar_compare_by_text_rendering`).
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

/// Build the WHERE clause for `--eq` (negate=false) or `--not-eq` (negate=true).
/// Every scalar-typed value is array-aware (NRN-81): string values match by
/// bracket-stripped text (scalar fields by equality, array fields via any
/// `json_each` element, `[[...]]` wrappers collapsed on both sides); number
/// and bool values match by SQLite's typed comparison over the same
/// array-aware skeleton (INTEGER/REAL compare numerically, TEXT never
/// equals a numeric bind). Null/object/array query values keep the bare
/// scalar equality — they are unreachable from the CLI/MCP parsers (see
/// `filter_args::coerce_value`), and the array-aware negation of a null
/// bind would surprise (`NOT EXISTS(element = NULL)` is vacuously true).
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
/// Lists carrying null/object/array values — unreachable from the CLI/MCP
/// parsers (see `filter_args::coerce_value`) — keep the legacy bare
/// `json_extract(...) IN (...)` form.
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
/// when every value in the list is a scalar (`all_scalar_values`). Same
/// array-aware skeleton as `--eq`, generalized to N values with each value
/// compared by its own rules (NRN-81): string values by bracket-stripped
/// text, number/bool values typed. A doc matches `--in` when its scalar —
/// or any array element — matches ANY listed value; `--not-in` when NONE
/// do. For an all-string list this compiles byte-identically to the
/// pre-NRN-81 string-membership form (the typed bucket vanishes), which the
/// EAV router's provability argument relies on.
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
        (false, true) => (
            format!("{STRIPPED_ARRAY_ELEMENT} IN ({s_ph})"),
            stripped.clone(),
            format!("{STRIPPED_SCALAR} {scalar_op} ({s_ph})"),
            std::iter::once(path).chain(stripped).collect(),
        ),
        (true, false) => (
            format!("value IN ({t_ph})"),
            typed.clone(),
            format!("json_extract(frontmatter_json, ?) {scalar_op} ({t_ph})"),
            std::iter::once(path).chain(typed).collect(),
        ),
        (false, false) => {
            let compound_scalar = format!(
                "({STRIPPED_SCALAR} IN ({s_ph}) OR json_extract(frontmatter_json, ?) IN ({t_ph}))"
            );
            let scalar_test = if negate {
                format!("NOT {compound_scalar}")
            } else {
                compound_scalar
            };
            let mut scalar_binds = vec![path.clone()];
            scalar_binds.extend(stripped.iter().cloned());
            scalar_binds.push(path);
            scalar_binds.extend(typed.iter().cloned());
            (
                format!("({STRIPPED_ARRAY_ELEMENT} IN ({s_ph}) OR value IN ({t_ph}))"),
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

/// EAV compilation of one anchored string operator: a two-branch UNION
/// inside a single driving subquery, so a string-op predicate can join the
/// index route instead of forcing the whole query to scan.
///
/// - Branch 1 reads `document_fields` rows already typed `TEXT` for this
///   key directly — those values are exactly what `push_string_operator`'s
///   array/scalar text tests would see for a string-shaped element or
///   scalar (the writer bracket-strips on the way in), so the SAME
///   `StringOperator::test`/`needle_binds` expression applies straight to
///   `value` with no CASE/json_each wrapping needed. `--starts-with`
///   compiles this branch as an index RANGE (`value >= prefix AND value <
///   upper`) rather than a `substr` predicate, so it can use
///   `idx_document_fields_kv`'s `(key, value, path)` ordering as a genuine
///   range scan — see `prefix_upper_bound`.
/// - Branch 2 catches every doc holding a NON-text row for this key —
///   bools/numbers, the shapes `push_string_operator`'s CASE rendering
///   diverges from `canonicalize_scalar`'s stored representation (see
///   `string_op_renders_bool_as_source_text_not_integer`) — and re-evaluates
///   `push_string_operator`'s EXACT fragment against the joined document's
///   `frontmatter_json`. Reusing that fragment verbatim (rather than
///   reimplementing it) is what makes this provably byte-identical: any
///   future change to the scan-side rendering automatically propagates here
///   with no separate EAV-side fix.
///
/// The two branches can overlap (e.g. a mixed-type array with both a
/// matching text element and a boolean element) — harmless, `UNION` dedups.
fn push_string_operator_eav(
    where_clauses: &mut Vec<String>,
    binds: &mut Vec<SqlValue>,
    field: &str,
    needle: &str,
    op: StringOperator,
) {
    let stripped = strip_wikilink_brackets(needle);
    if stripped.is_empty() {
        // Matches `push_string_operator`'s empty-needle behavior exactly.
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
                // No TEXT value can ever sort above `stripped` (see
                // `prefix_upper_bound`) — the BLOB sentinel region is the
                // exclusive upper bound of TEXT storage-class space, so this
                // is equivalent to an unbounded-above range within TEXT.
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

    // Branch 2: reuse `push_string_operator`'s clause verbatim — it always
    // pushes exactly one clause for a non-empty needle (checked above).
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

/// Exclusive upper bound, in BINARY-collation TEXT order, for every string
/// that has `prefix` as a byte prefix — i.e. the smallest string that is
/// provably NOT prefixed by `prefix`. Bumps the trailing Unicode scalar
/// value rather than the trailing byte: UTF-8 byte order matches codepoint
/// order, so bumping the last *character* gives the identical range a raw
/// byte-increment would, without ever producing an invalid UTF-8 string
/// along the way (a plain byte-increment can split a multi-byte character —
/// e.g. incrementing the trailing continuation byte of `"café"` can land
/// outside the continuation-byte range, or crossing the 0x7F/0x80 ASCII
/// boundary, or 0xBF/0xC0 continuation-byte boundary, all invalid UTF-8).
///
/// Carries into the preceding character when the trailing one is already
/// `char::MAX` (`U+10FFFF`) or would land in the surrogate range (which has
/// no UTF-8 encoding), skipping straight to `U+E000`. Returns `None` when no
/// upper bound exists in TEXT space at all (every character in `prefix` is
/// already at its maximum) — the caller falls back to the BLOB sentinel
/// region, since every BLOB sorts after every TEXT value regardless of
/// content.
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

/// The next Unicode scalar value after `c`, skipping the unencodable
/// surrogate range, or `None` if `c` is already the maximum representable
/// scalar value.
fn bump_char(c: char) -> Option<char> {
    let next = (c as u32).checked_add(1)?;
    let next = if (0xD800..=0xDFFF).contains(&next) {
        0xE000
    } else {
        next
    };
    char::from_u32(next)
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

    // ── object/nested-array canonicalization parity (Fix 1) ──────────────
    //
    // A CLI `--eq field:{"name":"Alice"}` token coerces to a plain
    // `Value::String` (see `filter_args::coerce_value` — it never parses
    // JSON syntax out of a raw arg), so this compares that literal string
    // against the field's whole JSON-serialized text — a comparison that IS
    // string-eligible on both sides of the router. Before the canonical.rs
    // fix, `canonicalize_scalar` only bracket-stripped the bare-String
    // variant, so a `document_fields` row for an object/array value kept
    // its embedded `[[brackets]]` while the scan path (which strips the
    // whole extracted JSON text unconditionally) did not — the two routes
    // disagreed on exactly this shape.

    fn object_bracket_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("bracketed.md").as_std_path(),
            "---\ncustom:\n  name: \"[[Alice]]\"\nnested:\n  - \"[[Alice]]\"\n  - Bob\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("unbracketed.md").as_std_path(),
            "---\ncustom:\n  name: Alice\nnested:\n  - Alice\n  - Bob\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("other.md").as_std_path(),
            "---\ncustom:\n  name: Someone Else\nnested:\n  - Someone\n  - Else\n---\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn eav_and_scan_agree_on_object_with_embedded_wikilink_eq() {
        let (_tmp, root) = object_bracket_vault();
        let indexed = open_authoritative(&root, &["custom", "nested"]);
        let scanned = Cache::open(&root).unwrap();

        let cases: Vec<DocumentQuery> = vec![
            DocumentQuery {
                frontmatter_eq: vec![(
                    "custom".to_string(),
                    serde_json::json!(r#"{"name":"Alice"}"#),
                )],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_not_eq: vec![(
                    "custom".to_string(),
                    serde_json::json!(r#"{"name":"Alice"}"#),
                )],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_in: vec![(
                    "custom".to_string(),
                    vec![serde_json::json!(r#"{"name":"Alice"}"#)],
                )],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![(
                    "nested".to_string(),
                    serde_json::json!(r#"["Alice","Bob"]"#),
                )],
                ..Default::default()
            },
        ];

        for query in cases {
            let routed = paths(&indexed.documents_matching(&query).unwrap());
            let scan = paths(&scanned.documents_matching(&query).unwrap());
            assert_eq!(routed, scan, "EAV and scan routes diverged for {query:?}");
        }

        // Pin the actual match, not just agreement: both `bracketed.md`
        // (via canonicalization stripping the stored brackets) and
        // `unbracketed.md` must match `--eq custom:{"name":"Alice"}`.
        let eq_alice = DocumentQuery {
            frontmatter_eq: vec![(
                "custom".to_string(),
                serde_json::json!(r#"{"name":"Alice"}"#),
            )],
            ..Default::default()
        };
        assert_eq!(
            paths(&indexed.documents_matching(&eq_alice).unwrap()),
            vec!["bracketed.md", "unbracketed.md"]
        );
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
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
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
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
    }

    // ── typed-value routing (NRN-81) ─────────────────────────────────────
    //
    // Number/bool --eq/--not-eq/--in/--not-in are EAV-eligible now that the
    // scan path is array-aware for them: `canonicalize_scalar` binds the
    // exact typed value the writer stored per element. These tests prove
    // routed vs. scan parity across the value shapes the typed comparison
    // must respect (numeric INTEGER/REAL affinity, TEXT-vs-numeric type
    // strictness, bool-as-INTEGER, sentinel exclusion), plus an EXPLAIN
    // guard proving a typed --eq actually drives the (key, value) index.

    fn typed_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("int_scalar.md").as_std_path(),
            "---\nn: 5\nb: true\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("int_array.md").as_std_path(),
            "---\nn: [5, 6]\nb: [false]\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("float_array.md").as_std_path(),
            "---\nn: [5.0]\nscore: 2.5\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("text_five.md").as_std_path(),
            "---\nn: [\"5\"]\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("mixed_array.md").as_std_path(),
            "---\nn: [seven, 8]\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("empty_array.md").as_std_path(),
            "---\nn: []\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("missing.md").as_std_path(),
            "---\nother: x\n---\n",
        )
        .unwrap();
        (tmp, root)
    }

    #[test]
    fn eav_and_scan_agree_on_typed_eq_in_across_value_shapes() {
        let (_tmp, root) = typed_vault();
        let fields = vec!["n", "b", "score"];
        let indexed = open_authoritative(&root, &fields);
        let scanned = Cache::open(&root).unwrap();

        let cases: Vec<DocumentQuery> = vec![
            DocumentQuery {
                frontmatter_eq: vec![("n".to_string(), serde_json::json!(5))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("n".to_string(), serde_json::json!(5.0))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("score".to_string(), serde_json::json!(2.5))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("b".to_string(), serde_json::json!(true))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_eq: vec![("b".to_string(), serde_json::json!(false))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_not_eq: vec![("n".to_string(), serde_json::json!(5))],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_in: vec![("n".to_string(), vec![serde_json::json!(5)])],
                ..Default::default()
            },
            // Mixed string+typed list: each value by its own rules.
            DocumentQuery {
                frontmatter_in: vec![(
                    "n".to_string(),
                    vec![serde_json::json!("seven"), serde_json::json!(5)],
                )],
                ..Default::default()
            },
            DocumentQuery {
                frontmatter_not_in: vec![(
                    "n".to_string(),
                    vec![serde_json::json!("seven"), serde_json::json!(5)],
                )],
                ..Default::default()
            },
            // Typed --eq combined with a string predicate on another field.
            DocumentQuery {
                frontmatter_eq: vec![("n".to_string(), serde_json::json!(5))],
                frontmatter_not_eq: vec![("b".to_string(), serde_json::json!(true))],
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

        // Pin the actual matched sets for the headline NRN-81 shapes, not
        // just route agreement.
        let eq_five = DocumentQuery {
            frontmatter_eq: vec![("n".to_string(), serde_json::json!(5))],
            ..Default::default()
        };
        assert_eq!(
            paths(&indexed.documents_matching(&eq_five).unwrap()),
            vec!["float_array.md", "int_array.md", "int_scalar.md"],
            "typed --eq matches scalar and array elements numerically; a \
             TEXT element \"5\" never equals a numeric bind"
        );
    }

    #[test]
    fn typed_eq_query_routes_and_drives_kv_index() {
        let (_tmp, root) = typed_vault();
        let cache = open_authoritative(&root, &["n"]);

        let query = DocumentQuery {
            frontmatter_eq: vec![("n".to_string(), serde_json::json!(5))],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            where_sql.contains("document_fields"),
            "typed --eq on an indexed field must route: {where_sql}"
        );
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(&cache, &sql, &binds);
        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "expected a driving SEARCH on idx_document_fields_kv: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
    }

    #[test]
    fn null_object_values_still_fall_back_to_scan() {
        let (_tmp, root) = typed_vault();
        let cache = open_authoritative(&root, &["n"]);

        for value in [serde_json::json!(null), serde_json::json!({"a": 1})] {
            let query = DocumentQuery {
                frontmatter_eq: vec![("n".to_string(), value)],
                ..Default::default()
            };
            let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
            assert!(
                !where_sql.contains("document_fields"),
                "null/object --eq values must fall back to scan: {where_sql}"
            );
        }
        // A list containing a null keeps the legacy scan form too.
        let query = DocumentQuery {
            frontmatter_in: vec![(
                "n".to_string(),
                vec![serde_json::json!(5), serde_json::json!(null)],
            )],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            !where_sql.contains("document_fields"),
            "--in lists carrying null must fall back to scan: {where_sql}"
        );
    }

    #[test]
    fn mixed_string_typed_membership_lists_fall_back_to_scan() {
        // A mixed list's string bucket can cross-match numeric rows on the
        // scan side (SQLite `replace()` casts INTEGER/REAL to TEXT), which
        // the EAV `value IN (...)` never reproduces — so mixed lists are
        // not EAV-provable (see NRN-85 for the underlying seam). The
        // adversarial-review repro: `--in n:[[5]],9` must return the same
        // documents whether or not `n` is indexed.
        let (_tmp, root) = typed_vault();
        let indexed = open_authoritative(&root, &["n"]);
        let scanned = Cache::open(&root).unwrap();

        for values in [
            vec![serde_json::json!("[[5]]"), serde_json::json!(9)],
            vec![serde_json::json!("seven"), serde_json::json!(5)],
        ] {
            let in_query = DocumentQuery {
                frontmatter_in: vec![("n".to_string(), values.clone())],
                ..Default::default()
            };
            let (where_sql, _binds) = build_documents_matching_sql_parts(&indexed, &in_query);
            assert!(
                !where_sql.contains("document_fields"),
                "mixed --in lists must fall back to scan: {where_sql}"
            );
            let not_in_query = DocumentQuery {
                frontmatter_not_in: vec![("n".to_string(), values)],
                ..Default::default()
            };
            let (where_sql, _binds) = build_documents_matching_sql_parts(&indexed, &not_in_query);
            assert!(
                !where_sql.contains("document_fields"),
                "mixed --not-in lists must fall back to scan: {where_sql}"
            );

            assert_eq!(
                paths(&indexed.documents_matching(&in_query).unwrap()),
                paths(&scanned.documents_matching(&in_query).unwrap()),
                "routes diverged for {in_query:?}"
            );
        }

        // Single-bucket lists still route.
        for values in [
            vec![serde_json::json!(5), serde_json::json!(9)],
            vec![serde_json::json!("a"), serde_json::json!("b")],
        ] {
            let query = DocumentQuery {
                frontmatter_in: vec![("n".to_string(), values)],
                ..Default::default()
            };
            let (where_sql, _binds) = build_documents_matching_sql_parts(&indexed, &query);
            assert!(
                where_sql.contains("document_fields"),
                "single-bucket --in lists on an indexed field must route: {where_sql}"
            );
        }
    }

    #[test]
    fn date_ops_still_fall_back_to_scan() {
        let (_tmp, root) = typed_vault();
        let cache = open_authoritative(&root, &["n"]);
        let query = DocumentQuery {
            frontmatter_eq: vec![("n".to_string(), serde_json::json!(5))],
            date_before: vec![("n".to_string(), "2026-01-01".to_string())],
            ..Default::default()
        };
        let (where_sql, _binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            !where_sql.contains("document_fields"),
            "any date op must force the whole query to the scan path: {where_sql}"
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

    // ── hybrid string-operator routing (NRN-79 extension) ────────────────
    //
    // `--starts-with`/`--ends-with`/`--contains` now join the EAV-eligible
    // set via `push_string_operator_eav`'s two-branch union. These tests
    // prove routed vs. scan parity across every value shape the union's two
    // branches need to agree on, plus EXPLAIN guards proving the union
    // actually drives off `idx_document_fields_kv` rather than scanning.

    /// One vault exercising every value shape `push_string_operator_eav`'s
    /// two branches need to agree on: plain/wikilink strings, a namespaced
    /// tags array, boolean scalar AND array element, integer, float, an
    /// object with an embedded wikilink, an empty array, a missing field,
    /// literal `%`/`_` (SQL LIKE metacharacters, must never be treated as
    /// wildcards), and a family of multi-byte-prefix shapes for the
    /// `prefix_upper_bound` range-bound boundary.
    fn string_op_vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::write(
            root.join("alpha.md").as_std_path(),
            "---\n\
             title: \"Alpha Report\"\n\
             alias: \"[[Alice]]\"\n\
             tags: [\"release:v0.1\", \"type:note\"]\n\
             archived: true\n\
             flags: [true, false]\n\
             count: 42\n\
             score: 2.5\n\
             custom:\n  name: \"[[Alice]]\"\n\
             empty_tags: []\n\
             pct: \"50% off\"\n\
             underscore: \"a_b_c\"\n\
             unicode: \"café\"\n\
             ---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("beta.md").as_std_path(),
            "---\n\
             title: \"Beta Report\"\n\
             alias: \"[[Bob]]\"\n\
             tags: [\"release:v0.2\"]\n\
             archived: false\n\
             flags: [false]\n\
             count: 7\n\
             score: 3.14\n\
             custom:\n  name: \"Bob\"\n\
             empty_tags: []\n\
             pct: \"no percent here\"\n\
             underscore: \"xyz\"\n\
             unicode: \"cafe\"\n\
             ---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("gamma.md").as_std_path(),
            "---\ntitle: \"Gamma Notes\"\ntags: [\"type:note\"]\ncount: 100\nunicode: \"cafz\"\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("delta.md").as_std_path(),
            "---\ntitle: \"Delta\"\nunicode: \"café2\"\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("epsilon.md").as_std_path(),
            "---\ntitle: \"Epsilon\"\nunicode: \"cafê\"\n---\n",
        )
        .unwrap();
        std::fs::write(
            root.join("missing.md").as_std_path(),
            "---\nother: x\n---\n",
        )
        .unwrap();
        (tmp, root)
    }

    fn string_op_index_fields() -> Vec<&'static str> {
        vec![
            "title",
            "alias",
            "tags",
            "archived",
            "flags",
            "count",
            "score",
            "custom",
            "empty_tags",
            "pct",
            "underscore",
            "unicode",
            "missing_entirely",
        ]
    }

    fn starts_with(field: &str, needle: &str) -> DocumentQuery {
        DocumentQuery {
            frontmatter_starts_with: vec![(field.to_string(), needle.to_string())],
            ..Default::default()
        }
    }

    fn ends_with(field: &str, needle: &str) -> DocumentQuery {
        DocumentQuery {
            frontmatter_ends_with: vec![(field.to_string(), needle.to_string())],
            ..Default::default()
        }
    }

    fn contains(field: &str, needle: &str) -> DocumentQuery {
        DocumentQuery {
            frontmatter_contains: vec![(field.to_string(), needle.to_string())],
            ..Default::default()
        }
    }

    #[test]
    fn string_op_eav_and_scan_agree_across_value_shapes() {
        let (_tmp, root) = string_op_vault();
        let fields = string_op_index_fields();
        let indexed = open_authoritative(&root, &fields);
        let scanned = Cache::open(&root).unwrap();

        let cases: Vec<DocumentQuery> = vec![
            // Plain string scalar.
            starts_with("title", "Alpha"),
            ends_with("title", "Report"),
            contains("title", "ph"),
            // Wikilink-bracketed string scalar — both sides bracket-collapse.
            starts_with("alias", "Alic"),
            contains("alias", "ob"),
            // Namespaced tags array (`release:v0.1` style) — array-aware.
            starts_with("tags", "release:"),
            contains("tags", "note"),
            // Boolean scalar: the exact divergence that used to force a
            // whole-query fallback (scan renders 'true'/'false' text, the
            // index stores INTEGER 0/1 — branch 2 must reconcile this).
            contains("archived", "true"),
            contains("archived", "1"),
            starts_with("archived", "tru"),
            // Boolean array element, same divergence, array-aware.
            starts_with("flags", "fal"),
            contains("flags", "true"),
            // Integer / float scalars — non-text `document_fields` rows.
            contains("count", "4"),
            starts_with("count", "42"),
            starts_with("count", "7"),
            contains("score", "2.5"),
            starts_with("score", "3."),
            // Object with an embedded wikilink (Fix 1 territory) — whole
            // JSON text is bracket-stripped and tested as one TEXT value.
            contains("custom", "Alice"),
            contains("custom", "[["),
            // Empty array: no scalar to test, must never match.
            contains("empty_tags", "x"),
            // No document has this field at all — never matches, and (since
            // it's declared/indexed) actually exercises the EAV route: every
            // doc gets only the absent-sentinel row, excluded by branch 2's
            // `typeof NOT IN ('text','blob','null')` filter.
            starts_with("missing_entirely", "x"),
            ends_with("missing_entirely", "x"),
            contains("missing_entirely", "x"),
            // SQL LIKE metacharacters treated literally, not as wildcards.
            contains("pct", "%"),
            starts_with("pct", "50%"),
            contains("underscore", "_"),
            contains("underscore", "a_b"),
            // Empty-string needle: no meaningful anchored match, ever.
            starts_with("title", ""),
            ends_with("title", ""),
            contains("title", ""),
        ];

        for query in cases {
            assert_eq!(
                paths(&indexed.documents_matching(&query).unwrap()),
                paths(&scanned.documents_matching(&query).unwrap()),
                "EAV and scan routes diverged for {query:?}"
            );
        }
    }

    #[test]
    fn string_op_eav_and_scan_agree_on_unicode_multibyte_prefix() {
        // `prefix_upper_bound` bumps the trailing Unicode scalar value, not
        // the trailing byte, so it never splits `é` (U+00E9, a 2-byte UTF-8
        // character). Exercise every boundary: exact match, an extension of
        // the prefix, values that sort just below it, and the bumped
        // boundary value itself (which must NOT match).
        let (_tmp, root) = string_op_vault();
        let fields = string_op_index_fields();
        let indexed = open_authoritative(&root, &fields);
        let scanned = Cache::open(&root).unwrap();

        let query = starts_with("unicode", "café");
        assert_eq!(
            paths(&indexed.documents_matching(&query).unwrap()),
            paths(&scanned.documents_matching(&query).unwrap()),
            "EAV and scan routes diverged for a multi-byte prefix: {query:?}"
        );
        // Pin the actual matched set too, not just that both sides agree —
        // a bug that made both sides wrong the same way would still pass a
        // bare equality check.
        assert_eq!(
            paths(&scanned.documents_matching(&query).unwrap()),
            vec!["alpha.md", "delta.md"],
            "expected exactly the docs whose unicode field starts with café"
        );
    }

    #[test]
    fn starts_with_only_query_drives_via_index_range_no_scan() {
        let (_tmp, root) = string_op_vault();
        let fields = string_op_index_fields();
        let cache = open_authoritative(&root, &fields);

        let query = starts_with("title", "Alpha");
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        // The scan builder never mentions `document_fields` at all (its
        // string-op clause is pure `json_extract`/`json_each` over
        // `frontmatter_json`) — presence of the table name is the routed-vs-
        // scan signal; the EAV form's branch 2 legitimately still contains
        // `json_extract` as part of its non-text fallback.
        assert!(
            where_sql.contains("document_fields"),
            "a starts-with-only query on an indexed field must route: {where_sql}"
        );
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(&cache, &sql, &binds);

        assert!(
            rows.iter()
                .any(|r| r.contains("SEARCH") && r.contains("idx_document_fields_kv")),
            "expected a driving SEARCH range on idx_document_fields_kv: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
    }

    #[test]
    fn combined_eq_and_starts_with_query_stays_all_search() {
        let (_tmp, root) = string_op_vault();
        let fields = string_op_index_fields();
        let cache = open_authoritative(&root, &fields);

        let query = DocumentQuery {
            frontmatter_eq: vec![("title".to_string(), serde_json::json!("Alpha Report"))],
            frontmatter_starts_with: vec![("tags".to_string(), "release:".to_string())],
            ..Default::default()
        };
        let (where_sql, binds) = build_documents_matching_sql_parts(&cache, &query);
        assert!(
            where_sql.contains("document_fields"),
            "combined --eq + --starts-with on indexed fields must route: {where_sql}"
        );
        let sql = format!("SELECT path FROM documents{where_sql}");
        let rows = explain_plan(&cache, &sql, &binds);

        assert!(
            !rows.iter().any(|r| r.contains("SCAN documents")),
            "must not SCAN documents: {rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("SCAN document_fields")),
            "must not SCAN document_fields: {rows:?}"
        );
    }
}
