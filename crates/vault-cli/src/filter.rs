use std::collections::BTreeSet;

use anyhow::{bail, Result};
use vault_core::{Document, GraphIndex};

pub fn filter_documents<'a>(
    index: &'a GraphIndex,
    filters: &[String],
) -> Result<Vec<&'a Document>> {
    let parsed_filters = filters
        .iter()
        .map(|filter| parse_filter(filter))
        .collect::<Result<Vec<_>>>()?;
    warn_for_absent_filter_fields(index, &parsed_filters);

    Ok(index
        .documents
        .iter()
        .filter(|document| {
            parsed_filters
                .iter()
                .all(|(field, expected)| frontmatter_matches(document, field, expected))
        })
        .collect())
}

fn warn_for_absent_filter_fields(index: &GraphIndex, filters: &[(String, String)]) {
    let frontmatter_keys = index
        .documents
        .iter()
        .filter_map(|document| document.frontmatter.as_ref())
        .flat_map(|frontmatter| {
            frontmatter
                .as_object()
                .into_iter()
                .flat_map(|object| object.keys())
        })
        .collect::<BTreeSet<_>>();

    let mut warned_fields = BTreeSet::new();
    for (field, _) in filters {
        if frontmatter_keys.contains(field) || !warned_fields.insert(field) {
            continue;
        }
        eprintln!(
            "warning: filter field '{field}' is not a frontmatter key in any document; returning empty result"
        );
    }
}

fn parse_filter(filter: &str) -> Result<(String, String)> {
    let Some((field, value)) = filter.split_once(':') else {
        bail!("invalid filter, expected field:value: {filter}");
    };

    let field = field.trim();
    let value = value.trim();
    if field.is_empty() || value.is_empty() {
        bail!("invalid filter, expected non-empty field and value: {filter}");
    }

    Ok((field.to_string(), value.to_string()))
}

fn frontmatter_matches(document: &Document, field: &str, expected: &str) -> bool {
    let Some(frontmatter) = &document.frontmatter else {
        return false;
    };
    let Some(value) = frontmatter.get(field) else {
        return false;
    };

    match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| scalar_value_matches(value, expected)),
        other => scalar_value_matches(other, expected),
    }
}

fn scalar_value_matches(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(actual) => actual == expected,
        serde_json::Value::Bool(actual) => actual.to_string() == expected,
        serde_json::Value::Number(actual) => actual.to_string() == expected,
        _ => false,
    }
}
