//! Per-check finding producers for the validate engine.
//!
//! Ported from the donor `src/standards/checks.rs` (ADR 0018). Each `check_*`
//! turns one document + one rule constraint into zero-or-more [`Finding`]s via
//! the [`Finding`] constructors; the [`engine`](super::engine) orchestrates the
//! order they run in. Pure functions of a [`Document`] — no IO.

use std::collections::HashMap;

use crate::domain::{Document, LinkStatus};
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
    // Sorted for deterministic finding order (HashMap iteration is unordered;
    // matches the check_field_references precedent).
    let mut types: Vec<_> = types.iter().collect();
    types.sort_by(|a, b| a.0.cmp(b.0));
    types
        .into_iter()
        .filter_map(|(field, spec)| {
            // A type-less extended entry (`{ indexed: bool }`) contributes only
            // to the index vote (see index_policy) — it declares no type here.
            let expected_type = spec.type_name()?;
            let actual = crate::standards::predicates::document_frontmatter_field(document, field)?;
            let max_length = spec.effective_max_length();
            if crate::standards::predicates::frontmatter_type_matches(
                actual,
                expected_type,
                max_length,
            ) {
                return None;
            }
            if let Some(actual_length) =
                crate::standards::predicates::frontmatter_exceeds_max_length(
                    actual,
                    expected_type,
                    max_length,
                )
            {
                return Some(Finding::frontmatter_exceeds_max_length(
                    document.path.clone(),
                    rule.map(str::to_string),
                    field.clone(),
                    actual.clone(),
                    max_length.expect("a length violation implies an effective bound"),
                    actual_length,
                ));
            }
            Some(Finding::frontmatter_invalid_type(
                document.path.clone(),
                rule.map(str::to_string),
                field.clone(),
                actual.clone(),
                expected_type.to_string(),
            ))
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
    // Sorted for deterministic finding order.
    let mut values: Vec<_> = values.iter().collect();
    values.sort_by(|a, b| a.0.cmp(b.0));
    values
        .into_iter()
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

/// Non-compiled allowed-path check, retained for the `#[cfg(test)]` per-rule
/// path. The hot engine path uses [`check_allowed_paths_compiled`]; this parses
/// its patterns per call, so it is gated to test builds only.
#[cfg(test)]
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
    // Sorted for deterministic finding order.
    let mut groups: Vec<_> = by_alias.into_iter().collect();
    groups.sort_by(|a, b| a.0.cmp(b.0));
    for (alias_value, docs) in groups {
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
                matches!(ctx.area, crate::domain::LinkSourceArea::Frontmatter)
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

/// (NRN-368) Cross-platform-illegal characters in a path segment:
/// NTFS-forbidden, Obsidian refuses them outright, macOS half-breaks a colon
/// (renders it as `/` in Finder while the byte on disk stays `:`), and a
/// backslash — legal in a Unix filename, but a path separator on Windows and
/// therefore forbidden in a Windows filename.
const NONPORTABLE_FILENAME_CHARS: [char; 8] = [':', '*', '?', '"', '<', '>', '|', '\\'];

/// (NRN-368) Every portability issue across every `/`-separated segment of
/// `path`: an illegal character (see [`NONPORTABLE_FILENAME_CHARS`]), or a
/// leading/trailing space, or a trailing dot (all three Windows-illegal
/// segment shapes). `.`/`..`/empty segments are not real filenames and are
/// skipped. Order is segment-then-check, matching the path's own left-to-right
/// reading order — deterministic given a deterministic path.
fn portable_filename_issues(path: &str) -> Vec<String> {
    let mut issues = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        for &ch in &NONPORTABLE_FILENAME_CHARS {
            if segment.contains(ch) {
                issues.push(format!(
                    "segment '{segment}' contains illegal character '{ch}'"
                ));
            }
        }
        if segment.starts_with(' ') {
            issues.push(format!("segment '{segment}' has a leading space"));
        }
        if segment.ends_with(' ') {
            issues.push(format!("segment '{segment}' has a trailing space"));
        }
        if segment.ends_with('.') {
            issues.push(format!("segment '{segment}' has a trailing dot"));
        }
    }
    issues
}

/// (NRN-368) One finding per document naming every portability issue across
/// every path segment — never more than one finding even when several
/// segments or violation classes fire. Warning severity, no auto-repair: a
/// rename cascades every backlink, which is a move, not a diagnose-only fix.
pub(crate) fn check_portable_filename(document: &Document) -> Option<Finding> {
    let issues = portable_filename_issues(document.path.as_str());
    if issues.is_empty() {
        return None;
    }
    Some(Finding::nonportable_filename(document.path.clone(), issues))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standards::config::FieldTypeSpec;
    use crate::standards::findings::FindingBody;
    use serde_json::json;

    fn doc_with_frontmatter(fm: serde_json::Value) -> Document {
        Document {
            path: Utf8PathBuf::from("notes/foo.md"),
            stem: "foo".to_string(),
            hash: "h".into(),
            frontmatter: Some(fm),
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    fn field_types(field_types_yaml: &str) -> HashMap<String, FieldTypeSpec> {
        let yaml =
            format!("validate:\n  rules:\n    - name: r\n      field_types:\n{field_types_yaml}");
        let cfg = crate::standards::parse_config(&yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse");
        cfg.validate.rules[0].field_types.clone()
    }

    #[test]
    fn over_length_string_reports_exceeds_max_length_not_invalid_type() {
        let types = field_types("        project: { type: string, max_length: 5 }\n");
        let doc = doc_with_frontmatter(json!({"project": "toolong"}));
        let findings = check_field_types(&doc, &types, None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "frontmatter-exceeds-max-length");
        match &findings[0].body {
            FindingBody::ExceedsMaxLength {
                field,
                max_length,
                actual_length,
                ..
            } => {
                assert_eq!(field, "project");
                assert_eq!(*max_length, 5);
                assert_eq!(*actual_length, 7);
            }
            other => panic!("expected ExceedsMaxLength, got {other:?}"),
        }
    }

    #[test]
    fn wrong_type_still_reports_invalid_type() {
        let types = field_types("        project: { type: string, max_length: 5 }\n");
        let doc = doc_with_frontmatter(json!({"project": 12345}));
        let findings = check_field_types(&doc, &types, None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "field-type-invalid");
    }

    #[test]
    fn within_bound_string_produces_no_finding() {
        let types = field_types("        project: { type: string, max_length: 5 }\n");
        let doc = doc_with_frontmatter(json!({"project": "ok"}));
        let findings = check_field_types(&doc, &types, None);
        assert!(findings.is_empty());
    }

    #[test]
    fn over_length_list_of_strings_element_reports_exceeds_max_length() {
        let types = field_types("        tags: { type: list_of_strings, max_length: 3 }\n");
        let doc = doc_with_frontmatter(json!({"tags": ["ok", "toolong"]}));
        let findings = check_field_types(&doc, &types, None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "frontmatter-exceeds-max-length");
        match &findings[0].body {
            FindingBody::ExceedsMaxLength { actual_length, .. } => assert_eq!(*actual_length, 7),
            other => panic!("expected ExceedsMaxLength, got {other:?}"),
        }
    }

    #[test]
    fn type_less_indexed_only_entry_contributes_no_type_finding() {
        let types = field_types("        status: { indexed: false }\n");
        let doc = doc_with_frontmatter(json!({"status": 12345}));
        let findings = check_field_types(&doc, &types, None);
        assert!(findings.is_empty());
    }

    fn doc_with_path(path: &str) -> Document {
        Document {
            path: Utf8PathBuf::from(path),
            stem: camino::Utf8Path::new(path)
                .file_stem()
                .unwrap_or("")
                .to_string(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    #[test]
    fn portable_filename_clean_path_is_silent() {
        let doc = doc_with_path("notes/Wide Open Spaces.md");
        assert!(check_portable_filename(&doc).is_none());
    }

    #[test]
    fn portable_filename_flags_each_illegal_character_class() {
        for ch in [':', '*', '?', '"', '<', '>', '|', '\\'] {
            let path = format!("notes/bad{ch}name.md");
            let doc = doc_with_path(&path);
            let finding = check_portable_filename(&doc)
                .unwrap_or_else(|| panic!("expected a finding for illegal char {ch:?}"));
            assert_eq!(finding.code, "nonportable-filename");
            match &finding.body {
                FindingBody::NonportableFilename { issues } => {
                    assert_eq!(issues.len(), 1);
                    let expected_segment = format!("bad{ch}name.md");
                    assert!(
                        issues[0].contains(&expected_segment),
                        "issue should name the offending segment '{expected_segment}': {}",
                        issues[0]
                    );
                    assert!(issues[0].contains(&ch.to_string()));
                }
                other => panic!("expected NonportableFilename, got {other:?}"),
            }
        }
    }

    #[test]
    fn portable_filename_flags_leading_space_segment() {
        let doc = doc_with_path(" leading-space/note.md");
        let finding = check_portable_filename(&doc).expect("expected a finding");
        match &finding.body {
            FindingBody::NonportableFilename { issues } => {
                assert_eq!(issues.len(), 1);
                assert!(issues[0].contains("leading space"));
                assert!(issues[0].contains(" leading-space"));
            }
            other => panic!("expected NonportableFilename, got {other:?}"),
        }
    }

    #[test]
    fn portable_filename_flags_trailing_space_segment() {
        let doc = doc_with_path("notes/trailing-space.md ");
        let finding = check_portable_filename(&doc).expect("expected a finding");
        match &finding.body {
            FindingBody::NonportableFilename { issues } => {
                assert_eq!(issues.len(), 1);
                assert!(issues[0].contains("trailing space"));
            }
            other => panic!("expected NonportableFilename, got {other:?}"),
        }
    }

    #[test]
    fn portable_filename_flags_trailing_dot_segment() {
        let doc = doc_with_path("notes/trailing-dot./note.md");
        let finding = check_portable_filename(&doc).expect("expected a finding");
        match &finding.body {
            FindingBody::NonportableFilename { issues } => {
                assert_eq!(issues.len(), 1);
                assert!(issues[0].contains("trailing dot"));
                assert!(issues[0].contains("trailing-dot."));
            }
            other => panic!("expected NonportableFilename, got {other:?}"),
        }
    }

    #[test]
    fn portable_filename_multiple_violations_collapse_into_one_finding() {
        // A single segment with two illegal chars, plus a second segment with a
        // trailing dot: three total issues, ONE finding.
        let doc = doc_with_path("weird:name?.dir/trailing-dot./note.md");
        let finding = check_portable_filename(&doc).expect("expected a finding");
        assert_eq!(finding.code, "nonportable-filename");
        match &finding.body {
            FindingBody::NonportableFilename { issues } => {
                assert_eq!(issues.len(), 3, "expected 3 issues, got: {issues:?}");
            }
            other => panic!("expected NonportableFilename, got {other:?}"),
        }
    }

    #[test]
    fn portable_filename_does_not_flag_dot_segments() {
        // `.`/`..` segments are not real filenames; the (synthetic) path below
        // exercises the skip without asserting on real path normalization.
        let doc = doc_with_path("notes/./note.md");
        assert!(check_portable_filename(&doc).is_none());
    }
}
