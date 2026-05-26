use std::collections::HashMap;

use crate::findings::Finding;
use crate::path_match::PathPattern;
use camino::Utf8PathBuf;
use serde_json::Value;
use vault_core::{Document, LinkStatus};

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
    if paths.iter().any(|pattern| {
        PathPattern::parse(pattern)
            .map(|p| p.match_path(document.path.as_str()).is_some())
            .unwrap_or(false)
    }) {
        return None;
    }
    Some(Finding::document_misrouted(
        document.path.clone(),
        rule.map(str::to_string),
        paths.to_vec(),
    ))
}

/// Like `check_allowed_paths` but uses pre-compiled `PathPattern` values.
/// `raw_paths` is passed through as the finding's allowed-path list.
pub(crate) fn check_allowed_paths_compiled(
    document: &Document,
    compiled_paths: &[PathPattern],
    raw_paths: &[String],
    rule: Option<&str>,
) -> Option<Finding> {
    if raw_paths.is_empty() {
        return None;
    }
    if compiled_paths
        .iter()
        .any(|p| p.match_path(document.path.as_str()).is_some())
    {
        return None;
    }
    Some(Finding::document_misrouted(
        document.path.clone(),
        rule.map(str::to_string),
        raw_paths.to_vec(),
    ))
}

pub(crate) fn check_alias_malformed(
    document: &Document,
    alias_field: Option<&str>,
) -> Vec<Finding> {
    let Some(field) = alias_field else {
        return Vec::new();
    };
    if document.alias_malformed.is_empty() {
        return Vec::new();
    }
    vec![Finding::frontmatter_alias_malformed(
        document.path.clone(),
        field.to_string(),
        document.alias_malformed.clone(),
    )]
}

pub(crate) fn check_alias_shadowed_by_stem(
    documents: &[&Document],
    alias_field: Option<&str>,
) -> Vec<Finding> {
    if alias_field.is_none() {
        return Vec::new();
    }
    // Build stem -> all docs with that stem (case-insensitive). Stems can collide;
    // shadow finding fires against ANY stem match.
    let mut by_stem_lower: std::collections::HashMap<String, Vec<&Document>> =
        std::collections::HashMap::new();
    for doc in documents {
        by_stem_lower
            .entry(doc.stem.to_lowercase())
            .or_default()
            .push(doc);
    }
    let mut findings = Vec::new();
    for doc in documents {
        for alias in &doc.aliases {
            // alias is already lowercased upstream
            if let Some(matches) = by_stem_lower.get(alias) {
                for shadowing in matches {
                    findings.push(Finding::frontmatter_alias_shadowed_by_stem(
                        doc.path.clone(),
                        alias.clone(),
                        shadowing.path.clone(),
                    ));
                }
            }
        }
    }
    findings
}

pub(crate) fn check_alias_duplicate_across_docs(
    documents: &[&Document],
    alias_field: Option<&str>,
) -> Vec<Finding> {
    if alias_field.is_none() {
        return Vec::new();
    }
    // alias-key -> Vec<doc references>
    let mut by_alias: std::collections::HashMap<&str, Vec<&Document>> =
        std::collections::HashMap::new();
    for doc in documents {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for alias in &doc.aliases {
            if seen.insert(alias.as_str()) {
                by_alias.entry(alias.as_str()).or_default().push(doc);
            }
        }
    }
    let mut findings = Vec::new();
    for (alias_value, docs) in by_alias {
        if docs.len() < 2 {
            continue;
        }
        for &doc in &docs {
            let peers: Vec<Utf8PathBuf> = docs
                .iter()
                .filter(|peer| peer.path != doc.path)
                .map(|peer| peer.path.clone())
                .collect();
            findings.push(Finding::frontmatter_alias_duplicate_across_docs(
                doc.path.clone(),
                alias_value.to_string(),
                peers,
            ));
        }
    }
    findings
}

pub(crate) fn check_links(document: &Document) -> Vec<Finding> {
    document
        .links
        .iter()
        .filter_map(|link| match link.status {
            LinkStatus::Resolved => None,
            LinkStatus::Unresolved | LinkStatus::Ambiguous => {
                Some(Finding::from_link(document.path.clone(), link.clone()))
            }
        })
        .collect()
}
