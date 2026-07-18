//! CLI-side adapters: convert `DocumentFilterOptions` and validate
//! `ValidateRule.match` into `crate::cache::DocumentQuery`.

use crate::cache::DocumentQuery;
use crate::standards::ValidateRule;

use crate::filter::DocumentFilterOptions;

/// Convert the CLI's document filter input into a Cache DocumentQuery.
/// CLI `filters` of the form `"field:value,value,..."` translate into one
/// `frontmatter_eq` entry per value. Note: each entry is ALL-of with the
/// others, which means CSV expands to "matches every value" — usually empty
/// for single-valued frontmatter fields. v1's `filter_documents` instead
/// treats CSV as ANY-of within the same field. This is a known parity gap
/// for CSV filters; single-value filters work correctly. The round-trip
/// property tests in vault-cache cover the single-value case; downstream
/// command migrations (T12+) will confirm whether CSV-filter snapshot
/// tests exist that catch this.
#[allow(dead_code)]
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
                .push((field.clone(), serde_json::Value::String(value.to_string())));
        }
    }
    Ok(query)
}

/// Convert a validate rule's `match` predicates into a DocumentQuery so
/// the per-rule scope can be SQL-narrowed.
///
/// The narrowing is a *superset* filter, not the authoritative predicate:
/// the SQL membership machinery (`push_string_membership`) also matches
/// array-valued document fields and collapses wikilink brackets, both of
/// which the engine's `frontmatter_predicate_matches` rejects. Any caller
/// must re-check candidates with the engine predicate (`rule_matches`).
#[allow(dead_code)]
pub fn rule_scope_query(rule: &ValidateRule) -> DocumentQuery {
    let mut query = DocumentQuery::default();
    if let Some(pattern) = &rule.r#match.path {
        query.path_globs.push(pattern.clone());
    }
    for (field, expected) in &rule.r#match.frontmatter {
        match expected {
            serde_json::Value::Array(options) => {
                query.frontmatter_in.push((field.clone(), options.clone()))
            }
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
        let paths: Vec<String> = vec![];
        let has: Vec<String> = vec![];
        let missing: Vec<String> = vec![];
        let opts = DocumentFilterOptions {
            filters: &filters,
            paths: &paths,
            has: &has,
            missing: &missing,
        };
        let q = document_query_from_options(&opts).unwrap();
        assert_eq!(q.frontmatter_eq, vec![("type".to_string(), json!("note"))]);
    }

    #[test]
    fn options_csv_filter_expands_to_multiple_predicates() {
        // Known parity caveat: CSV becomes ALL-of via repeated frontmatter_eq
        // entries; v1 treats CSV as ANY-of. Single-value filters are fine.
        let filters = vec!["type:note,log".to_string()];
        let paths: Vec<String> = vec![];
        let has: Vec<String> = vec![];
        let missing: Vec<String> = vec![];
        let opts = DocumentFilterOptions {
            filters: &filters,
            paths: &paths,
            has: &has,
            missing: &missing,
        };
        let q = document_query_from_options(&opts).unwrap();
        assert_eq!(q.frontmatter_eq.len(), 2);
    }

    #[test]
    fn rule_scope_list_predicate_maps_to_frontmatter_in() {
        use crate::standards::{RuleExclude, RuleSelector, ValidateRule};
        use std::collections::HashMap;

        let mut fm = HashMap::new();
        fm.insert("type".to_string(), json!(["task", "phase"]));
        let rule = ValidateRule {
            name: None,
            r#match: RuleSelector {
                path: None,
                path_not: None,
                frontmatter: fm,
            },
            exclude: RuleExclude { path: None },
            required_frontmatter: vec![],
            forbidden_frontmatter: vec![],
            field_types: HashMap::new(),
            allowed_values: HashMap::new(),
            allowed_paths: vec![],
            frontmatter_defaults: HashMap::new(),
            ..Default::default()
        };

        let q = rule_scope_query(&rule);

        assert!(q.frontmatter_eq.is_empty());
        assert_eq!(
            q.frontmatter_in,
            vec![("type".to_string(), vec![json!("task"), json!("phase")])]
        );
    }

    #[test]
    fn rule_scope_picks_up_match_path_and_frontmatter() {
        use crate::standards::{RuleExclude, RuleSelector, ValidateRule};
        use std::collections::HashMap;

        let mut fm = HashMap::new();
        fm.insert("type".to_string(), json!("note"));
        let rule = ValidateRule {
            name: None,
            r#match: RuleSelector {
                path: Some("Workspaces/**/*.md".to_string()),
                path_not: None,
                frontmatter: fm,
            },
            exclude: RuleExclude { path: None },
            required_frontmatter: vec![],
            forbidden_frontmatter: vec![],
            field_types: HashMap::new(),
            allowed_values: HashMap::new(),
            allowed_paths: vec![],
            frontmatter_defaults: HashMap::new(),
            ..Default::default()
        };

        let q = rule_scope_query(&rule);

        assert_eq!(q.path_globs, vec!["Workspaces/**/*.md".to_string()]);
        assert_eq!(q.frontmatter_eq, vec![("type".to_string(), json!("note"))]);
    }
}
