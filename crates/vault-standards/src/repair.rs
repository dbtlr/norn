use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vault_core::Severity;

use crate::config::{RepairAction, RepairConfig, RepairRule, RepairRuleMatch};
use crate::findings::{Finding, FindingBody};

const REPAIR_PLAN_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepairPlanFilters {
    pub code: Vec<String>,
    pub severity: Vec<String>,
    pub field: Vec<String>,
    pub rule: Vec<String>,
    pub path: Vec<String>,
    pub target: Vec<String>,
    pub reason: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepairPlan {
    pub schema_version: u32,
    pub vault_root: Utf8PathBuf,
    pub source_filters: RepairPlanFilters,
    pub summary: RepairPlanSummary,
    pub changes: Vec<PlannedChange>,
    pub skipped_findings: Vec<RepairPlanFinding>,
    pub unsupported_findings: Vec<RepairPlanFinding>,
    pub ambiguous_findings: Vec<RepairPlanFinding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepairPlanSummary {
    pub findings: usize,
    pub planned_changes: usize,
    pub skipped_findings: usize,
    pub unsupported_findings: usize,
    pub ambiguous_findings: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlannedChange {
    pub path: Utf8PathBuf,
    pub document_hash: String,
    pub finding_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding_rule: Option<String>,
    pub repair_rule: String,
    pub operation: String,
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_old_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepairPlanFinding {
    pub path: Utf8PathBuf,
    pub code: String,
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub candidates: Vec<Utf8PathBuf>,
    pub reason: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub next_actions: Vec<String>,
}

pub fn plan_repairs(
    vault_root: Utf8PathBuf,
    filters: RepairPlanFilters,
    findings: Vec<Finding>,
    config: &RepairConfig,
    document_hashes: &BTreeMap<Utf8PathBuf, String>,
) -> RepairPlan {
    let mut changes = Vec::new();
    let mut skipped_findings = Vec::new();

    for finding in &findings {
        if let Some((rule, action)) = matching_repair_rule(finding, &config.rules) {
            match planned_change(finding, rule, action, document_hashes) {
                Some(change) => changes.push(change),
                None => skipped_findings.push(plan_finding(
                    finding,
                    "matched repair rule cannot repair this finding",
                    vec!["inspect the repair rule and rerun repair plan".to_string()],
                )),
            }
        } else {
            skipped_findings.push(skipped_finding(finding));
        }
    }
    let unsupported_findings = skipped_findings
        .iter()
        .filter(|finding| !is_ambiguous_skipped(finding))
        .cloned()
        .collect::<Vec<_>>();
    let ambiguous_findings = skipped_findings
        .iter()
        .filter(|finding| is_ambiguous_skipped(finding))
        .cloned()
        .collect::<Vec<_>>();

    RepairPlan {
        schema_version: REPAIR_PLAN_SCHEMA_VERSION,
        vault_root,
        source_filters: filters,
        summary: RepairPlanSummary {
            findings: findings.len(),
            planned_changes: changes.len(),
            skipped_findings: skipped_findings.len(),
            unsupported_findings: unsupported_findings.len(),
            ambiguous_findings: ambiguous_findings.len(),
        },
        changes,
        skipped_findings,
        unsupported_findings,
        ambiguous_findings,
    }
}

fn matching_repair_rule<'a>(
    finding: &Finding,
    rules: &'a [RepairRule],
) -> Option<(&'a RepairRule, &'a RepairAction)> {
    rules
        .iter()
        .find(|rule| repair_match_applies(finding, &rule.r#match))
        .map(|rule| (rule, &rule.action))
}

fn repair_match_applies(finding: &Finding, rule_match: &RepairRuleMatch) -> bool {
    rule_match
        .code
        .as_ref()
        .is_none_or(|code| code == &finding.code)
        && rule_match
            .rule
            .as_ref()
            .is_none_or(|rule| finding_rule(finding).as_ref() == Some(rule))
        && rule_match
            .field
            .as_ref()
            .is_none_or(|field| finding_field(finding).as_ref() == Some(field))
        && rule_match
            .actual_value
            .as_ref()
            .is_none_or(|actual_value| finding_actual_value(finding) == Some(actual_value))
}

fn planned_change(
    finding: &Finding,
    rule: &RepairRule,
    action: &RepairAction,
    document_hashes: &BTreeMap<Utf8PathBuf, String>,
) -> Option<PlannedChange> {
    let repair_rule = rule
        .name
        .clone()
        .unwrap_or_else(|| "unnamed-repair-rule".to_string());
    let document_hash = document_hashes.get(&finding.path)?.clone();
    match action {
        RepairAction::SetFrontmatter { field, value } => Some(PlannedChange {
            path: finding.path.clone(),
            document_hash: document_hash.clone(),
            finding_code: finding.code.clone(),
            finding_rule: finding_rule(finding),
            repair_rule,
            operation: "set_frontmatter".to_string(),
            field: field.clone(),
            expected_old_value: finding_actual_value(finding).cloned(),
            new_value: Some(value.clone()),
        }),
        RepairAction::RemoveFrontmatter { field } => Some(PlannedChange {
            path: finding.path.clone(),
            document_hash,
            finding_code: finding.code.clone(),
            finding_rule: finding_rule(finding),
            repair_rule,
            operation: "remove_frontmatter".to_string(),
            field: field.clone(),
            expected_old_value: finding_actual_value(finding).cloned(),
            new_value: None,
        }),
    }
}

fn skipped_finding(finding: &Finding) -> RepairPlanFinding {
    match &finding.body {
        FindingBody::LinkIssue { link } => {
            let reason = if link.status == vault_core::LinkStatus::Ambiguous {
                "ambiguous link target"
            } else {
                "link repair requires an explicit path/link decision"
            };
            let next_actions = if link.status == vault_core::LinkStatus::Ambiguous {
                vec![
                    "change the link to an explicit path".to_string(),
                    "rename one duplicate candidate".to_string(),
                    "rerun repair plan after disambiguation".to_string(),
                ]
            } else {
                vec![
                    "create the missing target or target anchor".to_string(),
                    "rewrite the link manually".to_string(),
                    "rerun validate after resolving the link".to_string(),
                ]
            };
            let mut finding = plan_finding(finding, reason, Vec::new());
            finding.candidates = link.candidates.clone();
            finding.next_actions = next_actions;
            finding
        }
        FindingBody::RequiredFrontmatterMissing { field, .. } => plan_finding(
            finding,
            "missing field has no configured deterministic default",
            vec![
                format!("add a repair rule that sets {field} when safe"),
                "fill the field manually and rerun validate".to_string(),
            ],
        ),
        FindingBody::DisallowedValue { field, .. }
        | FindingBody::InvalidFieldType { field, .. }
        | FindingBody::ForbiddenField { field, .. } => plan_finding(
            finding,
            "no configured deterministic repair rule matched",
            vec![
                format!("add a repair rule for field {field}"),
                "rerun repair plan after updating config".to_string(),
            ],
        ),
        FindingBody::DocumentMisrouted { .. } => plan_finding(
            finding,
            "path repair is planning-only in this release",
            vec![
                "review allowed_paths and current document location".to_string(),
                "move files manually or use a future path apply command".to_string(),
            ],
        ),
        FindingBody::GraphDiagnostic { .. } => plan_finding(
            finding,
            "graph diagnostic cannot be repaired deterministically",
            vec![
                "inspect the diagnostic detail".to_string(),
                "fix the document manually and rerun validate".to_string(),
            ],
        ),
    }
}

fn plan_finding(finding: &Finding, reason: &str, next_actions: Vec<String>) -> RepairPlanFinding {
    RepairPlanFinding {
        path: finding.path.clone(),
        code: finding.code.clone(),
        severity: finding.severity.clone(),
        message: finding.message.clone(),
        rule: finding_rule(finding),
        field: finding_field(finding),
        target: finding_target(finding),
        candidates: finding_candidates(finding),
        reason: reason.to_string(),
        next_actions,
    }
}

fn is_ambiguous_skipped(finding: &RepairPlanFinding) -> bool {
    finding.code == "link-ambiguous" || finding.reason.contains("ambiguous")
}

fn finding_rule(finding: &Finding) -> Option<String> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { rule, .. }
        | FindingBody::DisallowedValue { rule, .. }
        | FindingBody::InvalidFieldType { rule, .. }
        | FindingBody::ForbiddenField { rule, .. }
        | FindingBody::DocumentMisrouted { rule, .. } => rule.clone(),
        FindingBody::GraphDiagnostic { .. } | FindingBody::LinkIssue { .. } => None,
    }
}

fn finding_field(finding: &Finding) -> Option<String> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { field, .. }
        | FindingBody::DisallowedValue { field, .. }
        | FindingBody::InvalidFieldType { field, .. }
        | FindingBody::ForbiddenField { field, .. } => Some(field.clone()),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::DocumentMisrouted { .. } => None,
    }
}

fn finding_actual_value(finding: &Finding) -> Option<&Value> {
    match &finding.body {
        FindingBody::DisallowedValue { actual_value, .. }
        | FindingBody::InvalidFieldType { actual_value, .. }
        | FindingBody::ForbiddenField { actual_value, .. } => Some(actual_value),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::RequiredFrontmatterMissing { .. }
        | FindingBody::DocumentMisrouted { .. } => None,
    }
}

fn finding_target(finding: &Finding) -> Option<String> {
    match &finding.body {
        FindingBody::LinkIssue { link } => Some(link.target.clone()),
        _ => None,
    }
}

fn finding_candidates(finding: &Finding) -> Vec<Utf8PathBuf> {
    match &finding.body {
        FindingBody::LinkIssue { link } => link.candidates.clone(),
        _ => Vec::new(),
    }
}
