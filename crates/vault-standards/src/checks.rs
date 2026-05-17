use std::collections::HashMap;

use vault_core::Document;

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
