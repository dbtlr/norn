//! Schema-aware pre-flight validation for `norn set`.

// These functions are pub for Phase 5 wiring; the binary doesn't call them yet.
#![allow(dead_code)]

use crate::core::Document;
use crate::set::error::SetError;
use crate::standards::PlannedChange;
use crate::standards::VaultConfig;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

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
    /// The post-state document schema resolution ran against (NRN-119). Exposed
    /// so the caller's wikilink-resolution sweep reuses it instead of rebuilding
    /// the same overlay + Document clone.
    pub(crate) effective_doc: Document,
}

/// Look up the declared schema type for `field` on the given document.
/// Returns the type string (e.g. "datetime", "list_of_strings", "wikilink") or
/// None when no matching rule declares a type for the field.
pub fn lookup_field_type(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<String> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(spec) = rule.field_types.get(field) {
            // A type-less extended entry (`{ indexed: bool }`) declares no
            // type — keep looking at subsequent matching rules.
            if let Some(ty) = spec.type_name() {
                return Some(ty.to_string());
            }
        }
    }
    None
}

/// The effective `max_length` bound for `field`'s declared schema type on
/// this document — `Some(n)` for `string`/`list_of_strings` (the declared
/// value, or the default of 64 when unset), `None` for every other type or
/// when no matching rule declares the field.
pub fn lookup_field_max_length(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<u32> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(spec) = rule.field_types.get(field) {
            return spec.effective_max_length();
        }
    }
    None
}

/// Coerce a raw CLI value string into a typed JSON Value matching the declared
/// schema type. Refuses when the input cannot be expressed as the type.
///
/// Wikilink-typed values are auto-wrapped: `norn` becomes `[[norn]]`.
/// Already-bracketed input passes through. Empty-stem wikilinks (`[[]]`) are
/// refused as shape-invalid.
/// `max_length` bounds `string` (the whole value) and each `list_of_strings`
/// element; it is ignored for every other type.
pub fn coerce_value_for_type(
    field_type: &str,
    raw: &str,
    max_length: Option<u32>,
) -> Result<Value> {
    match field_type {
        "datetime" => {
            if crate::standards::predicates::is_datetime_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                Err(SetError::InvalidDatetime {
                    value: raw.to_string(),
                }
                .into())
            }
        }
        "date" => {
            if crate::standards::predicates::is_date_string(raw) {
                Ok(Value::String(raw.to_string()))
            } else {
                Err(SetError::InvalidDate {
                    value: raw.to_string(),
                }
                .into())
            }
        }
        "wikilink" | "wikilink_or_list" => {
            let wrapped = wrap_wikilink(raw);
            if !crate::standards::predicates::is_wikilink_string(&wrapped) {
                return Err(SetError::InvalidWikilink {
                    value: raw.to_string(),
                }
                .into());
            }
            Ok(Value::String(wrapped))
        }
        "list_of_strings" => {
            check_max_length(raw, max_length, "list_of_strings")?;
            Ok(Value::Array(vec![Value::String(raw.to_string())]))
        }
        "string" => {
            check_max_length(raw, max_length, "string")?;
            Ok(Value::String(raw.to_string()))
        }
        "text" => Ok(Value::String(raw.to_string())),
        unknown => Err(SetError::UnknownFieldType {
            field_type: unknown.to_string(),
        }
        .into()),
    }
}

fn check_max_length(raw: &str, max_length: Option<u32>, field_type: &str) -> Result<()> {
    if let Some(bound) = max_length {
        if raw.chars().count() > bound as usize {
            return Err(SetError::ValueTooLong {
                value: raw.to_string(),
                bound,
                field_type: field_type.to_string(),
            }
            .into());
        }
    }
    Ok(())
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

/// Is `field` declared by any rule in `rules` via `field_types`,
/// `allowed_values`, `required_frontmatter`, `field_references`, or
/// `forbidden_frontmatter`?
///
/// This is deliberately separate from [`lookup_field_type`]: that answers "what
/// coercion type does the field have?" and returns `None` for a field governed
/// only by `allowed_values`/`required_frontmatter` (e.g. `status`). Using the
/// type lookup as the "is this field known?" oracle is what produced the
/// spurious `unknown field` warning — a field can be fully schema-declared yet
/// have no special coercion type.
///
/// `field_references` (NRN-37 F2) is a documented field-declaring construct
/// (`docs/rule-shape.md`'s own example: `field_references: { parent: {
/// target_type: [...] } }`) that this check omitted, so a typed-reference-only
/// field (no `field_types`/`allowed_values`/`required_frontmatter` entry) was
/// wrongly flagged unknown.
///
/// `forbidden_frontmatter` (NRN-37 F2) counts too: a forbidden field is known
/// to the schema — it's just disallowed — and already gets the dedicated
/// `frontmatter-forbidden-field` finding. Counting it here as "known"
/// suppresses the redundant unknown-field label so the field surfaces exactly
/// one finding instead of two.
///
/// Takes an already rule-matched slice rather than a `Document` so it can be
/// shared by callers that match rules differently: `is_known_field` below
/// matches against a real `Document` (via `rule_matches`); `norn new`
/// (`new::synth::build_plan`, no document exists yet) matches by path +
/// in-progress frontmatter (via `applicable_rules`). Both feed the same
/// "declared anywhere?" check (NRN-37).
pub fn field_known_in_rules<'a>(
    rules: impl IntoIterator<Item = &'a crate::standards::ValidateRule>,
    field: &str,
) -> bool {
    for rule in rules {
        // A type-less extended entry (`{ indexed: bool }`) contributes only to
        // the index vote — it must not make the field "known" to the schema.
        let has_typed_field_type = rule
            .field_types
            .get(field)
            .is_some_and(|spec| spec.type_name().is_some());
        if has_typed_field_type
            || rule.allowed_values.contains_key(field)
            || rule.required_frontmatter.iter().any(|f| f == field)
            || rule.field_references.contains_key(field)
            || rule.forbidden_frontmatter.iter().any(|f| f == field)
        {
            return true;
        }
    }
    false
}

/// Is `field` known to the schema for this document? True when any matching rule
/// declares it via `field_types`, `allowed_values`, or `required_frontmatter`.
pub fn is_known_field(cfg: &VaultConfig, doc: &Document, field: &str) -> bool {
    let matching = cfg
        .validate
        .rules
        .iter()
        .filter(|rule| crate::standards::engine::rule_matches(doc, rule));
    field_known_in_rules(matching, field)
}

/// The allowed-value set for `field` from the first matching rule that declares
/// one, or `None` when no matching rule constrains the field's values.
pub fn lookup_allowed_values(cfg: &VaultConfig, doc: &Document, field: &str) -> Option<Vec<Value>> {
    for rule in &cfg.validate.rules {
        if !crate::standards::engine::rule_matches(doc, rule) {
            continue;
        }
        if let Some(values) = rule.allowed_values.get(field) {
            return Some(values.clone());
        }
    }
    None
}

/// Does `value` satisfy the `allowed` set? Scalars must match one entry; arrays
/// (e.g. a `list_of_strings` field) require every element to match. Mirrors
/// validate's scalar comparison via `frontmatter_value_matches` (which only
/// compares scalars), so an array is checked element-by-element here.
fn value_in_allowed(value: &Value, allowed: &[Value]) -> bool {
    let matches_one = |v: &Value| {
        allowed
            .iter()
            .any(|a| crate::standards::predicates::frontmatter_value_matches(v, a))
    };
    match value {
        Value::Array(items) => items.iter().all(matches_one),
        scalar => matches_one(scalar),
    }
}

/// Render an allowed-value set for an error message: `a, b, c`.
fn display_allowed(allowed: &[Value]) -> String {
    allowed
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the post-state document for schema resolution: overlay the batch's
/// `--field` / `--field-json` changes onto the document's current frontmatter so
/// rule matching (which keys on `type`/`kind`) sees the INCOMING state (NRN-119).
///
/// Deliberate choices, each closing a review-found hole:
/// - **Base on `current_frontmatter`** (the fresh pre-state the ops actually run
///   against), not the possibly-staler cached `doc.frontmatter`, so schema
///   resolution and value computation agree on the same pre-state.
/// - **Overlay `--field` values as raw strings**, not `infer_scalar`: norn
///   coerces every typed field (the enum fields rules discriminate on) to a
///   string, so the string form models the post-write value; `infer_scalar`
///   would turn `2` / `true` into a number/bool and flip a string-typed match
///   predicate (matching is strict, no cross-type coercion).
/// - **Do NOT apply `--remove` deletions.** Removing a field must not change the
///   schema used to judge whether that field is required, or the removal-
///   protection check would un-match its own rule and silently drop a required
///   field. `--push`/`--pop` are likewise not overlaid; list fields are not
///   rule-match predicates.
pub(crate) fn effective_match_doc(
    doc: &Document,
    current_frontmatter: &Value,
    fields: &[String],
    field_json: &[String],
) -> Document {
    let mut fm = current_frontmatter.as_object().cloned().unwrap_or_default();

    for kv in fields {
        if let Ok((k, raw)) = crate::set::synth::parse_kv(kv) {
            fm.insert(k, Value::String(raw));
        }
    }
    for kv in field_json {
        if let Ok((k, raw)) = crate::set::synth::parse_kv(kv) {
            if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                fm.insert(k, v);
            }
        }
    }

    let mut d = doc.clone();
    d.frontmatter = Some(Value::Object(fm));
    d
}

/// Output type for `coerce_kv_slice`: typed pairs + any emitted warnings.
type CoercedKvs = (Vec<(String, Value)>, Vec<SetWarning>);

/// Coerce one `KEY=raw` slice into typed `(KEY, Value)` pairs.
/// Returns `(typed_pairs, warnings)`.
///
/// When `enforce_allowed_values` is set (the scalar set paths — `--field` and
/// `--field-json`), a coerced value outside its field's `allowed_values` set is
/// refused unless `force`, mirroring how a type mismatch is refused. `--force`
/// writes verbatim and emits a `ForceBypass` warning.
fn coerce_kv_slice(
    raw_kvs: &[String],
    force: bool,
    cfg: &VaultConfig,
    doc: &Document,
    enforce_allowed_values: bool,
    element_wise: bool,
) -> Result<CoercedKvs> {
    let mut out = Vec::new();
    let mut w = Vec::new();
    for kv in raw_kvs {
        let (key, raw) = crate::set::synth::parse_kv(kv)?;
        let coerced = match lookup_field_type(cfg, doc, &key) {
            Some(ty) if !force => {
                let max_length = lookup_field_max_length(cfg, doc, &key);
                // --push / --pop operate on a single list ELEMENT, so coerce a
                // `list_of_strings` field's value as its element type (string)
                // rather than wrapping it into a one-element array — otherwise
                // --pop's drop-value becomes `["x"]` and never matches the
                // string elements (silent no-op), and --push nests the array
                // (NRN-127).
                let effective_ty: &str = if element_wise && ty == "list_of_strings" {
                    "string"
                } else {
                    ty.as_str()
                };
                coerce_value_for_type(effective_ty, &raw, max_length)?
            }
            Some(_) => {
                w.push(SetWarning::ForceBypass {
                    field: key.clone(),
                    message: format!("--force bypassed type validation for '{key}'"),
                });
                Value::String(raw.clone())
            }
            None => {
                // "Known to the schema" is the union of field_types,
                // allowed_values, and required_frontmatter — not field_types
                // alone. Only warn when the field is declared by none of them.
                if !is_known_field(cfg, doc, &key) {
                    w.push(SetWarning::UnknownField {
                        field: key.clone(),
                        message: format!("field '{key}' not declared in schema"),
                    });
                }
                crate::set::synth::infer_scalar(&raw)
            }
        };

        if enforce_allowed_values {
            if let Some(allowed) = lookup_allowed_values(cfg, doc, &key) {
                if !value_in_allowed(&coerced, &allowed) {
                    if !force {
                        return Err(SetError::ValueNotAllowed {
                            field: key.clone(),
                            value: raw.clone(),
                            allowed: display_allowed(&allowed),
                        }
                        .into());
                    }
                    w.push(SetWarning::ForceBypass {
                        field: key.clone(),
                        message: format!("--force bypassed allowed-values validation for '{key}'"),
                    });
                }
            }
        }

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

    // Schema resolution must run against the POST-state document: a single call
    // that changes `type`/`kind` AND sets another field must coerce that field
    // under the incoming type's schema, not the outgoing one (NRN-119). Overlay
    // the batch's field changes onto the document used for rule matching.
    let effective = effective_match_doc(doc, current_frontmatter, fields, field_json);
    let doc = &effective;

    // --field is a scalar set → enforce allowed_values. --push / --pop operate on
    // list elements; per-element allowed_values is not a validated concept today,
    // so they only get the known-field warning suppression, not enforcement.
    let (fields_typed, w) = coerce_kv_slice(fields, force, cfg, doc, true, false)?;
    warnings.extend(w);
    let (push_typed, w) = coerce_kv_slice(push, force, cfg, doc, false, true)?;
    warnings.extend(w);
    let (pop_typed, w) = coerce_kv_slice(pop, force, cfg, doc, false, true)?;
    warnings.extend(w);

    // --field-json: raw JSON; validate against schema unless --force.
    let mut field_json_typed: Vec<(String, Value)> = Vec::new();
    for kv in field_json {
        let (key, raw_json) = crate::set::synth::parse_kv(kv)?;
        let parsed: Value =
            serde_json::from_str(&raw_json).map_err(|e| SetError::FieldJsonInvalid {
                field: key.clone(),
                detail: e.to_string(),
            })?;
        if let Some(ty) = lookup_field_type(cfg, doc, &key) {
            let max_length = lookup_field_max_length(cfg, doc, &key);
            let valid =
                crate::standards::predicates::frontmatter_type_matches(&parsed, &ty, max_length);
            if !valid {
                if !force {
                    return Err(SetError::FieldJsonTypeInvalid {
                        field: key.clone(),
                        field_type: ty.clone(),
                    }
                    .into());
                }
                warnings.push(SetWarning::ForceBypass {
                    field: key.clone(),
                    message: format!("--force bypassed type validation for '{key}'"),
                });
            }
        } else if !is_known_field(cfg, doc, &key) {
            warnings.push(SetWarning::UnknownField {
                field: key.clone(),
                message: format!("field '{key}' not declared in schema"),
            });
        }

        // allowed_values enforcement, same as the scalar --field path.
        if let Some(allowed) = lookup_allowed_values(cfg, doc, &key) {
            if !value_in_allowed(&parsed, &allowed) {
                if !force {
                    return Err(SetError::FieldJsonNotAllowed {
                        field: key.clone(),
                        allowed: display_allowed(&allowed),
                    }
                    .into());
                }
                warnings.push(SetWarning::ForceBypass {
                    field: key.clone(),
                    message: format!("--force bypassed allowed-values validation for '{key}'"),
                });
            }
        }

        field_json_typed.push((key, parsed));
    }

    // --remove: required-field protection.
    for key in remove {
        if !is_required_field(cfg, doc, key) {
            continue;
        }
        if !force {
            return Err(SetError::RequiredFieldRemoved { field: key.clone() }.into());
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

    Ok(SynthResult {
        changes,
        warnings,
        effective_doc: effective,
    })
}

/// Warn-class check: does the wikilink target resolve to a unique doc in the
/// vault? Empty `matches` → WikilinkUnresolved; >1 → WikilinkAmbiguous. Stem
/// comparison is case-insensitive. Anchor / pipe-alias suffixes are stripped.
///
/// Linear scan over GraphIndex.documents. Atlas-scale (~800 docs) is well
/// under perf budget.
pub fn check_wikilink_resolution(
    index: &crate::core::GraphIndex,
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

    let matches: Vec<&crate::core::Document> = index
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

    fn fixture_config_with_bounded_string_fields() -> VaultConfig {
        let yaml = r#"
validate:
  rules:
    - name: note-fields
      match:
        frontmatter:
          kind: note
      field_types:
        project: { type: string, max_length: 8 }
        summary: string
        notes: text
"#;
        crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse")
    }

    fn fixture_config_type_transition() -> VaultConfig {
        let yaml = r#"
validate:
  rules:
    - name: note-base
      match:
        frontmatter:
          kind: note
      field_types:
        workspace: wikilink
        aliases: list_of_strings
    - name: session-log-base
      match:
        frontmatter:
          kind: session-log
      field_types:
        workspace: string
        aliases: list_of_strings
"#;
        crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse")
    }

    #[test]
    fn coercion_uses_post_state_schema_on_type_change() {
        // Setting kind=session-log + workspace=Agents in one call must coerce
        // workspace under the POST-state (session-log → bare string), not the
        // outgoing note-base rule (→ wikilink "[[Agents]]"). NRN-119.
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_type_transition();
        let current = json!({"kind": "note", "title": "Foo"});
        let result = synth_with_schema(
            &cfg,
            &doc,
            &current,
            &["kind=session-log".into(), "workspace=Agents".into()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .expect("synth should succeed");
        let ws = result
            .changes
            .iter()
            .find(|c| c.field.as_deref() == Some("workspace"))
            .expect("workspace change present");
        assert_eq!(ws.new_value, Some(json!("Agents")));
    }

    #[test]
    fn pop_coerces_per_element_not_whole_field_wrap() {
        // --pop aliases=x on a list_of_strings field must drop the "x" element,
        // not the array ["x"] (which never matches → silent no-op). NRN-127.
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_type_transition();
        let current = json!({"kind": "note", "aliases": ["x", "y"]});
        let result = synth_with_schema(
            &cfg,
            &doc,
            &current,
            &[],
            &[],
            &[],
            &["aliases=x".into()],
            &[],
            false,
        )
        .expect("synth should succeed");
        let al = result
            .changes
            .iter()
            .find(|c| c.field.as_deref() == Some("aliases"))
            .expect("pop should produce an aliases change, not a no-op");
        assert_eq!(al.new_value, Some(json!(["y"])));
    }

    #[test]
    fn remove_of_self_matching_required_field_still_protected() {
        // Removing a field that is BOTH a rule's match predicate AND required by
        // that rule must stay protected: the post-state doc used for schema
        // resolution must not pre-apply the removal, or the rule un-matches
        // itself and protection silently evaporates (review finding 4).
        let yaml = r#"
validate:
  rules:
    - name: note-requires-kind
      match:
        frontmatter:
          kind: note
      required_frontmatter:
        - kind
"#;
        let cfg = crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config parses");
        let doc = fixture_doc_kind_note();
        let current = json!({"kind": "note", "title": "Foo"});
        let err = synth_with_schema(
            &cfg,
            &doc,
            &current,
            &[],
            &[],
            &[],
            &[],
            &["kind".into()],
            false,
        )
        .expect_err("removing a required field must be refused without --force");
        assert!(
            err.to_string().contains("required field 'kind'"),
            "expected required-field refusal, got: {err}"
        );
    }

    #[test]
    fn field_overlay_preserves_string_predicate_matching() {
        // A numeric-LOOKING but string-typed match predicate must keep matching
        // when set to a bare numeric value: the overlay uses the raw string, not
        // infer_scalar (which would make it Number and un-match). Review finding 2.
        let yaml = r#"
validate:
  rules:
    - name: edition-two
      match:
        frontmatter:
          edition: "2"
      field_types:
        ref: wikilink
"#;
        let cfg = crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config parses");
        let doc = fixture_doc_kind_note();
        let current = json!({"kind": "note", "edition": "2"});
        // Set edition=2 (bare) + ref=Foo in one batch. edition stays string "2",
        // the rule still matches, so ref is coerced as a wikilink.
        let result = synth_with_schema(
            &cfg,
            &doc,
            &current,
            &["edition=2".into(), "ref=Foo".into()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .expect("synth succeeds");
        let r = result
            .changes
            .iter()
            .find(|c| c.field.as_deref() == Some("ref"))
            .expect("ref change present");
        assert_eq!(r.new_value, Some(json!("[[Foo]]")));
    }

    #[test]
    fn push_coerces_per_element_not_nested_array() {
        // --push aliases=z must append the scalar "z", not the array ["z"]. NRN-127.
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_type_transition();
        let current = json!({"kind": "note", "aliases": ["x"]});
        let result = synth_with_schema(
            &cfg,
            &doc,
            &current,
            &[],
            &[],
            &["aliases=z".into()],
            &[],
            &[],
            false,
        )
        .expect("synth should succeed");
        let al = result
            .changes
            .iter()
            .find(|c| c.field.as_deref() == Some("aliases"))
            .expect("aliases change present");
        assert_eq!(al.new_value, Some(json!(["x", "z"])));
    }

    #[test]
    fn lookup_field_max_length_returns_declared_override() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_bounded_string_fields();
        assert_eq!(lookup_field_max_length(&cfg, &doc, "project"), Some(8));
    }

    #[test]
    fn lookup_field_max_length_returns_default_when_unset() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_bounded_string_fields();
        assert_eq!(lookup_field_max_length(&cfg, &doc, "summary"), Some(64));
    }

    #[test]
    fn lookup_field_max_length_none_for_text() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_bounded_string_fields();
        assert_eq!(lookup_field_max_length(&cfg, &doc, "notes"), None);
    }

    #[test]
    fn lookup_field_max_length_none_for_undeclared_field() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_bounded_string_fields();
        assert_eq!(lookup_field_max_length(&cfg, &doc, "madeup"), None);
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
        let out = coerce_value_for_type("datetime", raw, None).expect("should accept");
        assert_eq!(out, json!("2026-05-25T12:00:00"));
    }

    #[test]
    fn coerce_value_refuses_invalid_datetime() {
        assert!(coerce_value_for_type("datetime", "not a date", None).is_err());
    }

    #[test]
    fn coerce_value_wraps_bare_stem_in_wikilink_brackets() {
        let out = coerce_value_for_type("wikilink", "norn", None).expect("should wrap");
        assert_eq!(out, json!("[[norn]]"));
    }

    #[test]
    fn coerce_value_passes_through_already_bracketed_wikilink() {
        let out = coerce_value_for_type("wikilink", "[[norn]]", None).expect("should accept");
        assert_eq!(out, json!("[[norn]]"));
    }

    #[test]
    fn coerce_value_refuses_empty_wikilink_brackets() {
        // wrapping "" yields "[[]]" which is shape-invalid per is_wikilink_string.
        assert!(coerce_value_for_type("wikilink", "", None).is_err());
    }

    #[test]
    fn coerce_value_for_list_of_strings_wraps_single_string() {
        let out = coerce_value_for_type("list_of_strings", "single", None).expect("should wrap");
        assert_eq!(out, json!(["single"]));
    }

    #[test]
    fn coerce_value_refuses_unknown_field_type() {
        assert!(coerce_value_for_type("some_unknown", "x", None).is_err());
    }

    #[test]
    fn coerce_value_string_within_bound_accepted() {
        let out = coerce_value_for_type("string", "abc", Some(3)).expect("should accept");
        assert_eq!(out, json!("abc"));
    }

    #[test]
    fn coerce_value_string_over_bound_refused() {
        assert!(coerce_value_for_type("string", "abcd", Some(3)).is_err());
    }

    #[test]
    fn coerce_value_string_with_no_bound_accepts_any_length() {
        let long = "x".repeat(1000);
        let out = coerce_value_for_type("string", &long, None).expect("should accept");
        assert_eq!(out, json!(long));
    }

    #[test]
    fn coerce_value_text_ignores_bound() {
        let long = "x".repeat(1000);
        let out = coerce_value_for_type("text", &long, Some(64)).expect("text is unbounded");
        assert_eq!(out, json!(long));
    }

    #[test]
    fn coerce_value_list_of_strings_over_bound_refused() {
        assert!(coerce_value_for_type("list_of_strings", "abcd", Some(3)).is_err());
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
            &["workspace=norn".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(result.changes[0].new_value, Some(json!("[[norn]]")));
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
    fn synth_with_schema_refuses_string_over_max_length() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_bounded_string_fields();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["project=this-is-way-too-long".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn synth_with_schema_accepts_string_within_max_length() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_bounded_string_fields();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["project=short".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(result.changes[0].new_value, Some(json!("short")));
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

    fn fixture_index_with_docs(paths: &[&str]) -> (tempfile::TempDir, crate::core::GraphIndex) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-set-wikilink-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
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

    // ── allowed_values: known-field oracle + write-time enforcement ───────────

    /// A field governed by `allowed_values` + `required_frontmatter` but with no
    /// `field_types` entry — exactly the `status` shape that surfaced the bug.
    fn fixture_config_with_allowed_values() -> VaultConfig {
        let yaml = r#"
validate:
  rules:
    - name: note-status
      match:
        frontmatter:
          kind: note
      allowed_values:
        status:
          - backlog
          - completed
      required_frontmatter:
        - status
"#;
        crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse")
    }

    #[test]
    fn is_known_field_true_for_allowed_values_only_field() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_allowed_values();
        // Known to the schema via allowed_values + required_frontmatter...
        assert!(is_known_field(&cfg, &doc, "status"));
        // ...even though it has no field_type (the conflation that caused the bug).
        assert_eq!(lookup_field_type(&cfg, &doc, "status"), None);
        // A genuinely undeclared field is still unknown.
        assert!(!is_known_field(&cfg, &doc, "madeup"));
    }

    /// NRN-37 F2: `field_references` is a documented field-declaring
    /// construct (`docs/rule-shape.md`'s own example uses `field_references:
    /// { parent: { target_type: [...] } }`), but `field_known_in_rules`
    /// checked only `field_types`/`allowed_values`/`required_frontmatter` —
    /// so `set` (which shares this seam with `norn new`) would also
    /// false-flag a `field_references`-only field as unknown.
    #[test]
    fn is_known_field_true_for_field_references_only_field() {
        let doc = fixture_doc_kind_note();
        let yaml = r#"
validate:
  rules:
    - name: note-parent
      match:
        frontmatter:
          kind: note
      field_references:
        parent:
          target_type: [phase]
"#;
        let cfg = crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse");
        assert!(is_known_field(&cfg, &doc, "parent"));
        // No field_type declared for `parent` (the conflation that caused the bug).
        assert_eq!(lookup_field_type(&cfg, &doc, "parent"), None);
        // A genuinely undeclared field is still unknown.
        assert!(!is_known_field(&cfg, &doc, "madeup"));
    }

    #[test]
    fn synth_no_unknown_field_warning_for_allowed_values_field() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_allowed_values();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["status=completed".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(result.changes[0].new_value, Some(json!("completed")));
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| matches!(w, SetWarning::UnknownField { .. })),
            "status is schema-declared via allowed_values; no UnknownField expected"
        );
    }

    #[test]
    fn synth_refuses_value_outside_allowed_set_without_force() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_allowed_values();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["status=bogus".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not allowed"));
    }

    #[test]
    fn synth_force_bypasses_allowed_values_with_warning() {
        let doc = fixture_doc_kind_note();
        let cfg = fixture_config_with_allowed_values();
        let fm = current_fm(&doc);
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["status=bogus".to_string()],
            &[],
            &[],
            &[],
            &[],
            true,
        )
        .expect("--force should bypass allowed-values validation");
        assert_eq!(result.changes[0].new_value, Some(json!("bogus")));
        assert!(result
            .warnings
            .iter()
            .any(|w| matches!(w, SetWarning::ForceBypass { .. })));
    }

    // ── Fix 1: creatable-rule path-blindness in norn set ─────────────────────

    /// A config with a single creatable (target-based) task rule that constrains
    /// `allowed_values.status` to `[todo, done]` on paths matching
    /// `Workspaces/*/tasks/*.md`.
    fn fixture_config_with_creatable_task_rule() -> VaultConfig {
        let yaml = r#"
validate:
  rules:
    - name: task
      target: "Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"
      allowed_values:
        status:
          - todo
          - done
"#;
        crate::standards::parse_config(yaml, camino::Utf8Path::new("fixture.yaml"))
            .expect("config should parse")
    }

    fn doc_at(path: &str) -> Document {
        Document {
            path: Utf8PathBuf::from(path),
            stem: camino::Utf8Path::new(path)
                .file_stem()
                .unwrap_or("")
                .to_string(),
            hash: "h".into(),
            frontmatter: Some(json!({})),
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    /// The creatable task rule must NOT govern documents outside its target
    /// path hierarchy — setting `status=anything` on a note must be allowed.
    #[test]
    fn creatable_rule_does_not_govern_non_matching_path() {
        let cfg = fixture_config_with_creatable_task_rule();
        // This path does NOT match Workspaces/*/tasks/*.md
        let doc = doc_at("Workspaces/x/notes/foo.md");
        let fm = current_fm(&doc);

        // Without Fix 1 this would error because the rule (with match.path==None)
        // would apply to every document, treating "anything" as outside [todo, done].
        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["status=anything".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        );
        assert!(
            result.is_ok(),
            "creatable task rule must not constrain a non-matching path; got: {:?}",
            result.err()
        );
    }

    /// The same creatable task rule MUST govern documents that do match its
    /// target path hierarchy — setting a disallowed status must be refused.
    #[test]
    fn creatable_rule_governs_matching_path() {
        let cfg = fixture_config_with_creatable_task_rule();
        // This path DOES match Workspaces/*/tasks/*.md
        let doc = doc_at("Workspaces/x/tasks/foo.md");
        let fm = current_fm(&doc);

        let result = synth_with_schema(
            &cfg,
            &doc,
            &fm,
            &["status=bogus".to_string()],
            &[],
            &[],
            &[],
            &[],
            false,
        );
        assert!(
            result.is_err(),
            "creatable task rule must refuse a disallowed status on a matching path"
        );
        assert!(result.unwrap_err().to_string().contains("not allowed"));
    }
}
