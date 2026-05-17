use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Result};
use camino::Utf8Path;
use serde::Serialize;
use vault_core::{Document, GraphIndex};
use vault_graph::pattern_matches_path;

#[derive(Debug)]
pub struct DocumentFilterOptions<'a> {
    pub filters: &'a [String],
    pub paths: &'a [String],
    pub has: &'a [String],
    pub missing: &'a [String],
}

#[derive(Debug, Serialize)]
pub struct DocumentSummary {
    pub count_by: String,
    pub total: usize,
    pub counts: BTreeMap<String, usize>,
}

#[derive(Debug)]
struct ParsedFilter {
    field: String,
    values: Vec<String>,
}

pub fn filter_documents<'a>(
    index: &'a GraphIndex,
    options: &DocumentFilterOptions<'_>,
) -> Result<Vec<&'a Document>> {
    let parsed_filters = options
        .filters
        .iter()
        .map(|filter| parse_filter(filter))
        .collect::<Result<Vec<_>>>()?;
    let known_fields = frontmatter_keys(index);
    warn_for_absent_filter_fields(&known_fields, &parsed_filters, options.has, options.missing);

    Ok(index
        .documents
        .iter()
        .filter(|document| document_matches(document, options, &parsed_filters, &known_fields))
        .collect())
}

pub fn summarize_documents(
    documents: &[&Document],
    count_by: &str,
    known_fields: &BTreeSet<String>,
) -> DocumentSummary {
    if !known_fields.contains(count_by) {
        eprintln!(
            "warning: count-by field '{count_by}' is not a frontmatter key in any document; returning empty counts"
        );
        return DocumentSummary {
            count_by: count_by.to_string(),
            total: documents.len(),
            counts: BTreeMap::new(),
        };
    }

    let mut counts = BTreeMap::new();
    for document in documents {
        let Some(value) = frontmatter_value(document, count_by) else {
            continue;
        };
        count_value(value, &mut counts);
    }

    DocumentSummary {
        count_by: count_by.to_string(),
        total: documents.len(),
        counts,
    }
}

pub fn index_frontmatter_keys(index: &GraphIndex) -> BTreeSet<String> {
    frontmatter_keys(index)
}

fn document_matches(
    document: &Document,
    options: &DocumentFilterOptions<'_>,
    filters: &[ParsedFilter],
    known_fields: &BTreeSet<String>,
) -> bool {
    paths_match(&document.path, options.paths)
        && filters
            .iter()
            .all(|filter| frontmatter_matches(document, filter))
        && options.has.iter().all(|field| {
            known_fields.contains(field.as_str()) && document_has_frontmatter_field(document, field)
        })
        && options.missing.iter().all(|field| {
            known_fields.contains(field.as_str())
                && !document_has_frontmatter_field(document, field)
        })
}

fn paths_match(path: &Utf8Path, patterns: &[String]) -> bool {
    patterns.is_empty()
        || patterns
            .iter()
            .any(|pattern| pattern_matches_path(pattern, path))
}

fn warn_for_absent_filter_fields(
    known_fields: &BTreeSet<String>,
    filters: &[ParsedFilter],
    has_fields: &[String],
    missing_fields: &[String],
) {
    let mut warned_fields = BTreeSet::new();
    for (operator, field) in filters
        .iter()
        .map(|filter| ("--filter", filter.field.as_str()))
        .chain(has_fields.iter().map(|field| ("--has", field.as_str())))
        .chain(
            missing_fields
                .iter()
                .map(|field| ("--missing", field.as_str())),
        )
    {
        if known_fields.contains(field) || !warned_fields.insert(field.to_string()) {
            continue;
        }
        eprintln!(
            "warning: {operator} field '{field}' is not a frontmatter key in any document; returning empty result"
        );
    }
}

fn frontmatter_keys(index: &GraphIndex) -> BTreeSet<String> {
    index
        .documents
        .iter()
        .filter_map(|document| document.frontmatter.as_ref())
        .flat_map(|frontmatter| {
            frontmatter
                .as_object()
                .into_iter()
                .flat_map(|object| object.keys().cloned())
        })
        .collect()
}

fn parse_filter(filter: &str) -> Result<ParsedFilter> {
    let Some((field, value)) = filter.split_once(':') else {
        bail!("invalid filter, expected field:value: {filter}");
    };

    let field = field.trim();
    let value = value.trim();
    if field.is_empty() || value.is_empty() {
        bail!("invalid filter, expected non-empty field and value: {filter}");
    }

    let values = value
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if values.iter().any(String::is_empty) {
        bail!("invalid filter, expected non-empty comma-separated values: {filter}");
    }

    Ok(ParsedFilter {
        field: field.to_string(),
        values,
    })
}

fn frontmatter_matches(document: &Document, filter: &ParsedFilter) -> bool {
    let Some(value) = frontmatter_value(document, &filter.field) else {
        return false;
    };

    filter.values.iter().any(|expected| match value {
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| scalar_value_matches(value, expected)),
        other => scalar_value_matches(other, expected),
    })
}

fn document_has_frontmatter_field(document: &Document, field: &str) -> bool {
    frontmatter_value(document, field).is_some_and(|value| !value.is_null())
}

fn frontmatter_value<'a>(document: &'a Document, field: &str) -> Option<&'a serde_json::Value> {
    document.frontmatter.as_ref()?.get(field)
}

fn scalar_value_matches(value: &serde_json::Value, expected: &str) -> bool {
    match value {
        serde_json::Value::String(actual) => actual == expected,
        serde_json::Value::Bool(actual) => actual.to_string() == expected,
        serde_json::Value::Number(actual) => actual.to_string() == expected,
        _ => false,
    }
}

fn count_value(value: &serde_json::Value, counts: &mut BTreeMap<String, usize>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                count_scalar_value(value, counts);
            }
        }
        value => count_scalar_value(value, counts),
    }
}

fn count_scalar_value(value: &serde_json::Value, counts: &mut BTreeMap<String, usize>) {
    let key = match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Null => "null".to_string(),
        value => value.to_string(),
    };
    *counts.entry(key).or_default() += 1;
}
