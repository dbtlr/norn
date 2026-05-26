//! Schema-aware pre-flight validation for `vault set`.

// These functions are pub for Phase 5 wiring; the binary doesn't call them yet.
#![allow(dead_code)]

use crate::standards::PlannedChange;
use crate::standards::VaultConfig;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use vault_core::Document;

// ── Warning types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SetWarning {
    UnknownField {
        field: String,
        message: String,
    },
    WikilinkUnresolved {
        field: String,
        target: String,
    },
    WikilinkAmbiguous {
        field: String,
        target: String,
        candidates: Vec<String>,
    },
    ForceBypass {
        field: String,
        message: String,
    },
}

#[derive(Debug)]
pub struct SynthResult {
    pub changes: Vec<PlannedChange>,
    pub warnings: Vec<SetWarning>,
}

/// Look up the declared schema type for `field` on the given document.
/// Returns the type string (e.g. "datetime", "list_of_strings", "wikilink") or
/// None when no matching rule declares a type for the field.
pub fn lookup_field_type(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<String> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
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
            if crate::standards::predicates::is_datetime_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                anyhow::bail!(
                    "value '{raw}' is not a valid datetime (expected YYYY-MM-DDTHH:MM[:SS])"
                )
            }
        }
        "date" => {
            if crate::standards::predicates::is_date_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                anyhow::bail!("value '{raw}' is not a valid date (expected YYYY-MM-DD)")
            }
        }
        "wikilink" => {
            let wrapped = wrap_wikilink(raw);
            if !crate::standards::predicates::is_wikilink_string(&wrapped) {
                anyhow::bail!(
                    "value '{raw}' is not shape-valid as a wikilink (need non-empty stem inside [[…]])"
                )
            }
            Ok(Value::String(wrapped))
        }
        "wikilink_or_list" => {
            let wrapped = wrap_wikilink(raw);
            if !crate::standards::predicates::is_wikilink_string(&wrapped) {
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

/// Check whether a field is declared required-frontmatter by any rule that
/// matches this document.
pub fn is_required_field(cfg: &VaultConfig, doc: &Document, field: &str) -> bool {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if rule.required_frontmatter.iter().any(|f| f == field) {
            return true;
        }
    }
    false
}

/// Output type for `coerce_kv_slice`: typed pairs + any emitted warnings.
type CoercedKvs = (Vec<(String, Value)>, Vec<SetWarning>);

/// Coerce one `KEY=raw` slice into typed `(KEY, Value)` pairs.
/// Returns `(typed_pairs, warnings)`.
fn coerce_kv_slice(
    raw_kvs: &[String],
    force: bool,
    cfg: &VaultConfig,
    doc: &Document,
) -> Result<CoercedKvs> {
    let mut out = Vec::new();
    let mut w = Vec::new();
    for kv in raw_kvs {
        let (key, raw) = crate::set::synth::parse_kv(kv)?;
        let coerced = match lookup_field_type(cfg, doc, &key) {
            Some(ty) if !force => coerce_value_for_type(&ty, &raw)?,
            Some(_) => {
                w.push(SetWarning::ForceBypass {
                    field: key.clone(),
                    message: format!("--force bypassed type validation for '{key}'"),
                });
                Value::String(raw)
            }
            None => {
                w.push(SetWarning::UnknownField {
                    field: key.clone(),
                    message: format!("field '{key}' not declared in schema"),
                });
                crate::set::synth::infer_scalar(&raw)
            }
        };
        out.push((key, coerced));
    }
    Ok((out, w))
}

/// Schema-aware plan synthesis. Coerces values per schema; falls back to light
/// inference when no schema declares the field. Refuses on type mismatch
/// unless --force. Emits SetWarning entries for unknown fields, force bypasses,
/// and required-field bypasses.
///
/// Wikilink resolution warnings live separately in check_wikilink_resolution
/// and are added by the caller after this returns (caller has GraphIndex).
#[allow(clippy::too_many_arguments)]
pub fn synth_with_schema(
    cfg: &VaultConfig,
    doc: &Document,
    current_frontmatter: &Value,
    fields: &[String],
    field_json: &[String],
    push: &[String],
    pop: &[String],
    remove: &[String],
    force: bool,
) -> Result<SynthResult> {
    // Cross-class conflict refusal happens first.
    crate::set::synth::detect_cross_class_conflicts(fields, field_json, push, pop, remove)?;

    let mut warnings: Vec<SetWarning> = Vec::new();

    let (fields_typed, w) = coerce_kv_slice(fields, force, cfg, doc)?;
    warnings.extend(w);
    let (push_typed, w) = coerce_kv_slice(push, force, cfg, doc)?;
    warnings.extend(w);
    let (pop_typed, w) = coerce_kv_slice(pop, force, cfg, doc)?;
    warnings.extend(w);

    // --field-json: raw JSON; validate against schema unless --force.
    let mut field_json_typed: Vec<(String, Value)> = Vec::new();
    for kv in field_json {
        let (key, raw_json) = crate::set::synth::parse_kv(kv)?;
        let parsed: Value = serde_json::from_str(&raw_json)
            .map_err(|e| anyhow::anyhow!("--field-json value is not valid JSON ({key}): {e}"))?;
        if let Some(ty) = lookup_field_type(cfg, doc, &key) {
            let valid = crate::standards::predicates::frontmatter_type_matches(&parsed, &ty);
            if !valid {
                if !force {
                    anyhow::bail!(
                        "--field-json value for '{key}' does not match schema type '{ty}'"
                    );
                }
                warnings.push(SetWarning::ForceBypass {
                    field: key.clone(),
                    message: format!("--force bypassed type validation for '{key}'"),
                });
            }
        } else {
            warnings.push(SetWarning::UnknownField {
                field: key.clone(),
                message: format!("field '{key}' not declared in schema"),
            });
        }
        field_json_typed.push((key, parsed));
    }

    // --remove: required-field protection.
    for key in remove {
        if !is_required_field(cfg, doc, key) {
            continue;
        }
        if !force {
            anyhow::bail!("cannot remove required field '{key}'; use --force to override");
        }
        warnings.push(SetWarning::ForceBypass {
            field: key.clone(),
            message: format!("--force bypassed required-field protection for '{key}'"),
        });
    }

    // --field and --field-json both feed set/add ops.
    let mut all_fields = fields_typed;
    all_fields.extend(field_json_typed);

    let changes = crate::set::synth::synth_frontmatter_ops_typed(
        current_frontmatter,
        &all_fields,
        &push_typed,
        &pop_typed,
        remove,
    )?;

    Ok(SynthResult { changes, warnings })
}

/// Warn-class check: does the wikilink target resolve to a unique doc in the
/// vault? Empty `matches` → WikilinkUnresolved; >1 → WikilinkAmbiguous. Stem
/// comparison is case-insensitive. Anchor / pipe-alias suffixes are stripped.
///
/// Linear scan over GraphIndex.documents. Atlas-scale (~800 docs) is well
/// under perf budget.
pub fn check_wikilink_resolution(
    index: &vault_core::GraphIndex,
    field: &str,
    wikilink_value: &str,
) -> Vec<SetWarning> {
    let target = wikilink_value
        .strip_prefix("[[")
        .and_then(|s| s.strip_suffix("]]"))
        .unwrap_or(wikilink_value);
    let canonical = target
        .split('#')
        .next()
        .unwrap_or(target)
        .split('|')
        .next()
        .unwrap_or(target)
        .to_lowercase();

    let matches: Vec<&vault_core::Document> = index
        .documents
        .iter()
        .filter(|d| d.stem.to_lowercase() == canonical)
        .collect();

    match matches.len() {
        0 => vec![SetWarning::WikilinkUnresolved {
            field: field.to_string(),
            target: target.to_string(),
        }],
        1 => vec![],
        _ => vec![SetWarning::WikilinkAmbiguous {
            field: field.to_string(),
            target: target.to_string(),
            candidates: matches.iter().map(|d| d.path.to_string()).collect(),
        }],
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
        crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
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

    // ── Task 4.3: synth_with_schema ──────────────────────────────────────────

    fn current_fm(doc: &Document) -> Value {
        doc.frontmatter.as_ref().cloned().unwrap_or(json!({}))
    }

    #[test]
    fn synth_with_schema_coerces_wikilink_field() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["workspace=vault-cli".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(result.changes[0].new_value, Some(json!("[[vault-cli]]")));
    }

    #[test]
    fn synth_with_schema_refuses_invalid_datetime_without_force() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["created=not-a-date".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn synth_with_schema_with_force_writes_invalid_value_verbatim() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["created=not-a-date".to_string()],
            &[],
            &[],
            &[],
            &[],
            true,
        )
        .expect("--force should bypass schema");
        assert_eq!(result.changes[0].new_value, Some(json!("not-a-date")));
        assert!(result
            .warnings
            .iter()
            .any(|w| matches!(w, SetWarning::ForceBypass { .. })));
    }

    #[test]
    fn synth_with_schema_silent_path_uses_light_inference() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["custom_flag=true".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(result.changes[0].new_value, Some(json!(true)));
        assert!(result
            .warnings
            .iter()
            .any(|w| matches!(w, SetWarning::UnknownField { .. })));
    }

    #[test]
    fn remove_refuses_required_field_without_force() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        let fm = json!({"created": "2026-01-01T00:00:00", "kind": "note"});
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &[],
            &[],
            &[],
            &[],
            &["created".to_string()],
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("required"));
    }

    #[test]
    fn remove_with_force_drops_required_field_with_warning() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_field_types();
        let fm = json!({"created": "2026-01-01T00:00:00", "kind": "note"});
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &[],
            &[],
            &[],
            &[],
            &["created".to_string()],
            true,
        )
        .expect("--force should bypass required-field protection");
        assert_eq!(result.changes.len(), 1);
        assert!(result
            .warnings
            .iter()
            .any(|w| matches!(w, SetWarning::ForceBypass { .. })));
    }

    // ── Task 4.4: check_wikilink_resolution ──────────────────────────────────

    fn fixture_index_with_docs(paths: &[&str]) -> (tempfile::TempDir, vault_core::GraphIndex) {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-set-wikilink-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
        for p in paths {
            let path = tmp.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "---\ntype: note\n---\n").unwrap();
        }
        let index = crate::graph::build_index(&root).unwrap();
        (tmp, index)
    }

    #[test]
    fn wikilink_resolution_warns_on_unresolved() {
        let (_tmp, index) = fixture_index_with_docs(&["notes/foo.md", "notes/bar.md"]);
        let warnings = check_wikilink_resolution(&index, "workspace", "[[nonexistent]]");
        assert_eq!(warnings.len(), 1);
        assert!(matches!(warnings[0], SetWarning::WikilinkUnresolved { .. }));
    }

    #[test]
    fn wikilink_resolution_warns_on_ambiguous() {
        let (_tmp, index) = fixture_index_with_docs(&["a/shared.md", "b/shared.md"]);
        let warnings = check_wikilink_resolution(&index, "workspace", "[[shared]]");
        assert_eq!(warnings.len(), 1);
        assert!(matches!(warnings[0], SetWarning::WikilinkAmbiguous { .. }));
    }

    #[test]
    fn wikilink_resolution_no_warning_when_target_resolves_uniquely() {
        let (_tmp, index) = fixture_index_with_docs(&["notes/foo.md"]);
        let warnings = check_wikilink_resolution(&index, "workspace", "[[foo]]");
        assert!(warnings.is_empty());
    }
}
