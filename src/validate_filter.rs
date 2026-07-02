use std::collections::BTreeSet;

use crate::core::display;
use crate::standards::path_match::PathPattern;
use crate::standards::{Finding, FindingBody};
use anyhow::{bail, Result};

use crate::cli::{RepairArgs, ValidateArgs};

#[derive(Debug)]
enum CodeMatcher {
    Exact(String),
    Glob(globset::GlobMatcher),
}

impl CodeMatcher {
    fn parse(value: &str) -> Result<Self> {
        if value.contains(['*', '?', '[']) {
            let glob = globset::Glob::new(value)
                .map_err(|e| anyhow::anyhow!("invalid code glob '{value}': {e}"))?;
            Ok(Self::Glob(glob.compile_matcher()))
        } else {
            Ok(Self::Exact(value.to_string()))
        }
    }

    fn matches(&self, code: &str) -> bool {
        match self {
            Self::Exact(s) => s == code,
            Self::Glob(g) => g.is_match(code),
        }
    }
}

#[derive(Debug)]
pub struct ValidateFilterOptions<'a> {
    pub codes: &'a [String],
    pub severities: &'a [String],
    pub fields: &'a [String],
    pub rules: &'a [String],
    pub paths: &'a [String],
    pub targets: &'a [String],
    pub reasons: &'a [String],
}

impl<'a> From<&'a ValidateArgs> for ValidateFilterOptions<'a> {
    fn from(args: &'a ValidateArgs) -> Self {
        Self {
            codes: &args.triage.code,
            severities: &args.triage.severity,
            fields: &args.triage.field,
            rules: &args.triage.rule,
            paths: &args.triage.path,
            targets: &args.triage.target,
            reasons: &args.triage.reason,
        }
    }
}

impl<'a> From<&'a RepairArgs> for ValidateFilterOptions<'a> {
    fn from(args: &'a RepairArgs) -> Self {
        Self {
            codes: &args.triage.code,
            severities: &args.triage.severity,
            fields: &args.triage.field,
            rules: &args.triage.rule,
            paths: &args.triage.path,
            targets: &args.triage.target,
            reasons: &args.triage.reason,
        }
    }
}

#[derive(Debug)]
struct ParsedValidateFilters {
    codes: Vec<CodeMatcher>,
    severities: BTreeSet<String>,
    fields: BTreeSet<String>,
    rules: BTreeSet<String>,
    paths: Vec<String>,
    targets: BTreeSet<String>,
    reasons: BTreeSet<String>,
}

pub fn filter_findings(
    findings: Vec<Finding>,
    options: &ValidateFilterOptions<'_>,
) -> Result<Vec<Finding>> {
    let filters = ParsedValidateFilters::parse(options)?;
    Ok(findings
        .into_iter()
        .filter(|finding| finding_matches(finding, &filters))
        .collect())
}

fn finding_matches(finding: &Finding, filters: &ParsedValidateFilters) -> bool {
    code_matches(&filters.codes, &finding.code)
        && set_matches(&filters.severities, severity_key(finding))
        && paths_match(finding, &filters.paths)
        && optional_set_matches(&filters.fields, finding_field(finding))
        && optional_set_matches(&filters.rules, finding_rule(finding))
        && optional_set_matches(&filters.targets, finding_target(finding))
        && optional_set_matches(&filters.reasons, finding_reason(finding))
}

fn code_matches(matchers: &[CodeMatcher], code: &str) -> bool {
    matchers.is_empty() || matchers.iter().any(|m| m.matches(code))
}

impl ParsedValidateFilters {
    fn parse(options: &ValidateFilterOptions<'_>) -> Result<Self> {
        Ok(Self {
            codes: parse_code_matchers(options.codes)?,
            severities: parse_values(options.severities, "severity")?,
            fields: parse_values(options.fields, "field")?,
            rules: parse_values(options.rules, "rule")?,
            paths: parse_path_values(options.paths)?,
            targets: parse_values(options.targets, "target")?,
            reasons: parse_values(options.reasons, "reason")?,
        })
    }
}

fn parse_code_matchers(values: &[String]) -> Result<Vec<CodeMatcher>> {
    let mut parsed = Vec::new();
    for value in values {
        for item in value.split(',').map(str::trim) {
            if item.is_empty() {
                bail!("invalid code filter, expected non-empty comma-separated values");
            }
            parsed.push(CodeMatcher::parse(item)?);
        }
    }
    Ok(parsed)
}

fn parse_values(values: &[String], label: &str) -> Result<BTreeSet<String>> {
    let mut parsed = BTreeSet::new();
    for value in values {
        for item in value.split(',').map(str::trim) {
            if item.is_empty() {
                bail!("invalid {label} filter, expected non-empty comma-separated values");
            }
            parsed.insert(item.to_string());
        }
    }
    Ok(parsed)
}

fn parse_path_values(values: &[String]) -> Result<Vec<String>> {
    let mut parsed = Vec::new();
    for value in values {
        for item in value.split(',').map(str::trim) {
            if item.is_empty() {
                bail!("invalid path filter, expected non-empty comma-separated values");
            }
            parsed.push(item.to_string());
        }
    }
    Ok(parsed)
}

fn set_matches(values: &BTreeSet<String>, actual: &str) -> bool {
    values.is_empty() || values.contains(actual)
}

fn optional_set_matches(values: &BTreeSet<String>, actual: Option<&str>) -> bool {
    values.is_empty() || actual.is_some_and(|actual| values.contains(actual))
}

fn paths_match(finding: &Finding, patterns: &[String]) -> bool {
    patterns.is_empty()
        || patterns.iter().any(|pattern| {
            PathPattern::parse(pattern)
                .ok()
                .and_then(|p| p.match_path(finding.path.as_str()))
                .is_some()
        })
}

fn severity_key(finding: &Finding) -> &'static str {
    match finding.severity {
        crate::core::Severity::Warning => "warning",
        crate::core::Severity::Error => "error",
    }
}

fn finding_field(finding: &Finding) -> Option<&str> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { field, .. }
        | FindingBody::DisallowedValue { field, .. }
        | FindingBody::InvalidFieldType { field, .. }
        | FindingBody::ForbiddenField { field, .. }
        | FindingBody::AliasMalformed { field, .. }
        | FindingBody::ReferenceType { field, .. } => Some(field),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::DocumentMisrouted { .. }
        | FindingBody::AliasShadowedByStem { .. }
        | FindingBody::AliasDuplicateAcrossDocs { .. } => None,
    }
}

fn finding_rule(finding: &Finding) -> Option<&str> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { rule, .. }
        | FindingBody::DisallowedValue { rule, .. }
        | FindingBody::InvalidFieldType { rule, .. }
        | FindingBody::ForbiddenField { rule, .. }
        | FindingBody::DocumentMisrouted { rule, .. }
        | FindingBody::ReferenceType { rule, .. } => rule.as_deref(),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::AliasMalformed { .. }
        | FindingBody::AliasShadowedByStem { .. }
        | FindingBody::AliasDuplicateAcrossDocs { .. } => None,
    }
}

fn finding_target(finding: &Finding) -> Option<&str> {
    match &finding.body {
        FindingBody::LinkIssue { link } => Some(&link.target),
        _ => None,
    }
}

fn finding_reason(finding: &Finding) -> Option<&'static str> {
    let reason = match &finding.body {
        FindingBody::LinkIssue { link } => link.unresolved_reason.as_ref()?,
        _ => return None,
    };

    Some(display::unresolved_reason_str(reason))
}

#[cfg(test)]
mod glob_match_tests {
    use super::*;
    use crate::core::{Link, LinkKind, LinkStatus, Severity, UnresolvedReason};
    use crate::standards::{Finding, FindingBody};

    fn link_finding(code: &str) -> Finding {
        Finding {
            code: code.to_string(),
            severity: Severity::Warning,
            path: "doc.md".into(),
            message: String::new(),
            body: FindingBody::LinkIssue {
                link: Link {
                    source_path: "doc.md".into(),
                    raw: "[[x]]".into(),
                    kind: LinkKind::Wikilink,
                    target: "x".into(),
                    label: None,
                    anchor: None,
                    block_ref: None,
                    source_span: None,
                    source_context: None,
                    resolved_path: None,
                    unresolved_reason: Some(UnresolvedReason::TargetMissing),
                    candidates: vec![],
                    status: LinkStatus::Unresolved,
                },
            },
        }
    }

    #[test]
    fn glob_star_matches_link_family() {
        let codes = vec!["link-*".to_string()];
        let opts = ValidateFilterOptions {
            codes: &codes,
            severities: &[],
            fields: &[],
            rules: &[],
            paths: &[],
            targets: &[],
            reasons: &[],
        };
        let findings = vec![
            link_finding("link-target-missing"),
            link_finding("link-anchor-missing"),
            link_finding("link-block-missing"),
            link_finding("link-ambiguous"),
            link_finding("frontmatter-required-field-missing"),
        ];
        let out = filter_findings(findings, &opts).unwrap();
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|f| f.code.starts_with("link-")));
    }

    #[test]
    fn exact_string_match_still_works() {
        let codes = vec!["link-target-missing".to_string()];
        let opts = ValidateFilterOptions {
            codes: &codes,
            severities: &[],
            fields: &[],
            rules: &[],
            paths: &[],
            targets: &[],
            reasons: &[],
        };
        let findings = vec![
            link_finding("link-target-missing"),
            link_finding("link-anchor-missing"),
        ];
        let out = filter_findings(findings, &opts).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, "link-target-missing");
    }

    #[test]
    fn invalid_glob_returns_arg_error() {
        let codes = vec!["link-[invalid".to_string()];
        let opts = ValidateFilterOptions {
            codes: &codes,
            severities: &[],
            fields: &[],
            rules: &[],
            paths: &[],
            targets: &[],
            reasons: &[],
        };
        let result = filter_findings(vec![], &opts);
        assert!(result.is_err());
    }
}
