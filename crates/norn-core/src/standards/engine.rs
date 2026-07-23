//! The validate engine: run every configured check against a built graph.
//!
//! One blessed public entry — [`validate_with_compiled`] — walks the graph
//! index once, applies the graph/link/frontmatter checks per document plus the
//! cross-doc alias checks, and returns the flat [`Finding`] list. It takes a
//! [`CompiledConfig`] so path patterns are matched pre-compiled (an uncompiled
//! per-document re-parse of every rule glob is the accidental quadratic this
//! path avoids). The `validate` / `validate_with_alias_field` / `validate_rule*`
//! convenience wrappers are a second, uncompiled way to reach the same job —
//! retained here only as `#[cfg(test)]` helpers.

use crate::domain::{Document, GraphIndex};

use crate::standards::config::{CompiledConfig, CompiledRule, ValidateConfig, ValidateRule};
use crate::standards::findings::Finding;
use crate::standards::path_match::{effective_match_glob, PathPattern};
use crate::standards::predicates::frontmatter_predicates_match;

/// Validate using pre-compiled path patterns — the single engine entry point.
/// Call after loading the config via [`parse_config_compiled`](crate::standards::parse_config_compiled)
/// (or [`compile_config`](crate::standards::compile_config)) so every rule glob
/// is matched pre-compiled.
pub fn validate_with_compiled(
    index: &GraphIndex,
    config: &ValidateConfig,
    compiled: &CompiledConfig,
    alias_field: Option<&str>,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Target-type lookup for `field_references` checks: each validated
    // document's `type` frontmatter, keyed by path. Built once per run, and
    // only when some rule declares the constraint. Ignored documents are
    // deliberately absent — their frontmatter is outside the validation
    // contract, so references to them are never judged.
    let needs_reference_types = config
        .rules
        .iter()
        .any(|rule| !rule.field_references.is_empty());
    let type_by_path: std::collections::BTreeMap<&camino::Utf8Path, Option<&serde_json::Value>> =
        if needs_reference_types {
            index
                .documents
                .iter()
                .filter(|doc| !document_ignored_compiled(doc, compiled, &config.ignore))
                .map(|doc| {
                    let ty = doc.frontmatter.as_ref().and_then(|fm| fm.get("type"));
                    (doc.path.as_path(), ty)
                })
                .collect()
        } else {
            std::collections::BTreeMap::new()
        };

    for document in &index.documents {
        if document_ignored_compiled(document, compiled, &config.ignore) {
            continue;
        }

        findings.extend(crate::standards::checks::check_graph_diagnostics(document));

        findings.extend(crate::standards::checks::check_required_frontmatter(
            document,
            &config.required_frontmatter,
            None,
        ));

        for (rule, compiled_rule) in matching_rules_compiled(document, &config.rules, compiled) {
            findings.extend(crate::standards::checks::check_required_frontmatter(
                document,
                &rule.required_frontmatter,
                rule.name.as_deref(),
            ));

            findings.extend(crate::standards::checks::check_field_types(
                document,
                &rule.field_types,
                rule.name.as_deref(),
            ));

            findings.extend(crate::standards::checks::check_forbidden_frontmatter(
                document,
                &rule.forbidden_frontmatter,
                rule.name.as_deref(),
            ));

            if let Some(finding) = crate::standards::checks::check_allowed_paths_compiled(
                document,
                &compiled_rule.allowed_paths,
                &rule.allowed_paths,
                rule.name.as_deref(),
            ) {
                findings.push(finding);
            }

            findings.extend(crate::standards::checks::check_allowed_values(
                document,
                &rule.allowed_values,
                rule.name.as_deref(),
            ));

            findings.extend(crate::standards::checks::check_field_references(
                document,
                &rule.field_references,
                &type_by_path,
                rule.name.as_deref(),
            ));
        }

        findings.extend(crate::standards::checks::check_links(document));
        if let Some(finding) = crate::standards::checks::check_portable_filename(document) {
            findings.push(finding);
        }
        findings.extend(crate::standards::checks::check_alias_malformed(
            document,
            alias_field,
        ));
    }

    // Cross-doc alias checks (after per-doc loop).
    if alias_field.is_some() {
        let non_ignored: Vec<&Document> = index
            .documents
            .iter()
            .filter(|d| !document_ignored_compiled(d, compiled, &config.ignore))
            .collect();
        findings.extend(crate::standards::checks::check_alias_shadowed_by_stem(
            &non_ignored,
            alias_field,
        ));
        findings.extend(crate::standards::checks::check_alias_duplicate_across_docs(
            &non_ignored,
            alias_field,
        ));
    }

    findings
}

fn document_ignored_compiled(
    document: &Document,
    compiled: &CompiledConfig,
    fallback_patterns: &[String],
) -> bool {
    if !compiled.validate_ignore.is_empty() {
        compiled
            .validate_ignore
            .iter()
            .any(|p| p.match_path(document.path.as_str()).is_some())
    } else {
        fallback_patterns.iter().any(|pattern| {
            PathPattern::parse(pattern)
                .map(|p| p.match_path(document.path.as_str()).is_some())
                .unwrap_or(false)
        })
    }
}

fn matching_rules_compiled<'a>(
    document: &Document,
    rules: &'a [ValidateRule],
    compiled: &'a CompiledConfig,
) -> Vec<(&'a ValidateRule, &'a CompiledRule)> {
    if compiled.rules.is_empty() {
        // No compiled rules — fall back to uncompiled matching. In production
        // the owner always compiles the config, so this branch is reached only
        // by the `#[cfg(test)]` wrappers that pass `CompiledConfig::default()`.
        rules
            .iter()
            .filter(|rule| rule_matches(document, rule))
            .map(|rule| {
                static EMPTY: std::sync::OnceLock<CompiledRule> = std::sync::OnceLock::new();
                let empty = EMPTY.get_or_init(|| CompiledRule {
                    path: None,
                    path_not: None,
                    exclude_path: None,
                    allowed_paths: vec![],
                });
                (rule, empty)
            })
            .collect()
    } else {
        rules
            .iter()
            .zip(compiled.rules.iter())
            .filter(|(rule, compiled_rule)| rule_matches_compiled(document, rule, compiled_rule))
            .collect()
    }
}

pub(crate) fn rule_matches(document: &Document, rule: &ValidateRule) -> bool {
    // Use the effective path glob — `match.path` for conventional rules, the
    // glob derived from `target` for creatable rules — so that a creatable rule
    // does NOT match documents outside its target path hierarchy.
    if let Some(glob) = effective_match_glob(rule.r#match.path.as_deref(), rule.target.as_deref()) {
        let matches = PathPattern::parse(&glob)
            .map(|p| p.match_path(document.path.as_str()).is_some())
            .unwrap_or(false);
        if !matches {
            return false;
        }
    }
    if let Some(path_not_pattern) = &rule.r#match.path_not {
        let matches = PathPattern::parse(path_not_pattern)
            .map(|p| p.match_path(document.path.as_str()).is_some())
            .unwrap_or(false);
        if matches {
            return false;
        }
    }
    if let Some(exclude_path) = &rule.exclude.path {
        let matches = PathPattern::parse(exclude_path)
            .map(|p| p.match_path(document.path.as_str()).is_some())
            .unwrap_or(false);
        if matches {
            return false;
        }
    }
    frontmatter_predicates_match(document, &rule.r#match.frontmatter)
}

fn rule_matches_compiled(
    document: &Document,
    rule: &ValidateRule,
    compiled: &CompiledRule,
) -> bool {
    let path = document.path.as_str();
    if let Some(p) = &compiled.path {
        if p.match_path(path).is_none() {
            return false;
        }
    }
    if let Some(p) = &compiled.path_not {
        if p.match_path(path).is_some() {
            return false;
        }
    }
    if let Some(p) = &compiled.exclude_path {
        if p.match_path(path).is_some() {
            return false;
        }
    }
    frontmatter_predicates_match(document, &rule.r#match.frontmatter)
}

// ── #[cfg(test)] convenience wrappers ────────────────────────────────────────
//
// `validate` / `validate_with_alias_field` (default-compiled whole-index runs)
// and `validate_rule` / `validate_rule_compiled` (single-rule runs over a
// pre-narrowed `DocumentSummary` scope) reach the same job the compiled engine
// does, a second uncompiled way. They exist only to drive the tests, so they
// are gated to test builds.

#[cfg(test)]
fn validate(index: &GraphIndex, config: &ValidateConfig) -> Vec<Finding> {
    validate_with_alias_field(index, config, None)
}

#[cfg(test)]
fn validate_with_alias_field(
    index: &GraphIndex,
    config: &ValidateConfig,
    alias_field: Option<&str>,
) -> Vec<Finding> {
    validate_with_compiled(index, config, &CompiledConfig::default(), alias_field)
}

#[cfg(test)]
fn validate_rule(rule: &ValidateRule, scope: &[crate::domain::DocumentSummary]) -> Vec<Finding> {
    validate_rule_compiled(rule, None, scope)
}

#[cfg(test)]
fn validate_rule_compiled(
    rule: &ValidateRule,
    compiled: Option<&CompiledRule>,
    scope: &[crate::domain::DocumentSummary],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for summary in scope {
        let doc = summary_to_document(summary);

        findings.extend(crate::standards::checks::check_required_frontmatter(
            &doc,
            &rule.required_frontmatter,
            rule.name.as_deref(),
        ));

        findings.extend(crate::standards::checks::check_field_types(
            &doc,
            &rule.field_types,
            rule.name.as_deref(),
        ));

        findings.extend(crate::standards::checks::check_forbidden_frontmatter(
            &doc,
            &rule.forbidden_frontmatter,
            rule.name.as_deref(),
        ));

        let allowed_finding = match compiled {
            Some(c) => crate::standards::checks::check_allowed_paths_compiled(
                &doc,
                &c.allowed_paths,
                &rule.allowed_paths,
                rule.name.as_deref(),
            ),
            None => crate::standards::checks::check_allowed_paths(
                &doc,
                &rule.allowed_paths,
                rule.name.as_deref(),
            ),
        };
        if let Some(finding) = allowed_finding {
            findings.push(finding);
        }

        findings.extend(crate::standards::checks::check_allowed_values(
            &doc,
            &rule.allowed_values,
            rule.name.as_deref(),
        ));
    }
    findings
}

#[cfg(test)]
fn summary_to_document(summary: &crate::domain::DocumentSummary) -> Document {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Document, GraphIndex};
    use crate::standards::config::{RuleExclude, RuleSelector, ValidateConfig, ValidateRule};
    use serde_json::json;

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
            frontmatter_defaults: std::collections::HashMap::new(),
            ..Default::default()
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
    fn rule_with_list_selector_matches_any_listed_value() {
        let mut rule = empty_rule("base");
        rule.r#match
            .frontmatter
            .insert("type".into(), json!(["task", "phase"]));

        assert!(rule_matches(
            &document("a.md", Some(json!({"type": "task"}))),
            &rule
        ));
        assert!(rule_matches(
            &document("b.md", Some(json!({"type": "phase"}))),
            &rule
        ));
        assert!(!rule_matches(
            &document("c.md", Some(json!({"type": "note"}))),
            &rule
        ));
        assert!(!rule_matches(&document("d.md", Some(json!({}))), &rule));
        assert!(!rule_matches(&document("e.md", None), &rule));
    }

    #[test]
    fn list_selector_matches_bool_and_number_options() {
        let mut rule = empty_rule("levels");
        rule.r#match
            .frontmatter
            .insert("level".into(), json!([1, 2]));
        assert!(rule_matches(
            &document("a.md", Some(json!({"level": 2}))),
            &rule
        ));
        assert!(!rule_matches(
            &document("b.md", Some(json!({"level": 3}))),
            &rule
        ));

        let mut rule = empty_rule("flag");
        rule.r#match
            .frontmatter
            .insert("draft".into(), json!([true]));
        assert!(rule_matches(
            &document("c.md", Some(json!({"draft": true}))),
            &rule
        ));
        assert!(!rule_matches(
            &document("d.md", Some(json!({"draft": false}))),
            &rule
        ));
    }

    #[test]
    fn list_selector_does_not_match_array_valued_field() {
        // Any-of lists the *candidate scalar values*; it is not containment
        // over an array-valued document field.
        let mut rule = empty_rule("base");
        rule.r#match
            .frontmatter
            .insert("type".into(), json!(["task"]));
        assert!(!rule_matches(
            &document("a.md", Some(json!({"type": ["task"]}))),
            &rule
        ));
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
        let dupes = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-duplicate-across-docs")
            .count();
        assert_eq!(dupes, 0);
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
        let dupes = findings
            .iter()
            .filter(|f| f.code == "frontmatter-alias-duplicate-across-docs")
            .count();
        assert_eq!(
            dupes, 0,
            "expected no duplicate-across-docs finding for single-doc repeated alias"
        );
    }
}

#[cfg(test)]
mod validate_rule_tests {
    use super::*;
    use crate::domain::DocumentSummary;
    use crate::standards::config::{RuleExclude, RuleSelector, ValidateRule};
    use serde_json::json;
    use std::collections::HashMap;

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
            frontmatter_defaults: HashMap::new(),
            ..Default::default()
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
