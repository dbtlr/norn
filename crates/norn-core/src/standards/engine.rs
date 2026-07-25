//! The validate engine: run every configured check against a built graph.
//!
//! Two public entries share one per-document body ([`document_findings`]), so
//! the whole-vault and single-document passes cannot drift:
//! [`validate_with_compiled`] walks the graph index once and returns the flat
//! [`Finding`] list for every document; [`validate_document_with_compiled`]
//! returns exactly the sub-list for ONE path, evaluating rules against that
//! document alone while still resolving reference targets against the whole
//! index. Both take a [`CompiledConfig`] so path patterns are matched
//! pre-compiled (an uncompiled per-document re-parse of every rule glob is the
//! accidental quadratic this path avoids). The `validate` / `validate_rule*`
//! convenience wrappers are a second, uncompiled way to reach the same job —
//! retained here only as `#[cfg(test)]` helpers.

use camino::Utf8Path;

use crate::domain::{Document, GraphIndex};

use crate::standards::config::{CompiledConfig, CompiledRule, ValidateConfig, ValidateRule};
use crate::standards::findings::Finding;
use crate::standards::path_match::{effective_match_glob, PathPattern};
use crate::standards::predicates::frontmatter_predicates_match;

/// Target-type lookup for `field_references` checks: a validated document's
/// `type` frontmatter, keyed by path. Ignored documents are deliberately absent
/// — their frontmatter is outside the validation contract, so references to
/// them are never judged.
type ReferenceTypes<'a> = std::collections::BTreeMap<&'a Utf8Path, Option<&'a serde_json::Value>>;

// Test-only, PER-THREAD tally of documents the engine ran its checks over. The
// whole-vault pass evaluates one per non-ignored document; the scoped pass
// evaluates exactly one. The post-create scope guard resets it, drives the
// post-create validate pass, and reads the count to prove that pass's rule
// evaluation tracks the created document, not the vault. Thread-local so the
// parallel test runner's other validate runs never pollute the measurement.
#[cfg(test)]
thread_local! {
    static DOCS_EVALUATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reset the current thread's document-evaluation tally (test-only).
#[cfg(test)]
pub(crate) fn docs_evaluated_reset() {
    DOCS_EVALUATED.with(|count| count.set(0));
}

/// Read the current thread's document-evaluation tally (test-only).
#[cfg(test)]
pub(crate) fn docs_evaluated_count() -> usize {
    DOCS_EVALUATED.with(|count| count.get())
}

/// Validate using pre-compiled path patterns — the whole-vault engine entry
/// point. Call after loading the config via [`parse_config_compiled`](crate::standards::parse_config_compiled)
/// (or [`compile_config`](crate::standards::compile_config)) so every rule glob
/// is matched pre-compiled.
pub fn validate_with_compiled(
    index: &GraphIndex,
    config: &ValidateConfig,
    compiled: &CompiledConfig,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    // Built once per run, and only when some rule declares the constraint.
    let type_by_path: ReferenceTypes<'_> = if needs_reference_types(config) {
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
        findings.extend(document_findings(document, config, compiled, &type_by_path));
    }

    findings
}

/// Validate ONE document: exactly the findings [`validate_with_compiled`]
/// returns for `path`, in the same order, for the same index — without
/// evaluating rules against any other document. Rule evaluation is
/// document-local (every check is a pure function of one document plus the
/// rule), so the only whole-graph input is the `field_references` target-type
/// lookup, which is narrowed here to the document's own frontmatter link
/// targets and still read from the full index. Link resolution itself is the
/// caller's job: the passed index must already be resolved across the whole
/// graph, since a link's resolved/unresolved/ambiguous status depends on every
/// other document.
///
/// Ordering precondition: `index.documents` is expected sorted by
/// [`Utf8Path`] order, which is COMPONENT-WISE — `Projects/a.md` sorts before
/// `Projects.md`, the reverse of their byte order. The graph walk and the apply
/// overlay both sort under exactly this comparator. An index ordered any other
/// way still answers correctly, because the lookup falls back to a scan on a
/// miss; it just pays a linear lookup, which a debug build reports as a failed
/// assertion rather than letting it pass unnoticed.
///
/// A path absent from the index, or one excluded by `validate.ignore`, yields
/// no findings — the same answer the whole-vault pass gives for it.
pub fn validate_document_with_compiled(
    index: &GraphIndex,
    config: &ValidateConfig,
    compiled: &CompiledConfig,
    path: &Utf8Path,
) -> Vec<Finding> {
    // Checked once per call, not per lookup: the precondition belongs to the
    // entry point, and an O(documents) check inside the lookup would itself
    // scale with the vault.
    debug_assert!(
        index.documents.windows(2).all(|w| w[0].path <= w[1].path),
        "index documents are expected sorted under path-component order; \
         another order still answers correctly but costs a linear lookup"
    );
    let Some(document) = lookup_document(index, path) else {
        return Vec::new();
    };
    if document_ignored_compiled(document, compiled, &config.ignore) {
        return Vec::new();
    }
    let type_by_path = reference_types_for_targets(index, document, config, compiled);
    document_findings(document, config, compiled, &type_by_path)
}

/// Every check the engine runs for one document, in the order the flat finding
/// list carries them. The single body behind both entry points.
fn document_findings(
    document: &Document,
    config: &ValidateConfig,
    compiled: &CompiledConfig,
    type_by_path: &ReferenceTypes<'_>,
) -> Vec<Finding> {
    #[cfg(test)]
    DOCS_EVALUATED.with(|count| count.set(count.get() + 1));

    let mut findings = Vec::new();

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
            type_by_path,
            rule.name.as_deref(),
        ));
    }

    findings.extend(crate::standards::checks::check_links(document));
    if let Some(finding) = crate::standards::checks::check_portable_filename(document) {
        findings.push(finding);
    }

    findings
}

/// Does any rule declare a `field_references` constraint? Gates building the
/// target-type lookup at all — the same condition on both entry points, so a
/// config without the constraint pays nothing on either.
fn needs_reference_types(config: &ValidateConfig) -> bool {
    config
        .rules
        .iter()
        .any(|rule| !rule.field_references.is_empty())
}

/// The [`ReferenceTypes`] entries one document's `field_references` checks can
/// possibly consult: its own RESOLVED frontmatter link targets, read from the
/// full index. A superset of what any single rule reads (each rule looks only
/// at its own constrained fields) and a subset of the whole-vault map's
/// entries, so every lookup answers identically — including the deliberate
/// misses for ignored targets, which stay absent here too.
fn reference_types_for_targets<'a>(
    index: &'a GraphIndex,
    document: &Document,
    config: &ValidateConfig,
    compiled: &CompiledConfig,
) -> ReferenceTypes<'a> {
    if !needs_reference_types(config) {
        return std::collections::BTreeMap::new();
    }
    document
        .links
        .iter()
        .filter(|link| link.status == crate::domain::LinkStatus::Resolved)
        .filter(|link| {
            link.source_context
                .as_ref()
                .is_some_and(|ctx| matches!(ctx.area, crate::domain::LinkSourceArea::Frontmatter))
        })
        .filter_map(|link| lookup_document(index, link.resolved_path.as_deref()?))
        .filter(|target| !document_ignored_compiled(target, compiled, &config.ignore))
        .map(|target| {
            let ty = target.frontmatter.as_ref().and_then(|fm| fm.get("type"));
            (target.path.as_path(), ty)
        })
        .collect()
}

/// Look one path up in the index. The graph walk and the apply overlay both
/// sort `documents` under [`Utf8Path`]'s own ordering, so the lookup
/// binary-searches rather than scanning — a scoped validate must not pay for
/// the vault's size.
///
/// The miss then re-checks with a scan, because that ordering is COMPONENT-WISE
/// while a byte-ordered sequence of the same paths disagrees with it: `.`
/// (0x2E) sorts below `/` (0x2F), so bytes put `Projects.md` before
/// `Projects/a.md` where the comparator puts it after. A search over paths
/// ordered the other way can miss a document that is present, and a validate
/// that silently reports nothing for a document it holds is worse than a slow
/// one. The scan runs only on a miss, so a sorted index — every index this
/// engine is handed today — pays nothing for it.
fn lookup_document<'a>(index: &'a GraphIndex, path: &Utf8Path) -> Option<&'a Document> {
    index
        .documents
        .binary_search_by(|doc| doc.path.as_path().cmp(path))
        .ok()
        .map(|position| &index.documents[position])
        .or_else(|| index.documents.iter().find(|doc| doc.path == path))
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
// `validate` (default-compiled whole-index run) and `validate_rule` /
// `validate_rule_compiled` (single-rule runs over a pre-narrowed
// `DocumentSummary` scope) reach the same job the compiled engine does, a second
// uncompiled way. They exist only to drive the tests, so they are gated to test
// builds.

#[cfg(test)]
fn validate(index: &GraphIndex, config: &ValidateConfig) -> Vec<Finding> {
    validate_with_compiled(index, config, &CompiledConfig::default())
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
        head_text: String::new(),
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
            head_text: String::new(),
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
