use std::collections::BTreeMap;

use anyhow::Result;
use camino::Utf8PathBuf;
use serde::Serialize;
use vault_core::{Diagnostic, Document, GraphIndex, Link, LinkStatus, Severity};
use vault_index::{pattern_matches_path, ValidateConfig, ValidateRuleConfig};

#[derive(Debug, Serialize)]
pub struct ValidateFinding {
    pub code: String,
    pub severity: Severity,
    pub path: Utf8PathBuf,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_values: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<Link>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
}

#[derive(Debug, Serialize)]
pub struct ValidateSummary {
    pub findings: usize,
    pub codes: BTreeMap<String, usize>,
    pub severities: BTreeMap<String, usize>,
    pub rules: BTreeMap<String, usize>,
    pub fields: BTreeMap<String, usize>,
    pub disallowed_values: BTreeMap<String, BTreeMap<String, usize>>,
    pub path_prefixes: BTreeMap<String, usize>,
}

pub fn validate_findings(index: &GraphIndex, config: &ValidateConfig) -> Vec<ValidateFinding> {
    let mut findings = Vec::new();

    for document in &index.documents {
        if validate_ignored(document, config) {
            continue;
        }

        for diagnostic in &document.diagnostics {
            findings.push(ValidateFinding {
                code: diagnostic.code.clone(),
                severity: diagnostic.severity.clone(),
                path: document.path.clone(),
                message: diagnostic.message.clone(),
                field: None,
                rule: None,
                actual_value: None,
                allowed_values: None,
                expected_type: None,
                allowed_paths: None,
                link: None,
                diagnostic: Some(diagnostic.clone()),
            });
        }

        for field in &config.required_frontmatter {
            if !document_has_frontmatter_field(document, field) {
                findings.push(ValidateFinding {
                    code: "frontmatter-required-field-missing".to_string(),
                    severity: Severity::Warning,
                    path: document.path.clone(),
                    message: format!("required frontmatter field is missing: {field}"),
                    field: Some(field.clone()),
                    rule: None,
                    actual_value: None,
                    allowed_values: None,
                    expected_type: None,
                    allowed_paths: None,
                    link: None,
                    diagnostic: None,
                });
            }
        }

        for rule in matching_validate_rules(document, &config.rules) {
            let rule_name = rule.name.clone();
            for field in &rule.required_frontmatter {
                if !document_has_frontmatter_field(document, field) {
                    findings.push(ValidateFinding {
                        code: "frontmatter-required-field-missing".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("required frontmatter field is missing: {field}"),
                        field: Some(field.clone()),
                        rule: rule_name.clone(),
                        actual_value: None,
                        allowed_values: None,
                        expected_type: None,
                        allowed_paths: None,
                        link: None,
                        diagnostic: None,
                    });
                }
            }

            for (field, expected_type) in &rule.field_types {
                if let Some(actual_value) = document_frontmatter_field(document, field) {
                    if !frontmatter_type_matches(actual_value, expected_type) {
                        findings.push(ValidateFinding {
                            code: "frontmatter-field-type-invalid".to_string(),
                            severity: Severity::Warning,
                            path: document.path.clone(),
                            message: format!(
                                "frontmatter field has invalid type: {field}; expected {expected_type}"
                            ),
                            field: Some(field.clone()),
                            rule: rule_name.clone(),
                            actual_value: Some(actual_value.clone()),
                            allowed_values: None,
                            expected_type: Some(expected_type.clone()),
                            allowed_paths: None,
                            link: None,
                            diagnostic: None,
                        });
                    }
                }
            }

            for field in &rule.forbidden_frontmatter {
                if let Some(actual_value) = document_frontmatter_field(document, field) {
                    findings.push(ValidateFinding {
                        code: "frontmatter-field-forbidden".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("frontmatter field is forbidden: {field}"),
                        field: Some(field.clone()),
                        rule: rule_name.clone(),
                        actual_value: Some(actual_value.clone()),
                        allowed_values: None,
                        expected_type: None,
                        allowed_paths: None,
                        link: None,
                        diagnostic: None,
                    });
                }
            }

            if !rule.allowed_paths.is_empty()
                && !rule
                    .allowed_paths
                    .iter()
                    .any(|pattern| pattern_matches_path(pattern, &document.path))
            {
                findings.push(ValidateFinding {
                    code: "path-not-allowed".to_string(),
                    severity: Severity::Warning,
                    path: document.path.clone(),
                    message: "document path is outside allowed rule locations".to_string(),
                    field: None,
                    rule: rule_name.clone(),
                    actual_value: None,
                    allowed_values: None,
                    expected_type: None,
                    allowed_paths: Some(rule.allowed_paths.clone()),
                    link: None,
                    diagnostic: None,
                });
            }

            for (field, allowed_values) in &rule.allowed_values {
                if let Some(actual_value) = document_frontmatter_field(document, field) {
                    if !allowed_values
                        .iter()
                        .any(|allowed_value| frontmatter_value_matches(actual_value, allowed_value))
                    {
                        findings.push(ValidateFinding {
                            code: "frontmatter-field-value-not-allowed".to_string(),
                            severity: Severity::Warning,
                            path: document.path.clone(),
                            message: format!("frontmatter field has a disallowed value: {field}"),
                            field: Some(field.clone()),
                            rule: rule_name.clone(),
                            actual_value: Some(actual_value.clone()),
                            allowed_values: Some(allowed_values.clone()),
                            expected_type: None,
                            allowed_paths: None,
                            link: None,
                            diagnostic: None,
                        });
                    }
                }
            }
        }

        for link in &document.links {
            match link.status {
                LinkStatus::Resolved => {}
                LinkStatus::Unresolved => {
                    findings.push(ValidateFinding {
                        code: "link-unresolved".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("unresolved link target: {}", link.target),
                        field: None,
                        rule: None,
                        actual_value: None,
                        allowed_values: None,
                        expected_type: None,
                        allowed_paths: None,
                        link: Some(link.clone()),
                        diagnostic: None,
                    });
                }
                LinkStatus::Ambiguous => {
                    findings.push(ValidateFinding {
                        code: "link-ambiguous".to_string(),
                        severity: Severity::Warning,
                        path: document.path.clone(),
                        message: format!("ambiguous link target: {}", link.target),
                        field: None,
                        rule: None,
                        actual_value: None,
                        allowed_values: None,
                        expected_type: None,
                        allowed_paths: None,
                        link: Some(link.clone()),
                        diagnostic: None,
                    });
                }
            }
        }
    }

    findings
}

pub fn validate_summary(findings: &[ValidateFinding]) -> ValidateSummary {
    let mut summary = ValidateSummary {
        findings: findings.len(),
        codes: BTreeMap::new(),
        severities: BTreeMap::new(),
        rules: BTreeMap::new(),
        fields: BTreeMap::new(),
        disallowed_values: BTreeMap::new(),
        path_prefixes: BTreeMap::new(),
    };

    for finding in findings {
        increment(&mut summary.codes, &finding.code);
        increment(&mut summary.severities, severity_key(&finding.severity));
        if let Some(rule) = &finding.rule {
            increment(&mut summary.rules, rule);
        }
        if let Some(field) = &finding.field {
            increment(&mut summary.fields, field);
        }
        if finding.code == "frontmatter-field-value-not-allowed" {
            if let (Some(field), Some(actual_value)) = (&finding.field, &finding.actual_value) {
                let value_counts = summary.disallowed_values.entry(field.clone()).or_default();
                increment(value_counts, summary_value_key(actual_value));
            }
        }
        increment(&mut summary.path_prefixes, &path_prefix_key(&finding.path));
    }

    summary
}

fn validate_ignored(document: &Document, config: &ValidateConfig) -> bool {
    config
        .ignore
        .iter()
        .any(|pattern| pattern_matches_path(pattern, &document.path))
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

fn summary_value_key(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
        }
    }
}

fn matching_validate_rules<'a>(
    document: &Document,
    rules: &'a [ValidateRuleConfig],
) -> Vec<&'a ValidateRuleConfig> {
    rules
        .iter()
        .filter(|rule| validate_rule_matches(document, rule))
        .collect()
}

fn validate_rule_matches(document: &Document, rule: &ValidateRuleConfig) -> bool {
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

    if let Some(exclude_path_pattern) = &rule.exclude.path {
        if pattern_matches_path(exclude_path_pattern, &document.path) {
            return false;
        }
    }

    frontmatter_predicates_match(document, &rule.r#match.frontmatter)
}

fn frontmatter_predicates_match(
    document: &Document,
    predicates: &std::collections::HashMap<String, serde_json::Value>,
) -> bool {
    if predicates.is_empty() {
        return true;
    }

    let Some(frontmatter) = document.frontmatter.as_ref() else {
        return false;
    };

    predicates.iter().all(|(field, expected)| {
        frontmatter
            .get(field)
            .is_some_and(|actual| frontmatter_value_matches(actual, expected))
    })
}

fn frontmatter_value_matches(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    match (actual, expected) {
        (serde_json::Value::String(actual), serde_json::Value::String(expected)) => {
            actual == expected
        }
        (serde_json::Value::Bool(actual), serde_json::Value::Bool(expected)) => actual == expected,
        (serde_json::Value::Number(actual), serde_json::Value::Number(expected)) => {
            actual == expected
        }
        _ => false,
    }
}

fn frontmatter_type_matches(value: &serde_json::Value, expected_type: &str) -> bool {
    match expected_type {
        "datetime" => value
            .as_str()
            .is_some_and(|value| is_datetime_string(value)),
        "date" => value.as_str().is_some_and(is_date_string),
        "list_of_strings" => value
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value.as_str().is_some())),
        "wikilink" => value.as_str().is_some_and(is_wikilink_string),
        "wikilink_or_list" => {
            value.as_str().is_some_and(is_wikilink_string)
                || value.as_array().is_some_and(|values| {
                    values
                        .iter()
                        .all(|value| value.as_str().is_some_and(is_wikilink_string))
                })
        }
        _ => false,
    }
}

fn is_datetime_string(value: &str) -> bool {
    if value.len() < 16 {
        return false;
    }

    let Some((date, time)) = value.split_once('T').or_else(|| value.split_once(' ')) else {
        return false;
    };

    is_date_string(date) && is_time_string(time)
}

fn is_date_string(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };

    year.len() == 4
        && month.len() == 2
        && day.len() == 2
        && year.chars().all(|char| char.is_ascii_digit())
        && month
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
        && day.parse::<u8>().is_ok_and(|day| (1..=31).contains(&day))
}

fn is_time_string(value: &str) -> bool {
    let time = value
        .strip_suffix('Z')
        .unwrap_or(value)
        .split_once(['+', '-'])
        .map_or(value, |(time, _)| time);
    let mut parts = time.split(':');
    let (Some(hour), Some(minute), maybe_second, None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };

    hour.len() == 2
        && minute.len() == 2
        && hour.parse::<u8>().is_ok_and(|hour| hour <= 23)
        && minute.parse::<u8>().is_ok_and(|minute| minute <= 59)
        && maybe_second.is_none_or(|second| {
            second.len() == 2 && second.parse::<u8>().is_ok_and(|second| second <= 59)
        })
}

fn is_wikilink_string(value: &str) -> bool {
    value.starts_with("[[") && value.ends_with("]]") && value.len() > 4
}

fn document_has_frontmatter_field(document: &Document, field: &str) -> bool {
    document_frontmatter_field(document, field).is_some()
}

fn document_frontmatter_field<'a>(
    document: &'a Document,
    field: &str,
) -> Option<&'a serde_json::Value> {
    document
        .frontmatter
        .as_ref()
        .and_then(|frontmatter| frontmatter.get(field))
        .filter(|value| !value.is_null())
}

pub fn validate_config_value(config_path: &Utf8PathBuf, value: &serde_yaml::Value) -> Result<()> {
    let Some(root) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: root must be a mapping");
    };

    if let Some(graph) = mapping_get(root, "graph") {
        let Some(graph) = graph.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: graph must be a mapping");
        };

        if let Some(ignore) = mapping_get(graph, "ignore") {
            validate_string_sequence(config_path, "graph.ignore", ignore)?;
        }
    }

    if let Some(validate) = mapping_get(root, "validate") {
        let Some(validate) = validate.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: validate must be a mapping");
        };

        if let Some(required_frontmatter) = mapping_get(validate, "required_frontmatter") {
            validate_string_sequence(
                config_path,
                "validate.required_frontmatter",
                required_frontmatter,
            )?;
        }

        if let Some(ignore) = mapping_get(validate, "ignore") {
            validate_string_sequence(config_path, "validate.ignore", ignore)?;
        }

        if let Some(rules) = mapping_get(validate, "rules") {
            let Some(rules) = rules.as_sequence() else {
                anyhow::bail!("invalid config {config_path}: validate.rules must be a sequence");
            };

            for (index, rule) in rules.iter().enumerate() {
                let rule_path = format!("validate.rules[{index}]");
                validate_rule_value(config_path, &rule_path, rule)?;
            }
        }
    }

    Ok(())
}

fn validate_rule_value(
    config_path: &Utf8PathBuf,
    rule_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(rule) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {rule_path} must be a mapping");
    };

    if let Some(name) = mapping_get(rule, "name") {
        if name.as_str().is_none() {
            anyhow::bail!("invalid config {config_path}: {rule_path}.name must be a string");
        }
    }

    if let Some(rule_match) = mapping_get(rule, "match") {
        let Some(rule_match) = rule_match.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: {rule_path}.match must be a mapping");
        };

        validate_known_mapping_keys(
            config_path,
            &format!("{rule_path}.match"),
            rule_match,
            &["path", "path_not", "frontmatter"],
        )?;

        if let Some(path) = mapping_get(rule_match, "path") {
            if path.as_str().is_none() {
                anyhow::bail!(
                    "invalid config {config_path}: {rule_path}.match.path must be a string"
                );
            }
        }

        if let Some(path_not) = mapping_get(rule_match, "path_not") {
            if path_not.as_str().is_none() {
                anyhow::bail!(
                    "invalid config {config_path}: {rule_path}.match.path_not must be a string"
                );
            }
        }

        if let Some(frontmatter) = mapping_get(rule_match, "frontmatter") {
            validate_frontmatter_predicates(
                config_path,
                &format!("{rule_path}.match.frontmatter"),
                frontmatter,
            )?;
        }
    }

    if let Some(required_frontmatter) = mapping_get(rule, "required_frontmatter") {
        validate_string_sequence(
            config_path,
            &format!("{rule_path}.required_frontmatter"),
            required_frontmatter,
        )?;
    }

    if let Some(allowed_values) = mapping_get(rule, "allowed_values") {
        validate_allowed_values(
            config_path,
            &format!("{rule_path}.allowed_values"),
            allowed_values,
        )?;
    }

    if let Some(field_types) = mapping_get(rule, "field_types") {
        validate_field_types(
            config_path,
            &format!("{rule_path}.field_types"),
            field_types,
        )?;
    }

    if let Some(forbidden_frontmatter) = mapping_get(rule, "forbidden_frontmatter") {
        validate_string_sequence(
            config_path,
            &format!("{rule_path}.forbidden_frontmatter"),
            forbidden_frontmatter,
        )?;
    }

    if let Some(allowed_paths) = mapping_get(rule, "allowed_paths") {
        validate_string_sequence(
            config_path,
            &format!("{rule_path}.allowed_paths"),
            allowed_paths,
        )?;
    }

    if let Some(exclude) = mapping_get(rule, "exclude") {
        let Some(exclude) = exclude.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: {rule_path}.exclude must be a mapping");
        };
        validate_known_mapping_keys(
            config_path,
            &format!("{rule_path}.exclude"),
            exclude,
            &["path"],
        )?;
        if let Some(path) = mapping_get(exclude, "path") {
            if path.as_str().is_none() {
                anyhow::bail!(
                    "invalid config {config_path}: {rule_path}.exclude.path must be a string"
                );
            }
        }
    }

    Ok(())
}

fn validate_known_mapping_keys(
    config_path: &Utf8PathBuf,
    field_path: &str,
    mapping: &serde_yaml::Mapping,
    known_keys: &[&str],
) -> Result<()> {
    for key in mapping.keys() {
        let Some(key) = key.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };

        if !known_keys.contains(&key) {
            anyhow::bail!("invalid config {config_path}: unknown key {field_path}.{key}");
        }
    }

    Ok(())
}

fn validate_frontmatter_predicates(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(predicates) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    for (field, expected) in predicates {
        let Some(field) = field.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };

        if !is_scalar_yaml_value(expected) {
            anyhow::bail!(
                "invalid config {config_path}: {field_path}.{field} must be a string, boolean, or number"
            );
        }
    }

    Ok(())
}

fn validate_allowed_values(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(fields) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    for (field, allowed_values) in fields {
        let Some(field) = field.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };

        let Some(values) = allowed_values.as_sequence() else {
            anyhow::bail!("invalid config {config_path}: {field_path}.{field} must be a sequence");
        };

        if values.is_empty() {
            anyhow::bail!("invalid config {config_path}: {field_path}.{field} must not be empty");
        }

        for (index, allowed_value) in values.iter().enumerate() {
            if !is_scalar_yaml_value(allowed_value) {
                anyhow::bail!(
                    "invalid config {config_path}: {field_path}.{field}[{index}] must be a string, boolean, or number"
                );
            }
        }
    }

    Ok(())
}

fn validate_field_types(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(fields) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    for (field, field_type) in fields {
        let Some(field) = field.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path} keys must be strings");
        };
        let Some(field_type) = field_type.as_str() else {
            anyhow::bail!("invalid config {config_path}: {field_path}.{field} must be a string");
        };
        if !is_known_field_type(field_type) {
            anyhow::bail!(
                "invalid config {config_path}: {field_path}.{field} has unknown field type: {field_type}"
            );
        }
    }

    Ok(())
}

fn is_known_field_type(field_type: &str) -> bool {
    matches!(
        field_type,
        "datetime" | "date" | "list_of_strings" | "wikilink" | "wikilink_or_list"
    )
}

fn is_scalar_yaml_value(value: &serde_yaml::Value) -> bool {
    matches!(
        value,
        serde_yaml::Value::String(_) | serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_)
    )
}

fn validate_string_sequence(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(items) = value.as_sequence() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a sequence");
    };

    for (index, item) in items.iter().enumerate() {
        if item.as_str().is_none() {
            anyhow::bail!("invalid config {config_path}: {field_path}[{index}] must be a string");
        }
    }

    Ok(())
}

fn mapping_get<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a serde_yaml::Value> {
    mapping.get(&serde_yaml::Value::String(key.to_string()))
}

#[cfg(test)]
mod config_validation_tests {
    use crate::config::load_config;
    use camino::Utf8PathBuf;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rejects_malformed_validate_rule_match_path() {
        let config_path = write_temp_config(
            "validate:\n  rules:\n    - name: bad\n      match:\n        path: 123\n      required_frontmatter:\n        - type\n",
        );

        let cwd =
            Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp path should be utf8");
        let message = match load_config(&cwd, Some(&config_path)) {
            Ok(_) => panic!("config should fail validation"),
            Err(error) => error.to_string(),
        };

        assert!(message.contains("invalid config"));
        assert!(message.contains("validate.rules[0].match.path must be a string"));
    }

    #[test]
    fn rejects_malformed_scoped_required_frontmatter() {
        let config_path = write_temp_config(
            "validate:\n  rules:\n    - name: bad\n      match:\n        path: Workspaces/**/*.md\n      required_frontmatter:\n        - 123\n",
        );

        let cwd =
            Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp path should be utf8");
        let message = match load_config(&cwd, Some(&config_path)) {
            Ok(_) => panic!("config should fail validation"),
            Err(error) => error.to_string(),
        };

        assert!(message.contains("invalid config"));
        assert!(message.contains("validate.rules[0].required_frontmatter[0] must be a string"));
    }

    fn write_temp_config(contents: &str) -> Utf8PathBuf {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        path.push(format!("vault-cli-config-validation-{nanos}.yaml"));
        fs::write(&path, contents).expect("temp config should be written");
        Utf8PathBuf::from_path_buf(path).expect("temp path should be utf8")
    }
}
