use vault_core::{Document, GraphIndex};
use vault_graph::pattern_matches_path;

use crate::config::{ValidateConfig, ValidateRule};
use crate::findings::Finding;
use crate::predicates::frontmatter_predicates_match;

pub fn validate(index: &GraphIndex, config: &ValidateConfig) -> Vec<Finding> {
    let mut findings = Vec::new();

    for document in &index.documents {
        if document_ignored(document, config) {
            continue;
        }

        findings.extend(crate::checks::check_graph_diagnostics(document));

        findings.extend(crate::checks::check_required_frontmatter(
            document,
            &config.required_frontmatter,
            None,
        ));

        for rule in matching_rules(document, &config.rules) {
            findings.extend(crate::checks::check_required_frontmatter(
                document,
                &rule.required_frontmatter,
                rule.name.as_deref(),
            ));

            findings.extend(crate::checks::check_field_types(
                document,
                &rule.field_types,
                rule.name.as_deref(),
            ));

            findings.extend(crate::checks::check_forbidden_frontmatter(
                document,
                &rule.forbidden_frontmatter,
                rule.name.as_deref(),
            ));

            if let Some(finding) = crate::checks::check_allowed_paths(
                document,
                &rule.allowed_paths,
                rule.name.as_deref(),
            ) {
                findings.push(finding);
            }

            findings.extend(crate::checks::check_allowed_values(
                document,
                &rule.allowed_values,
                rule.name.as_deref(),
            ));
        }

        findings.extend(crate::checks::check_links(document));
    }

    findings
}

pub(crate) fn document_ignored(document: &Document, config: &ValidateConfig) -> bool {
    config
        .ignore
        .iter()
        .any(|pattern| pattern_matches_path(pattern, &document.path))
}

pub(crate) fn matching_rules<'a>(
    document: &Document,
    rules: &'a [ValidateRule],
) -> Vec<&'a ValidateRule> {
    rules
        .iter()
        .filter(|rule| rule_matches(document, rule))
        .collect()
}

pub(crate) fn rule_matches(document: &Document, rule: &ValidateRule) -> bool {
    if let Some(path_pattern) = &rule.r#match.path {
        if !pattern_matches_path(path_pattern, &document.path) {
            return false;
        }
    }
    if let Some(path_not_pattern) = &rule.r#match.path_not {
        if pattern_matches_path(path_not_pattern, &document.path) {
            return false;
        }
    }
    if let Some(exclude_path) = &rule.exclude.path {
        if pattern_matches_path(exclude_path, &document.path) {
            return false;
        }
    }
    frontmatter_predicates_match(document, &rule.r#match.frontmatter)
}
