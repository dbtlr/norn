use std::collections::HashMap;

use serde_json::Value;
use vault_core::{Document, LinkStatus};
use vault_graph::pattern_matches_path;

use crate::findings::Finding;

pub(crate) fn check_graph_diagnostics(document: &Document) -> Vec<Finding> {
    document
        .diagnostics
        .iter()
        .map(|diagnostic| Finding::from_graph_diagnostic(document.path.clone(), diagnostic.clone()))
        .collect()
}

pub(crate) fn check_required_frontmatter(
    document: &Document,
    fields: &[String],
    rule: Option<&str>,
) -> Vec<Finding> {
    fields
        .iter()
        .filter(|field| !crate::predicates::document_has_frontmatter_field(document, field))
        .map(|field| {
            Finding::frontmatter_required_missing(
                document.path.clone(),
                rule.map(str::to_string),
                field.clone(),
            )
        })
        .collect()
}

pub(crate) fn check_field_types(
    document: &Document,
    types: &HashMap<String, String>,
    rule: Option<&str>,
) -> Vec<Finding> {
    types
        .iter()
        .filter_map(|(field, expected_type)| {
            let actual = crate::predicates::document_frontmatter_field(document, field)?;
            if crate::predicates::frontmatter_type_matches(actual, expected_type) {
                None
            } else {
                Some(Finding::frontmatter_invalid_type(
                    document.path.clone(),
                    rule.map(str::to_string),
                    field.clone(),
                    actual.clone(),
                    expected_type.clone(),
                ))
            }
        })
        .collect()
}

pub(crate) fn check_forbidden_frontmatter(
    document: &Document,
    fields: &[String],
    rule: Option<&str>,
) -> Vec<Finding> {
    fields
        .iter()
        .filter_map(|field| {
            let actual = crate::predicates::document_frontmatter_field(document, field)?;
            Some(Finding::frontmatter_forbidden_field(
                document.path.clone(),
                rule.map(str::to_string),
                field.clone(),
                actual.clone(),
            ))
        })
        .collect()
}

pub(crate) fn check_allowed_values(
    document: &Document,
    values: &HashMap<String, Vec<Value>>,
    rule: Option<&str>,
) -> Vec<Finding> {
    values
        .iter()
        .filter_map(|(field, allowed_values)| {
            let actual = crate::predicates::document_frontmatter_field(document, field)?;
            if allowed_values
                .iter()
                .any(|av| crate::predicates::frontmatter_value_matches(actual, av))
            {
                None
            } else {
                Some(Finding::frontmatter_disallowed_value(
                    document.path.clone(),
                    rule.map(str::to_string),
                    field.clone(),
                    actual.clone(),
                    allowed_values.clone(),
                ))
            }
        })
        .collect()
}

pub(crate) fn check_allowed_paths(
    document: &Document,
    paths: &[String],
    rule: Option<&str>,
) -> Option<Finding> {
    if paths.is_empty() {
        return None;
    }
    if paths
        .iter()
        .any(|pattern| pattern_matches_path(pattern, &document.path))
    {
        return None;
    }
    Some(Finding::document_misrouted(
        document.path.clone(),
        rule.map(str::to_string),
        paths.to_vec(),
    ))
}

pub(crate) fn check_links(document: &Document) -> Vec<Finding> {
    document
        .links
        .iter()
        .filter_map(|link| match link.status {
            LinkStatus::Resolved => None,
            LinkStatus::Unresolved => Some(Finding::link_unresolved(
                document.path.clone(),
                link.clone(),
            )),
            LinkStatus::Ambiguous => {
                Some(Finding::link_ambiguous(document.path.clone(), link.clone()))
            }
        })
        .collect()
}
