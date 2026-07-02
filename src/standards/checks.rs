use std::collections::HashMap;

use crate::core::{Document, LinkStatus};
use crate::standards::findings::Finding;
use crate::standards::path_match::PathPattern;
use camino::Utf8PathBuf;
use serde_json::Value;

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
        .filter(|field| {
            !crate::standards::predicates::document_has_frontmatter_field(document, field)
        })
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
    types: &HashMap<String, crate::standards::config::FieldTypeSpec>,
    rule: Option<&str>,
) -> Vec<Finding> {
    types
        .iter()
        .filter_map(|(field, spec)| {
            let actual = crate::standards::predicates::document_frontmatter_field(document, field)?;
            let expected_type = spec.type_name();
            if crate::standards::predicates::frontmatter_type_matches(
                actual,
                expected_type,
                spec.effective_max_length(),
            ) {
                None
            } else {
                Some(Finding::frontmatter_invalid_type(
                    document.path.clone(),
                    rule.map(str::to_string),
                    field.clone(),
                    actual.clone(),
                    expected_type.to_string(),
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
            let actual = crate::standards::predicates::document_frontmatter_field(document, field)?;
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
            let actual = crate::standards::predicates::document_frontmatter_field(document, field)?;
            if allowed_values
                .iter()
                .any(|av| crate::standards::predicates::frontmatter_value_matches(actual, av))
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

// Superseded by check_allowed_paths_compiled (hot path). Only the dead
// validate_rule_compiled fallback (when compiled patterns are absent) calls
// this. Safe to delete in a cleanup pass.
#[allow(dead_code)]
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

/// Typed-reference constraint (`field_references`): each frontmatter
/// wikilink in a constrained field must resolve to a document whose `type`
/// is in the allowed set. Only **resolved** links are judged — unresolved
/// and ambiguous references are link validation's findings (`link-*`), not
/// a reference-type violation. A resolved target without a `type` field is
/// outside every set and reports as `(missing)`; a non-string `type` cannot
/// satisfy any set and reports its JSON rendering. Targets absent from
/// `type_by_path` (documents excluded by `validate.ignore`) are skipped —
/// their frontmatter is explicitly outside the validation contract.
pub(crate) fn check_field_references(
    document: &Document,
    field_references: &HashMap<String, crate::standards::config::FieldReferenceConstraint>,
    type_by_path: &std::collections::BTreeMap<&camino::Utf8Path, Option<&Value>>,
    rule: Option<&str>,
) -> Vec<Finding> {
    if field_references.is_empty() {
        return Vec::new();
    }
    // Deterministic finding order regardless of HashMap iteration.
    let mut fields: Vec<(&String, &crate::standards::config::FieldReferenceConstraint)> =
        field_references.iter().collect();
    fields.sort_by_key(|(field, _)| field.as_str());

    let mut findings = Vec::new();
    for (field, constraint) in fields {
        let allowed = constraint.allowed_types();
        for link in &document.links {
            let in_field = link.source_context.as_ref().is_some_and(|ctx| {
                matches!(ctx.area, crate::core::LinkSourceArea::Frontmatter)
                    && ctx.property.as_deref() == Some(field.as_str())
            });
            if !in_field || link.status != LinkStatus::Resolved {
                continue;
            }
            let Some(target) = link.resolved_path.as_ref() else {
                continue;
            };
            // Map miss = the target is outside the validation contract
            // (validate.ignore) — never judged. A present-but-None entry is
            // a validated doc without a `type` field.
            let Some(target_type) = type_by_path.get(target.as_path()) else {
                continue;
            };
            let satisfied = target_type
                .and_then(|value| value.as_str())
                .is_some_and(|actual| allowed.iter().any(|ty| ty == actual));
            if !satisfied {
                let actual = match target_type {
                    None => "(missing)".to_string(),
                    Some(Value::String(actual)) => (*actual).clone(),
                    // Non-string types can never satisfy a set of type
                    // names; report their JSON rendering honestly.
                    Some(other) => other.to_string(),
                };
                findings.push(Finding::frontmatter_reference_type(
                    document.path.clone(),
                    rule.map(str::to_string),
                    field.clone(),
                    link.raw.clone(),
                    target.clone(),
                    actual,
                    allowed.clone(),
                ));
            }
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
