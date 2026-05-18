use anyhow::Result;
use camino::Utf8PathBuf;

pub fn validate_config_yaml(config_path: &Utf8PathBuf, value: &serde_yaml::Value) -> Result<()> {
    let Some(root) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: root must be a mapping");
    };

    if let Some(files) = mapping_get(root, "files") {
        let Some(files) = files.as_mapping() else {
            anyhow::bail!("invalid config {config_path}: files must be a mapping");
        };

        if let Some(ignore) = mapping_get(files, "ignore") {
            validate_string_sequence(config_path, "files.ignore", ignore)?;
        }
    }

    if let Some(graph) = mapping_get(root, "graph") {
        if let Some(graph) = graph.as_mapping() {
            if mapping_get(graph, "ignore").is_some() {
                anyhow::bail!(
                    "invalid config {config_path}: 'graph.ignore' was renamed to 'files.ignore' in v0.16"
                );
            }
        }
        anyhow::bail!(
            "invalid config {config_path}: 'graph.ignore' was renamed to 'files.ignore' in v0.16"
        );
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

    if let Some(repair) = mapping_get(root, "repair") {
        validate_repair_config(config_path, repair)?;
    }

    Ok(())
}

fn validate_repair_config(config_path: &Utf8PathBuf, value: &serde_yaml::Value) -> Result<()> {
    let Some(repair) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: repair must be a mapping");
    };

    validate_known_mapping_keys(config_path, "repair", repair, &["rules"])?;

    if let Some(rules) = mapping_get(repair, "rules") {
        let Some(rules) = rules.as_sequence() else {
            anyhow::bail!("invalid config {config_path}: repair.rules must be a sequence");
        };

        for (index, rule) in rules.iter().enumerate() {
            validate_repair_rule(config_path, &format!("repair.rules[{index}]"), rule)?;
        }
    }

    Ok(())
}

fn validate_repair_rule(
    config_path: &Utf8PathBuf,
    rule_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(rule) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {rule_path} must be a mapping");
    };

    validate_known_mapping_keys(
        config_path,
        rule_path,
        rule,
        &["name", "match", "set_frontmatter", "remove_frontmatter"],
    )?;

    if let Some(name) = mapping_get(rule, "name") {
        if name.as_str().is_none() {
            anyhow::bail!("invalid config {config_path}: {rule_path}.name must be a string");
        }
    }

    if let Some(rule_match) = mapping_get(rule, "match") {
        validate_repair_match(config_path, &format!("{rule_path}.match"), rule_match)?;
    }

    let has_set = mapping_get(rule, "set_frontmatter").is_some();
    let has_remove = mapping_get(rule, "remove_frontmatter").is_some();
    match (has_set, has_remove) {
        (true, true) => anyhow::bail!(
            "invalid config {config_path}: {rule_path} must declare exactly one repair action"
        ),
        (false, false) => {
            anyhow::bail!("invalid config {config_path}: {rule_path} must declare a repair action")
        }
        _ => {}
    }

    if let Some(action) = mapping_get(rule, "set_frontmatter") {
        validate_set_frontmatter_action(
            config_path,
            &format!("{rule_path}.set_frontmatter"),
            action,
        )?;
    }
    if let Some(action) = mapping_get(rule, "remove_frontmatter") {
        validate_remove_frontmatter_action(
            config_path,
            &format!("{rule_path}.remove_frontmatter"),
            action,
        )?;
    }

    Ok(())
}

fn validate_repair_match(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(rule_match) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    validate_known_mapping_keys(
        config_path,
        field_path,
        rule_match,
        &["code", "rule", "field", "actual_value"],
    )?;

    for key in ["code", "rule", "field"] {
        if let Some(value) = mapping_get(rule_match, key) {
            if value.as_str().is_none() {
                anyhow::bail!("invalid config {config_path}: {field_path}.{key} must be a string");
            }
        }
    }

    if let Some(actual_value) = mapping_get(rule_match, "actual_value") {
        if !is_scalar_yaml_value(actual_value) {
            anyhow::bail!(
                "invalid config {config_path}: {field_path}.actual_value must be a string, boolean, or number"
            );
        }
    }

    Ok(())
}

fn validate_set_frontmatter_action(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(action) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    validate_known_mapping_keys(config_path, field_path, action, &["field", "value"])?;

    match mapping_get(action, "field").and_then(serde_yaml::Value::as_str) {
        Some(field) if !field.is_empty() => {}
        _ => anyhow::bail!("invalid config {config_path}: {field_path}.field must be a string"),
    }

    if mapping_get(action, "value").is_none() {
        anyhow::bail!("invalid config {config_path}: {field_path}.value is required");
    }

    Ok(())
}

fn validate_remove_frontmatter_action(
    config_path: &Utf8PathBuf,
    field_path: &str,
    value: &serde_yaml::Value,
) -> Result<()> {
    let Some(action) = value.as_mapping() else {
        anyhow::bail!("invalid config {config_path}: {field_path} must be a mapping");
    };

    validate_known_mapping_keys(config_path, field_path, action, &["field"])?;

    match mapping_get(action, "field").and_then(serde_yaml::Value::as_str) {
        Some(field) if !field.is_empty() => Ok(()),
        _ => anyhow::bail!("invalid config {config_path}: {field_path}.field must be a string"),
    }
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
    mapping.get(serde_yaml::Value::String(key.to_string()))
}

#[cfg(test)]
mod tests {
    use super::validate_config_yaml;
    use camino::Utf8PathBuf;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn rejects_malformed_validate_rule_match_path() {
        let config_path = write_temp_config(
            "validate:\n  rules:\n    - name: bad\n      match:\n        path: 123\n      required_frontmatter:\n        - type\n",
        );
        let config_text = fs::read_to_string(&config_path).unwrap();
        let config_value: serde_yaml::Value = serde_yaml::from_str(&config_text).unwrap();
        let message = match validate_config_yaml(&config_path, &config_value) {
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
        let config_text = fs::read_to_string(&config_path).unwrap();
        let config_value: serde_yaml::Value = serde_yaml::from_str(&config_text).unwrap();
        let message = match validate_config_yaml(&config_path, &config_value) {
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
