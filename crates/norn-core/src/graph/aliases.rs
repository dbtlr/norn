//! Alias-field parsing from a document's frontmatter.

use serde_json::Value;

/// Parses the configured alias field from a doc's frontmatter, returning
/// (lowercased coerced strings, malformed raw values).
///
/// Coercion rules:
/// - String, Number, Bool → string-repr, lowercased
/// - Null entries inside a list → silently skipped (empty slot is not drift)
/// - Maps, Sequences → malformed
/// - Top-level: scalar treated as single-element list; null contributes nothing;
///   map at top level is malformed
pub fn parse_aliases(frontmatter: Option<&Value>, field: &str) -> (Vec<String>, Vec<Value>) {
    let mut aliases = Vec::new();
    let mut malformed = Vec::new();

    let Some(Value::Object(obj)) = frontmatter else {
        return (aliases, malformed);
    };
    let Some(value) = obj.get(field) else {
        return (aliases, malformed);
    };

    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => {
            if let Some(s) = coerce_scalar(value) {
                aliases.push(s);
            }
        }
        Value::Null => {}
        Value::Array(items) => {
            for item in items {
                match item {
                    Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                        if let Some(s) = coerce_scalar(item) {
                            aliases.push(s);
                        }
                    }
                    Value::Null => {}
                    Value::Array(_) | Value::Object(_) => {
                        malformed.push(item.clone());
                    }
                }
            }
        }
        Value::Object(_) => {
            malformed.push(value.clone());
        }
    }

    (aliases, malformed)
}

fn coerce_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.to_lowercase()),
        Value::Number(n) => Some(n.to_string().to_lowercase()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: Value, field: &str) -> (Vec<String>, Vec<Value>) {
        let fm = json!({ field: value });
        parse_aliases(Some(&fm), field)
    }

    #[test]
    fn list_of_strings_lowercased() {
        let (aliases, malformed) = parse(json!(["Vault Memory", "VM"]), "aliases");
        assert_eq!(aliases, vec!["vault memory".to_string(), "vm".to_string()]);
        assert!(malformed.is_empty());
    }

    #[test]
    fn scalar_string_at_top_level_becomes_single_element() {
        let (aliases, malformed) = parse(json!("Vault Memory"), "aliases");
        assert_eq!(aliases, vec!["vault memory".to_string()]);
        assert!(malformed.is_empty());
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn numeric_alias_is_coerced() {
        let (aliases, malformed) = parse(json!([42, 3.14]), "aliases");
        assert_eq!(aliases, vec!["42".to_string(), "3.14".to_string()]);
        assert!(malformed.is_empty());
    }

    #[test]
    fn boolean_alias_is_coerced() {
        let (aliases, malformed) = parse(json!([true, false]), "aliases");
        assert_eq!(aliases, vec!["true".to_string(), "false".to_string()]);
        assert!(malformed.is_empty());
    }

    #[test]
    fn null_entries_in_list_silently_skipped() {
        let (aliases, malformed) = parse(json!(["foo", null, "bar"]), "aliases");
        assert_eq!(aliases, vec!["foo".to_string(), "bar".to_string()]);
        assert!(malformed.is_empty());
    }

    #[test]
    fn map_entry_is_malformed() {
        let (aliases, malformed) = parse(json!(["foo", {"nested": "x"}]), "aliases");
        assert_eq!(aliases, vec!["foo".to_string()]);
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0], json!({"nested": "x"}));
    }

    #[test]
    fn nested_sequence_entry_is_malformed() {
        let (aliases, malformed) = parse(json!(["foo", ["a", "b"]]), "aliases");
        assert_eq!(aliases, vec!["foo".to_string()]);
        assert_eq!(malformed.len(), 1);
    }

    #[test]
    fn top_level_map_is_malformed() {
        let (aliases, malformed) = parse(json!({"a": "b"}), "aliases");
        assert!(aliases.is_empty());
        assert_eq!(malformed.len(), 1);
        assert_eq!(malformed[0], json!({"a": "b"}));
    }

    #[test]
    fn top_level_null_contributes_nothing() {
        let (aliases, malformed) = parse(json!(null), "aliases");
        assert!(aliases.is_empty());
        assert!(malformed.is_empty());
    }

    #[test]
    fn frontmatter_lacks_field_yields_empty() {
        let fm = json!({"other_field": "x"});
        let (aliases, malformed) = parse_aliases(Some(&fm), "aliases");
        assert!(aliases.is_empty());
        assert!(malformed.is_empty());
    }

    #[test]
    fn no_frontmatter_yields_empty() {
        let (aliases, malformed) = parse_aliases(None, "aliases");
        assert!(aliases.is_empty());
        assert!(malformed.is_empty());
    }

    #[test]
    fn whitespace_is_preserved_inside_alias() {
        let (aliases, _) = parse(json!(["Vault Memory"]), "aliases");
        assert_eq!(aliases, vec!["vault memory".to_string()]);
    }
}
