use std::collections::BTreeMap;

use crate::core::Severity;
use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;

use crate::standards::findings::{Finding, FindingBody};

#[derive(Debug, Serialize)]
pub struct Summary {
    pub findings: usize,
    pub codes: BTreeMap<String, usize>,
    pub severities: BTreeMap<String, usize>,
    pub rules: BTreeMap<String, usize>,
    pub fields: BTreeMap<String, usize>,
    pub disallowed_values: BTreeMap<String, BTreeMap<String, usize>>,
    pub invalid_types: BTreeMap<String, BTreeMap<String, usize>>,
    pub path_prefixes: BTreeMap<String, usize>,
}

pub fn summarize(findings: &[Finding]) -> Summary {
    let mut summary = Summary {
        findings: findings.len(),
        codes: BTreeMap::new(),
        severities: BTreeMap::new(),
        rules: BTreeMap::new(),
        fields: BTreeMap::new(),
        disallowed_values: BTreeMap::new(),
        invalid_types: BTreeMap::new(),
        path_prefixes: BTreeMap::new(),
    };

    for finding in findings {
        increment(&mut summary.codes, &finding.code);
        increment(&mut summary.severities, severity_key(&finding.severity));

        match &finding.body {
            FindingBody::RequiredFrontmatterMissing { rule, field } => {
                if let Some(rule) = rule {
                    increment(&mut summary.rules, rule);
                }
                increment(&mut summary.fields, field);
            }
            FindingBody::DisallowedValue {
                rule,
                field,
                actual_value,
                ..
            } => {
                if let Some(rule) = rule {
                    increment(&mut summary.rules, rule);
                }
                increment(&mut summary.fields, field);
                let value_counts = summary.disallowed_values.entry(field.clone()).or_default();
                increment(value_counts, summary_value_key(actual_value));
            }
            FindingBody::InvalidFieldType {
                rule,
                field,
                expected_type,
                ..
            } => {
                if let Some(rule) = rule {
                    increment(&mut summary.rules, rule);
                }
                increment(&mut summary.fields, field);
                let type_counts = summary.invalid_types.entry(field.clone()).or_default();
                increment(type_counts, expected_type);
            }
            FindingBody::ForbiddenField { rule, field, .. }
            | FindingBody::ReferenceType { rule, field, .. } => {
                if let Some(rule) = rule {
                    increment(&mut summary.rules, rule);
                }
                increment(&mut summary.fields, field);
            }
            FindingBody::DocumentMisrouted { rule, .. } => {
                if let Some(rule) = rule {
                    increment(&mut summary.rules, rule);
                }
            }
            FindingBody::AliasMalformed { field, .. } => {
                increment(&mut summary.fields, field);
            }
            FindingBody::AliasShadowedByStem { .. }
            | FindingBody::AliasDuplicateAcrossDocs { .. }
            | FindingBody::LinkIssue { .. }
            | FindingBody::GraphDiagnostic { .. } => {}
        }

        increment(&mut summary.path_prefixes, path_prefix_key(&finding.path));
    }

    summary
}

fn increment(counts: &mut BTreeMap<String, usize>, key: impl AsRef<str>) {
    *counts.entry(key.as_ref().to_string()).or_insert(0) += 1;
}

fn severity_key(severity: &Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

fn path_prefix_key(path: &Utf8PathBuf) -> String {
    let path = path.as_str();
    match path.split_once('/') {
        Some((prefix, _)) if !prefix.is_empty() => prefix.to_string(),
        _ => "root".to_string(),
    }
}

fn summary_value_key(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
        }
    }
}
