use vault_core::{Document, DocumentSummary, GraphIndex};
use vault_graph::pattern_matches_path;

use crate::config::{ValidateConfig, ValidateRule};
use crate::findings::Finding;
use crate::predicates::frontmatter_predicates_match;

pub fn validate(index: &GraphIndex, config: &ValidateConfig) -> Vec<Finding> {
    validate_with_alias_field(index, config, None)
}

pub fn validate_with_alias_field(
    index: &GraphIndex,
    config: &ValidateConfig,
    alias_field: Option<&str>,
) -> Vec<Finding> {
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
        findings.extend(crate::checks::check_alias_malformed(document, alias_field));
    }

    // Cross-doc alias checks (after per-doc loop).
    if alias_field.is_some() {
        let non_ignored: Vec<&Document> = index
            .documents
            .iter()
            .filter(|d| !document_ignored(d, config))
            .collect();
        findings.extend(crate::checks::check_alias_shadowed_by_stem(
            &non_ignored,
            alias_field,
        ));
        findings.extend(crate::checks::check_alias_duplicate_across_docs(
            &non_ignored,
            alias_field,
        ));
    }

    findings
}

/// Apply a single validate rule against a pre-narrowed scope of document
/// summaries. The rule's `match` predicates are assumed already applied
/// (the caller narrowed the scope via SQL). Only the constraint checks
/// (required / forbidden / allowed_values / field_types / allowed_paths)
/// run.
///
/// Internally lifts each `DocumentSummary` to a `Document` with empty
/// joined tables so the existing per-doc check helpers can be reused
/// without signature changes.
pub fn validate_rule(rule: &ValidateRule, scope: &[DocumentSummary]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for summary in scope {
        let doc = summary_to_document(summary);

        findings.extend(crate::checks::check_required_frontmatter(
            &doc,
            &rule.required_frontmatter,
            rule.name.as_deref(),
        ));

        findings.extend(crate::checks::check_field_types(
            &doc,
            &rule.field_types,
            rule.name.as_deref(),
        ));

        findings.extend(crate::checks::check_forbidden_frontmatter(
            &doc,
            &rule.forbidden_frontmatter,
            rule.name.as_deref(),
        ));

        if let Some(finding) =
            crate::checks::check_allowed_paths(&doc, &rule.allowed_paths, rule.name.as_deref())
        {
            findings.push(finding);
        }

        findings.extend(crate::checks::check_allowed_values(
            &doc,
            &rule.allowed_values,
            rule.name.as_deref(),
        ));
    }
    findings
}

fn summary_to_document(summary: &DocumentSummary) -> Document {
    Document {
        path: summary.path.clone(),
        stem: summary.stem.clone(),
        hash: summary.hash.clone(),
        frontmatter: summary.frontmatter.clone(),
        body_text: summary.body_text.clone(),
        headings: Vec::new(),
        block_ids: Vec::new(),
        links: Vec::new(),
        diagnostics: Vec::new(),
        aliases: vec![],
        alias_malformed: vec![],
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RuleExclude, RuleSelector, ValidateConfig, ValidateRule};
    use serde_json::json;
    use vault_core::{Document, GraphIndex};

    fn empty_rule(name: &str) -> ValidateRule {
        ValidateRule {
            name: Some(name.into()),
            r#match: RuleSelector {
                path: None,
                path_not: None,
                frontmatter: std::collections::HashMap::new(),
            },
            exclude: RuleExclude { path: None },
            required_frontmatter: vec![],
            forbidden_frontmatter: vec![],
            field_types: std::collections::HashMap::new(),
            allowed_values: std::collections::HashMap::new(),
            allowed_paths: vec![],
        }
    }

    fn document(path: &str, frontmatter: Option<serde_json::Value>) -> Document {
        Document {
            path: path.into(),
            stem: camino::Utf8Path::new(path).file_stem().unwrap().to_string(),
            hash: String::new(),
            frontmatter,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    fn index_with(documents: Vec<Document>) -> GraphIndex {
        GraphIndex {
            root: "/vault".into(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    #[test]
    fn validate_with_no_config_emits_no_findings_on_clean_document() {
        let index = index_with(vec![document("a.md", Some(json!({"title": "hi"})))]);
        let config = ValidateConfig {
            ignore: vec![],
            required_frontmatter: vec![],
            rules: vec![],
        };
        let findings = validate(&index, &config);
        assert!(findings.is_empty());
    }

    #[test]
    fn validate_emits_required_frontmatter_findings() {
        let index = index_with(vec![document("a.md", Some(json!({})))]);
        let config = ValidateConfig {
            ignore: vec![],
            required_frontmatter: vec!["title".into()],
            rules: vec![],
        };
        let findings = validate(&index, &config);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].code, "frontmatter-required-field-missing");
    }

    #[test]
    fn document_ignored_skips_findings() {
        let index = index_with(vec![document("Archive/old.md", Some(json!({})))]);
        let config = ValidateConfig {
            ignore: vec!["Archive/**".into()],
            required_frontmatter: vec!["title".into()],
            rules: vec![],
        };
        let findings = validate(&index, &config);
        assert!(findings.is_empty());
    }

    #[test]
    fn validate_emits_alias_malformed_finding() {
        use serde_json::json;
        use vault_core::{Document, GraphIndex};

        let doc = Document {
            path: "a.md".into(),
            stem: "a".into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["valid".into()],
            alias_malformed: vec![json!({"nested": "x"})],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc],
        };
        let findings =
            validate_with_alias_field(&index, &ValidateConfig::default(), Some("aliases"));
        let malformed_count = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-malformed")
            .count();
        assert_eq!(malformed_count, 1);
    }

    #[test]
    fn validate_does_not_emit_alias_findings_when_field_unconfigured() {
        use serde_json::json;
        use vault_core::{Document, GraphIndex};

        let doc = Document {
            path: "a.md".into(),
            stem: "a".into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![json!({"nested": "x"})],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc],
        };
        let findings = validate_with_alias_field(&index, &ValidateConfig::default(), None);
        let malformed_count = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-malformed")
            .count();
        assert_eq!(malformed_count, 0);
    }

    #[test]
    fn validate_emits_alias_shadowed_by_stem_finding() {
        use vault_core::{Document, GraphIndex};

        // doc-a.md has stem "foo"
        // doc-b.md has aliases: ["foo"] — shadowed by doc-a's stem.
        let doc_a = Document {
            path: "doc-a.md".into(),
            stem: "foo".into(),
            hash: "h1".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        };
        let doc_b = Document {
            path: "doc-b.md".into(),
            stem: "doc-b".into(),
            hash: "h2".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["foo".into()],
            alias_malformed: vec![],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc_a, doc_b],
        };
        let findings =
            validate_with_alias_field(&index, &ValidateConfig::default(), Some("aliases"));
        let shadow: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-shadowed-by-stem")
            .collect();
        assert_eq!(shadow.len(), 1);
        assert_eq!(shadow[0].path, "doc-b.md");
    }

    #[test]
    fn validate_emits_self_stem_shadow_finding() {
        use vault_core::{Document, GraphIndex};

        let doc = Document {
            path: "self.md".into(),
            stem: "self".into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["self".into()],
            alias_malformed: vec![],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc],
        };
        let findings =
            validate_with_alias_field(&index, &ValidateConfig::default(), Some("aliases"));
        let shadow: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-shadowed-by-stem")
            .collect();
        assert_eq!(shadow.len(), 1);
        assert!(
            shadow[0].message.contains("this doc's own stem"),
            "expected self-stem message; got: {}",
            shadow[0].message
        );
    }

    #[test]
    fn validate_does_not_emit_shadow_when_alias_field_none() {
        use vault_core::{Document, GraphIndex};

        let doc_a = Document {
            path: "doc-a.md".into(),
            stem: "foo".into(),
            hash: "h1".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        };
        let doc_b = Document {
            path: "doc-b.md".into(),
            stem: "doc-b".into(),
            hash: "h2".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["foo".into()],
            alias_malformed: vec![],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc_a, doc_b],
        };
        let findings = validate_with_alias_field(&index, &ValidateConfig::default(), None);
        let shadow: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-shadowed-by-stem")
            .collect();
        assert!(shadow.is_empty());
    }

    #[test]
    fn validate_emits_alias_duplicate_finding_for_each_participant() {
        use vault_core::{Document, GraphIndex};

        let doc_a = Document {
            path: "a.md".into(),
            stem: "a".into(),
            hash: "h1".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["vault memory".into()],
            alias_malformed: vec![],
        };
        let doc_b = Document {
            path: "b.md".into(),
            stem: "b".into(),
            hash: "h2".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["vault memory".into()],
            alias_malformed: vec![],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc_a, doc_b],
        };
        let findings =
            validate_with_alias_field(&index, &ValidateConfig::default(), Some("aliases"));
        let dupes: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-duplicate-across-docs")
            .collect();
        assert_eq!(dupes.len(), 2, "both docs should get the finding");
        let paths: Vec<_> = dupes.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"a.md"));
        assert!(paths.contains(&"b.md"));
    }

    #[test]
    fn validate_does_not_emit_duplicate_when_only_one_doc_claims_alias() {
        use vault_core::{Document, GraphIndex};

        let doc = Document {
            path: "a.md".into(),
            stem: "a".into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["vault memory".into()],
            alias_malformed: vec![],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc],
        };
        let findings =
            validate_with_alias_field(&index, &ValidateConfig::default(), Some("aliases"));
        let dupes: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-duplicate-across-docs")
            .collect();
        assert!(dupes.is_empty());
    }

    #[test]
    fn scoped_rule_fires_only_on_matching_path() {
        let mut rule = empty_rule("workspace-notes");
        rule.r#match.path = Some("Workspaces/**/notes/*.md".into());
        rule.required_frontmatter = vec!["kind".into()];

        let index = index_with(vec![
            document("Workspaces/foo/notes/a.md", Some(json!({}))),
            document("README.md", Some(json!({}))),
        ]);
        let config = ValidateConfig {
            ignore: vec![],
            required_frontmatter: vec![],
            rules: vec![rule],
        };
        let findings = validate(&index, &config);
        // Only the Workspaces/foo/notes/a.md document should fire the rule.
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path, "Workspaces/foo/notes/a.md");
    }

    #[test]
    fn validate_does_not_emit_duplicate_when_single_doc_has_repeated_alias() {
        use vault_core::{Document, GraphIndex};

        // A doc with the same alias listed twice — weird but legal frontmatter.
        // The duplicate-across-docs check must NOT fire (only one real doc claims it).
        let doc = Document {
            path: "a.md".into(),
            stem: "a".into(),
            hash: "h".into(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec!["foo".into(), "foo".into()],
            alias_malformed: vec![],
        };
        let index = GraphIndex {
            root: ".".into(),
            files: vec![],
            ignored_files: vec![],
            documents: vec![doc],
        };
        let findings =
            validate_with_alias_field(&index, &ValidateConfig::default(), Some("aliases"));
        let dupes: Vec<_> = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-duplicate-across-docs")
            .collect();
        assert!(
            dupes.is_empty(),
            "expected no duplicate-across-docs finding for single-doc repeated alias; got {} findings",
            dupes.len()
        );
    }
}

#[cfg(test)]
mod validate_rule_tests {
    use super::*;
    use crate::config::{RuleExclude, RuleSelector, ValidateRule};
    use serde_json::json;
    use std::collections::HashMap;
    use vault_core::DocumentSummary;

    #[test]
    fn validate_rule_applies_required_frontmatter_only_to_scope() {
        let rule = ValidateRule {
            name: Some("type-note-requires-kind".into()),
            r#match: RuleSelector {
                path: None,
                path_not: None,
                frontmatter: HashMap::new(),
            },
            exclude: RuleExclude { path: None },
            required_frontmatter: vec!["kind".into()],
            forbidden_frontmatter: vec![],
            field_types: HashMap::new(),
            allowed_values: HashMap::new(),
            allowed_paths: vec![],
        };

        let scope = vec![
            DocumentSummary {
                path: "good.md".into(),
                stem: "good".into(),
                hash: "h".into(),
                frontmatter: Some(json!({"type": "note", "kind": "log"})),
                body_text: String::new(),
            },
            DocumentSummary {
                path: "bad.md".into(),
                stem: "bad".into(),
                hash: "h".into(),
                frontmatter: Some(json!({"type": "note"})),
                body_text: String::new(),
            },
        ];

        let findings = validate_rule(&rule, &scope);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].path.as_str(), "bad.md");
    }
}
