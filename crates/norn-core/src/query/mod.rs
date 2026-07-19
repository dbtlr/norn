//! The document-query predicate layer — pure value model + input parsing.
//!
//! [`DocumentQuery`] is the SQL-agnostic predicate model: a bag of typed
//! frontmatter / path / date / body / link predicates a `find` / `count` /
//! `get` invocation asks for. It carries *what* to match, never *how*. The
//! matching itself — SQL emission over the EAV cache (the donor's
//! `json_path_for` JSON-path escaping, `documents_matching`, the string
//! membership machinery) and the Rust post-passes — belongs to the cache
//! engine and is a deliberate seam: this module builds and shapes queries;
//! the engine runs them.
//!
//! Two producers land here:
//!
//! - [`filter_args::build_document_query`] — the canonical path: parse the
//!   read-verb wire vocabulary ([`norn_wire::FilterParams`]) into a
//!   [`DocumentQuery`], applying ADR 0010 separator forgiveness and JSON value
//!   coercion. This supersedes the older CSV adapter below.
//! - [`document_query_from_options`] / [`rule_scope_query`] — the CLI
//!   filter-option adapter and the validate-rule scope narrower. `rule_scope_query`
//!   produces a *superset* scope for a rule's `match` selector (SQL-narrowable);
//!   any caller re-checks candidates with the engine predicate.
//!
//! # Seams left behind (ADR 0018)
//!
//! - **SQL emission** — the whole run side (`json_path_for`, `documents_matching`,
//!   string-membership SQL, the path-glob post-pass) is the cache-engine port.
//! - **`links_to` resolution** — [`filter_args::build_document_query`] leaves
//!   `DocumentQuery::links_to` empty; resolving a `--links-to TARGET` string to a
//!   vault path needs the warm cache + target resolution (the donor's
//!   `resolve_links_to`) and ports with the read verbs.
//! - **Sort / paging execution** — the wire carrier ([`norn_wire::SortPaginateParams`])
//!   already exists; ORDER BY emission and offset/limit slicing are the cache /
//!   verb port, not predicate shaping.

pub mod filter_args;

use serde_json::Value;

use crate::standards::ValidateRule;

/// The SQL-agnostic document predicate model.
///
/// ANY-of within `path_globs` and within each `frontmatter_in` value list;
/// ALL-of across all flag-fields and across vectors.
#[derive(Default, Debug, Clone)]
pub struct DocumentQuery {
    /// Path glob patterns in [`crate::standards::path_match::PathPattern`] syntax.
    /// ANY-of. Empty = no path narrowing. Applied as a Rust post-pass by the engine.
    pub path_globs: Vec<String>,
    /// Frontmatter equality predicates `(field, value)`. ALL-of.
    pub frontmatter_eq: Vec<(String, Value)>,
    /// Frontmatter inequality predicates `(field, value)` — negation of
    /// `frontmatter_eq`. For array-shaped string fields, matches when no
    /// element equals the value. ALL-of.
    pub frontmatter_not_eq: Vec<(String, Value)>,
    /// Required-present fields. ALL-of.
    pub frontmatter_has: Vec<String>,
    /// Required-absent fields. ALL-of.
    pub frontmatter_missing: Vec<String>,
    /// `(field, allowed_values)` — frontmatter field is one of the values
    /// (ANY-of within each entry; ALL-of across entries).
    pub frontmatter_in: Vec<(String, Vec<Value>)>,
    /// `(field, disallowed_values)` — frontmatter field is NOT one of the values.
    pub frontmatter_not_in: Vec<(String, Vec<Value>)>,
    /// `(field, needle)` — `field` starts with `needle`. ALL-of. Anchored
    /// string operator: case-sensitive, array-aware (any element may match),
    /// wikilink-bracket-collapsed on both sides like `frontmatter_eq`. A needle
    /// that is empty after bracket-stripping matches nothing.
    pub frontmatter_starts_with: Vec<(String, String)>,
    /// `(field, needle)` — `field` ends with `needle`. Same semantics as
    /// `frontmatter_starts_with`.
    pub frontmatter_ends_with: Vec<(String, String)>,
    /// `(field, needle)` — `field` contains `needle` as a substring. Same
    /// semantics as `frontmatter_starts_with`.
    pub frontmatter_contains: Vec<(String, String)>,
    /// `(field, date_string)` — `field` < `date_string` (lexical, ISO 8601).
    pub date_before: Vec<(String, String)>,
    /// `(field, date_string)` — `field` > `date_string`.
    pub date_after: Vec<(String, String)>,
    /// `(field, date_string)` — `field` = `date_string`.
    pub date_on: Vec<(String, String)>,
    /// Body-text substring; case-insensitive.
    pub body_text_contains: Option<String>,
    /// Documents whose outgoing links resolve to ALL of these (resolved) paths.
    /// ALL-of. Resolved-only: matched against `links.resolved_path`. Targets are
    /// resolved to paths at the command layer (the deferred `resolve_links_to`).
    pub links_to: Vec<camino::Utf8PathBuf>,
    /// True ⇒ restrict to documents with ≥1 link whose status is unresolved.
    /// Ambiguous-status links are excluded (distinct state, own validate codes).
    pub has_unresolved_links: bool,
}

/// The document-filter option set shared by the query commands — the borrowed
/// slices a `find` / `count` / `get` invocation asked for.
#[derive(Debug)]
pub struct DocumentFilterOptions<'a> {
    pub filters: &'a [String],
    pub paths: &'a [String],
    pub has: &'a [String],
    pub missing: &'a [String],
}

/// Convert the CLI's document filter input into a [`DocumentQuery`].
///
/// CLI `filters` of the form `"field:value,value,..."` translate into one
/// `frontmatter_eq` entry per value. Note: each entry is ALL-of with the
/// others, which means CSV expands to "matches every value" — usually empty
/// for single-valued frontmatter fields. The canonical
/// [`filter_args::build_document_query`] path instead routes ANY-of through
/// `--in`; this older adapter's CSV-as-ALL-of is a preserved parity behavior,
/// not a fixed one (single-value filters work correctly either way).
pub fn document_query_from_options(
    options: &DocumentFilterOptions<'_>,
) -> anyhow::Result<DocumentQuery> {
    let mut query = DocumentQuery {
        path_globs: options.paths.to_vec(),
        frontmatter_has: options.has.to_vec(),
        frontmatter_missing: options.missing.to_vec(),
        ..Default::default()
    };
    for filter in options.filters {
        let (field, values_csv) = filter
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid filter, expected field:value: {filter}"))?;
        let field = field.trim().to_string();
        for value in values_csv
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            query
                .frontmatter_eq
                .push((field.clone(), Value::String(value.to_string())));
        }
    }
    Ok(query)
}

/// Convert a validate rule's `match` predicates into a [`DocumentQuery`] so the
/// per-rule scope can be SQL-narrowed.
///
/// The narrowing is a *superset* filter, not the authoritative predicate: the
/// engine's string membership also matches array-valued document fields and
/// collapses wikilink brackets, both of which the strict frontmatter predicate
/// rejects. Any caller must re-check candidates with the engine predicate.
pub fn rule_scope_query(rule: &ValidateRule) -> DocumentQuery {
    let mut query = DocumentQuery::default();
    if let Some(pattern) = &rule.r#match.path {
        query.path_globs.push(pattern.clone());
    }
    for (field, expected) in &rule.r#match.frontmatter {
        match expected {
            Value::Array(options) => query.frontmatter_in.push((field.clone(), options.clone())),
            scalar => query.frontmatter_eq.push((field.clone(), scalar.clone())),
        }
    }
    query
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn options_with_filter_translates_to_frontmatter_eq() {
        let filters = vec!["type:note".to_string()];
        let opts = DocumentFilterOptions {
            filters: &filters,
            paths: &[],
            has: &[],
            missing: &[],
        };
        let q = document_query_from_options(&opts).unwrap();
        assert_eq!(q.frontmatter_eq, vec![("type".to_string(), json!("note"))]);
    }

    #[test]
    fn options_csv_filter_expands_to_multiple_predicates() {
        // Preserved parity behavior: CSV becomes ALL-of via repeated
        // frontmatter_eq entries. Single-value filters are fine.
        let filters = vec!["type:note,log".to_string()];
        let opts = DocumentFilterOptions {
            filters: &filters,
            paths: &[],
            has: &[],
            missing: &[],
        };
        let q = document_query_from_options(&opts).unwrap();
        assert_eq!(q.frontmatter_eq.len(), 2);
    }

    #[test]
    fn options_invalid_filter_without_colon_errors() {
        let filters = vec!["nocolon".to_string()];
        let opts = DocumentFilterOptions {
            filters: &filters,
            paths: &[],
            has: &[],
            missing: &[],
        };
        assert!(document_query_from_options(&opts).is_err());
    }

    #[test]
    fn rule_scope_list_predicate_maps_to_frontmatter_in() {
        use crate::standards::ValidateRule;

        let mut rule = ValidateRule::default();
        rule.r#match
            .frontmatter
            .insert("type".to_string(), json!(["task", "phase"]));

        let q = rule_scope_query(&rule);

        assert!(q.frontmatter_eq.is_empty());
        assert_eq!(
            q.frontmatter_in,
            vec![("type".to_string(), vec![json!("task"), json!("phase")])]
        );
    }

    #[test]
    fn rule_scope_picks_up_match_path_and_frontmatter() {
        use crate::standards::ValidateRule;

        let mut rule = ValidateRule::default();
        rule.r#match.path = Some("Workspaces/**/*.md".to_string());
        rule.r#match
            .frontmatter
            .insert("type".to_string(), json!("note"));

        let q = rule_scope_query(&rule);

        assert_eq!(q.path_globs, vec!["Workspaces/**/*.md".to_string()]);
        assert_eq!(q.frontmatter_eq, vec![("type".to_string(), json!("note"))]);
    }
}
