//! Grouped-count summary over a validated finding set (`validate --summary`).
//!
//! [`summarize`]
//! folds a `&[Finding]` into deterministic `BTreeMap` tallies — by code,
//! severity, rule, field, and path prefix, plus per-field disallowed-value and
//! invalid-type breakdowns. Serialized directly for `--format json --summary`.

use std::collections::BTreeMap;

use crate::domain::Severity;
use camino::Utf8PathBuf;
use serde::Serialize;
use serde_json::Value;

use crate::standards::findings::Finding;

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

        // Rule + field tallies are populated whenever the finding names them
        // (rule-scoped frontmatter findings). The per-code branches add the
        // value / type breakdowns that only certain codes carry. Link, graph,
        // alias-collision, and nonportable findings name no rule/field, so they
        // contribute only to the code / severity / path-prefix rollups.
        if let Some(rule) = &finding.rule {
            increment(&mut summary.rules, rule);
        }
        if let Some(field) = &finding.field {
            increment(&mut summary.fields, field);
        }
        match finding.code.as_str() {
            "value-not-allowed" => {
                if let (Some(field), Some(actual_value)) = (&finding.field, &finding.actual_value) {
                    let value_counts = summary.disallowed_values.entry(field.clone()).or_default();
                    increment(value_counts, summary_value_key(actual_value));
                }
            }
            "field-type-invalid" => {
                if let (Some(field), Some(expected_type)) = (&finding.field, &finding.expected_type)
                {
                    let type_counts = summary.invalid_types.entry(field.clone()).or_default();
                    increment(type_counts, expected_type);
                }
            }
            _ => {}
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
