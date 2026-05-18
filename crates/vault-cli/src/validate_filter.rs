use std::collections::BTreeSet;

use anyhow::{bail, Result};
use vault_core::display;
use vault_graph::pattern_matches_path;
use vault_standards::{Finding, FindingBody};

use crate::cli::{RepairPlanArgs, ValidateArgs};

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
            codes: &args.code,
            severities: &args.severity,
            fields: &args.field,
            rules: &args.rule,
            paths: &args.path,
            targets: &args.target,
            reasons: &args.reason,
        }
    }
}

impl<'a> From<&'a RepairPlanArgs> for ValidateFilterOptions<'a> {
    fn from(args: &'a RepairPlanArgs) -> Self {
        Self {
            codes: &args.code,
            severities: &args.severity,
            fields: &args.field,
            rules: &args.rule,
            paths: &args.path,
            targets: &args.target,
            reasons: &args.reason,
        }
    }
}

#[derive(Debug)]
struct ParsedValidateFilters {
    codes: BTreeSet<String>,
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
    set_matches(&filters.codes, &finding.code)
        && set_matches(&filters.severities, severity_key(finding))
        && paths_match(finding, &filters.paths)
        && optional_set_matches(&filters.fields, finding_field(finding))
        && optional_set_matches(&filters.rules, finding_rule(finding))
        && optional_set_matches(&filters.targets, finding_target(finding))
        && optional_set_matches(&filters.reasons, finding_reason(finding))
}

impl ParsedValidateFilters {
    fn parse(options: &ValidateFilterOptions<'_>) -> Result<Self> {
        Ok(Self {
            codes: parse_values(options.codes, "code")?,
            severities: parse_values(options.severities, "severity")?,
            fields: parse_values(options.fields, "field")?,
            rules: parse_values(options.rules, "rule")?,
            paths: parse_path_values(options.paths)?,
            targets: parse_values(options.targets, "target")?,
            reasons: parse_values(options.reasons, "reason")?,
        })
    }
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
        || patterns
            .iter()
            .any(|pattern| pattern_matches_path(pattern, &finding.path))
}

fn severity_key(finding: &Finding) -> &'static str {
    match finding.severity {
        vault_core::Severity::Warning => "warning",
        vault_core::Severity::Error => "error",
    }
}

fn finding_field(finding: &Finding) -> Option<&str> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { field, .. }
        | FindingBody::DisallowedValue { field, .. }
        | FindingBody::InvalidFieldType { field, .. }
        | FindingBody::ForbiddenField { field, .. } => Some(field),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::DocumentMisrouted { .. } => None,
    }
}

fn finding_rule(finding: &Finding) -> Option<&str> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { rule, .. }
        | FindingBody::DisallowedValue { rule, .. }
        | FindingBody::InvalidFieldType { rule, .. }
        | FindingBody::ForbiddenField { rule, .. }
        | FindingBody::DocumentMisrouted { rule, .. } => rule.as_deref(),
        FindingBody::GraphDiagnostic { .. } | FindingBody::LinkIssue { .. } => None,
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
