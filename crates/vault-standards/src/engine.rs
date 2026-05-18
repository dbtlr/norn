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
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
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
}
