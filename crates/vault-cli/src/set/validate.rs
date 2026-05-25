//! Schema-aware pre-flight validation for `vault set`.

// These functions are pub for Phase 5 wiring; the binary doesn't call them yet.
#![allow(dead_code)]

use anyhow::Result;
use serde_json::Value;
use vault_core::Document;
use vault_standards::VaultConfig;

/// Look up the declared schema type for `field` on the given document.
/// Returns the type string (e.g. "datetime", "list_of_strings", "wikilink") or
/// None when no matching rule declares a type for the field.
pub fn lookup_field_type(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<String> {
    for rule in &cfg.validate.rules {
        if !vault_standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(ty) = rule.field_types.get(field) {
            return Some(ty.clone());
        }
    }
    None
}

/// Coerce a raw CLI value string into a typed JSON Value matching the declared
/// schema type. Refuses when the input cannot be expressed as the type.
///
/// Wikilink-typed values are auto-wrapped: `vault-cli` becomes `[[vault-cli]]`.
/// Already-bracketed input passes through. Empty-stem wikilinks (`[[]]`) are
/// refused as shape-invalid.
pub fn coerce_value_for_type(field_type: &str, raw: &str) -> Result<Value> {
    match field_type {
        "datetime" => {
            if vault_standards::predicates::is_datetime_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                anyhow::bail!(
                    "value '{raw}' is not a valid datetime (expected YYYY-MM-DDTHH:MM[:SS])"
                )
            }
        }
        "date" => {
            if vault_standards::predicates::is_date_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                anyhow::bail!("value '{raw}' is not a valid date (expected YYYY-MM-DD)")
            }
        }
        "wikilink" => {
            let wrapped = wrap_wikilink(raw);
            if !vault_standards::predicates::is_wikilink_string(&wrapped) {
                anyhow::bail!(
                    "value '{raw}' is not shape-valid as a wikilink (need non-empty stem inside [[…]])"
                )
            }
            Ok(Value::String(wrapped))
        }
        "wikilink_or_list" => {
            let wrapped = wrap_wikilink(raw);
            if !vault_standards::predicates::is_wikilink_string(&wrapped) {
                anyhow::bail!(
                    "value '{raw}' is not shape-valid as a wikilink (need non-empty stem inside [[…]])"
                )
            }
            Ok(Value::String(wrapped))
        }
        "list_of_strings" => Ok(Value::Array(vec![Value::String(raw.to_string())])),
        unknown => anyhow::bail!("unknown field_type: {unknown}"),
    }
}

fn wrap_wikilink(raw: &str) -> String {
    if raw.starts_with("[[") && raw.ends_with("]]") {
        raw.to_string()
    } else {
        format!("[[{raw}]]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use serde_json::json;

    fn fixture_doc_kind_note() -> Document {
        let frontmatter = Some(json!({"kind": "note", "title": "Foo"}));
        Document {
            path: Utf8PathBuf::from("notes/foo.md"),
            stem: "foo".to_string(),
            hash: "abc123".to_string(),
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

    fn fixture_config_with_field_types() -> VaultConfig {
        let yaml = r#"
validate:
  rules:
    - name: note-fields
      match:
        frontmatter:
          kind: note
      field_types:
        created: datetime
        aliases: list_of_strings
        workspace: wikilink
      required_frontmatter:
        - created
"#;
        vault_standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse")
    }

    // ── Task 4.1: lookup_field_type ──────────────────────────────────────────

    #[test]
    fn lookup_field_type_returns_type_for_matched_rule() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        assert_eq!(
            lookup_field_type(&cfg, &doc, "created"),
            Some("datetime".to_string())
        );
        assert_eq!(
            lookup_field_type(&cfg, &doc, "aliases"),
            Some("list_of_strings".to_string())
        );
        assert_eq!(
            lookup_field_type(&cfg, &doc, "workspace"),
            Some("wikilink".to_string())
        );
    }

    #[test]
    fn lookup_field_type_returns_none_for_unknown_field() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        assert_eq!(lookup_field_type(&cfg, &doc, "madeup"), None);
    }

    #[test]
    fn lookup_field_type_returns_none_when_no_rule_matches() {
        let frontmatter = Some(json!({"kind": "task"}));
        let doc = Document {
            path: Utf8PathBuf::from("tasks/foo.md"),
            stem: "foo".to_string(),
            hash: "abc123".to_string(),
            frontmatter,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        };
        let cfg = fixture_config_with_field_types();
        assert_eq!(lookup_field_type(&cfg, &doc, "created"), None);
    }

    // ── Task 4.2: coerce_value_for_type ──────────────────────────────────────

    #[test]
    fn coerce_value_passes_through_string_when_type_matches_string_shape() {
        let raw = "2026-05-25T12:00:00";
        let out = coerce_value_for_type("datetime", raw).expect("should accept");
        assert_eq!(out, json!("2026-05-25T12:00:00"));
    }

    #[test]
    fn coerce_value_refuses_invalid_datetime() {
        assert!(coerce_value_for_type("datetime", "not a date").is_err());
    }

    #[test]
    fn coerce_value_wraps_bare_stem_in_wikilink_brackets() {
        let out = coerce_value_for_type("wikilink", "vault-cli").expect("should wrap");
        assert_eq!(out, json!("[[vault-cli]]"));
    }

    #[test]
    fn coerce_value_passes_through_already_bracketed_wikilink() {
        let out = coerce_value_for_type("wikilink", "[[vault-cli]]").expect("should accept");
        assert_eq!(out, json!("[[vault-cli]]"));
    }

    #[test]
    fn coerce_value_refuses_empty_wikilink_brackets() {
        // wrapping "" yields "[[]]" which is shape-invalid per is_wikilink_string.
        assert!(coerce_value_for_type("wikilink", "").is_err());
    }

    #[test]
    fn coerce_value_for_list_of_strings_wraps_single_string() {
        let out = coerce_value_for_type("list_of_strings", "single").expect("should wrap");
        assert_eq!(out, json!(["single"]));
    }

    #[test]
    fn coerce_value_refuses_unknown_field_type() {
        assert!(coerce_value_for_type("some_unknown", "x").is_err());
    }
}
