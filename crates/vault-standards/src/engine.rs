use vault_core::{Document, GraphIndex, LinkStatus};
use vault_graph::pattern_matches_path;

use crate::config::{ValidateConfig, ValidateRule};
use crate::findings::Finding;
use crate::predicates::{
    document_frontmatter_field, document_has_frontmatter_field, frontmatter_predicates_match,
    frontmatter_type_matches, frontmatter_value_matches,
};

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

            for field in &rule.forbidden_frontmatter {
                if let Some(actual) = document_frontmatter_field(document, field) {
                    findings.push(Finding::frontmatter_forbidden_field(
                        document.path.clone(),
                        rule.name.clone(),
                        field.clone(),
                        actual.clone(),
                    ));
                }
            }

            if !rule.allowed_paths.is_empty()
                && !rule
                    .allowed_paths
                    .iter()
                    .any(|pattern| pattern_matches_path(pattern, &document.path))
            {
                findings.push(Finding::document_misrouted(
                    document.path.clone(),
                    rule.name.clone(),
                    rule.allowed_paths.clone(),
                ));
            }

            for (field, allowed_values) in &rule.allowed_values {
                if let Some(actual) = document_frontmatter_field(document, field) {
                    if !allowed_values
                        .iter()
                        .any(|av| frontmatter_value_matches(actual, av))
                    {
                        findings.push(Finding::frontmatter_disallowed_value(
                            document.path.clone(),
                            rule.name.clone(),
                            field.clone(),
                            actual.clone(),
                            allowed_values.clone(),
                        ));
                    }
                }
            }
        }

        for link in &document.links {
            match link.status {
                LinkStatus::Resolved => {}
                LinkStatus::Unresolved => {
                    findings.push(Finding::link_unresolved(
                        document.path.clone(),
                        link.clone(),
                    ));
                }
                LinkStatus::Ambiguous => {
                    findings.push(Finding::link_ambiguous(document.path.clone(), link.clone()));
                }
            }
        }
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
