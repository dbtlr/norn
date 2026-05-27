//! Synthesize a `create_document` RepairPlan from CLI args + schema.
//! Filled in Task 7.4.
//
// `dead_code` silenced: public types + `build_plan` will be wired into the
// orchestrator in Task 7.7. Remove these allows there.
#![allow(dead_code)]

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use serde_json::Value;

// ── Public types ──────────────────────────────────────────────────────────────

/// Fully synthesized plan for creating a single new document.
#[derive(Debug)]
pub struct CreateDocumentPlan {
    /// The single `create_document` PlannedChange.
    pub change: crate::standards::PlannedChange,
    /// Informational warnings (never blocking); shown to the operator.
    pub warnings: Vec<Warning>,
    /// Provenance for each frontmatter field — used by `report::render_json`.
    pub field_sources: Vec<FieldSource>,
}

/// Provenance record for one frontmatter field in the plan.
#[derive(Debug, Clone)]
pub struct FieldSource {
    pub field: String,
    pub value: serde_json::Value,
    pub source: FieldSourceKind,
    /// The rule name that contributed this default, if any.
    pub rule: Option<String>,
}

/// Where a frontmatter field's value originated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSourceKind {
    SchemaDefault,
    OperatorFlag,
    OperatorFlagJson,
}

/// Non-blocking informational warnings emitted by `build_plan`.
#[derive(Debug, Clone)]
pub enum Warning {
    MissingRequiredField {
        field: String,
        rules: Vec<String>,
    },
    UnresolvedWikilink {
        field: String,
        target: String,
    },
    StemCollision {
        stem: String,
        locations: Vec<camino::Utf8PathBuf>,
    },
    PathVariableUnresolved {
        field: String,
        variable: String,
    },
}

/// Hard errors that prevent plan synthesis.
#[derive(Debug, thiserror::Error)]
pub enum SynthError {
    #[error("invalid --field format (expected key=value): {0}")]
    InvalidField(String),
    #[error("invalid --field-json {key}: {message}")]
    InvalidFieldJson { key: String, message: String },
    #[error("substitution failed: {0}")]
    Substitution(String),
    #[error("schema-aware coercion failed for field `{field}`: {message}")]
    Coercion { field: String, message: String },
}

// ── build_plan ────────────────────────────────────────────────────────────────

/// Synthesize a [`CreateDocumentPlan`] from CLI args + compiled schema + optional index.
///
/// # Build sequence
/// 1. Extract path variables from matching rules.
/// 2. Parse operator overrides (`--field`, `--field-json`).
/// 3. Apply schema-aware coercion to operator overrides (unless `--force`).
/// 4. Call `resolve_to_fixpoint` to expand schema defaults.
/// 5. Emit wikilink resolution warnings (requires index).
/// 6. Emit missing-required-field warnings.
/// 7. Emit stem-collision warnings (requires index).
/// 8. Construct the `PlannedChange`.
pub fn build_plan(
    args: &crate::cli::NewArgs,
    cfg: &crate::standards::VaultConfig,
    compiled: &crate::standards::CompiledConfig,
    index: Option<&crate::core::GraphIndex>,
    body: String,
) -> Result<CreateDocumentPlan, SynthError> {
    // ── Step 1: path variable extraction ─────────────────────────────────────
    // Walk compiled rules; for each whose path pattern matches, extract captures.
    // First-rule-wins on collisions.
    let mut path_vars: BTreeMap<String, String> = BTreeMap::new();
    for compiled_rule in &compiled.rules {
        let captures = crate::standards::path_variables(compiled_rule, args.path.as_str());
        for (k, v) in captures {
            path_vars.entry(k).or_insert(v);
        }
    }

    // ── Step 2: operator overrides parsing ────────────────────────────────────
    // Parse --field and --field-json into a typed override map.
    // We collect (field, value, FieldSourceKind) so provenance is preserved.
    let mut raw_overrides: Vec<(String, Value, FieldSourceKind)> = Vec::new();

    for kv in &args.field {
        let (key, value) = split_kv(kv).map_err(|_| SynthError::InvalidField(kv.clone()))?;
        raw_overrides.push((key, Value::String(value), FieldSourceKind::OperatorFlag));
    }

    for kv in &args.field_json {
        let (key, raw_json) = split_kv(kv).map_err(|_| SynthError::InvalidField(kv.clone()))?;
        let parsed: Value =
            serde_json::from_str(&raw_json).map_err(|e| SynthError::InvalidFieldJson {
                key: key.clone(),
                message: e.to_string(),
            })?;
        raw_overrides.push((key, parsed, FieldSourceKind::OperatorFlagJson));
    }

    // ── Step 3: schema-aware coercion of operator overrides ───────────────────
    // For --field (string input), coerce to schema type unless --force.
    // For --field-json, the value is already typed; no string coercion needed.
    //
    // We need to look up field_types from rules matching this path. Since the
    // document doesn't exist yet, we use applicable_rules with path-only matching
    // (frontmatter = None for the first pass — same as resolve_to_fixpoint does).
    let path_only_rules =
        crate::standards::applicable_rules(cfg, compiled, args.path.as_str(), None);

    let mut operator_overrides: BTreeMap<String, Value> = BTreeMap::new();
    let mut operator_sources: Vec<(String, Value, FieldSourceKind)> = Vec::new();

    for (key, value, kind) in raw_overrides {
        let coerced = if kind == FieldSourceKind::OperatorFlag && !args.force {
            // String input — try schema-aware coercion.
            let raw_str = value.as_str().unwrap_or("");
            let field_type = path_only_rules
                .iter()
                .find_map(|(rule, _)| rule.field_types.get(&key))
                .cloned();
            match field_type {
                Some(ty) => {
                    crate::set::validate::coerce_value_for_type(&ty, raw_str).map_err(|e| {
                        SynthError::Coercion {
                            field: key.clone(),
                            message: e.to_string(),
                        }
                    })?
                }
                None => {
                    // Unknown field — fall back to light type inference.
                    crate::set::synth::infer_scalar(raw_str)
                }
            }
        } else {
            // --force, or --field-json (already typed): use as-is.
            value
        };
        operator_overrides.insert(key.clone(), coerced.clone());
        operator_sources.push((key, coerced, kind));
    }

    // ── Step 4: fixpoint resolution ───────────────────────────────────────────
    let (resolved_fm, applied_rule_names) = crate::standards::resolve_to_fixpoint(
        cfg,
        compiled,
        args.path.as_str(),
        &operator_overrides,
        &path_vars,
    )
    .map_err(|e| SynthError::Substitution(e.to_string()))?;

    // Build field_sources from the resolved map.
    // Operator overrides get their declared provenance; everything else is SchemaDefault.
    let operator_keys: BTreeMap<String, (Value, FieldSourceKind)> = operator_sources
        .iter()
        .map(|(k, v, kind)| (k.clone(), (v.clone(), kind.clone())))
        .collect();

    // We need to know which rule contributed each schema default. The fixpoint
    // doesn't return per-field rule attribution, so we do a best-effort scan:
    // first matching rule that declares the field as a default gets credited.
    let mut field_sources: Vec<FieldSource> = Vec::new();
    for (field, value) in &resolved_fm {
        if let Some((op_val, op_kind)) = operator_keys.get(field) {
            field_sources.push(FieldSource {
                field: field.clone(),
                value: op_val.clone(),
                source: op_kind.clone(),
                rule: None,
            });
        } else {
            // Schema default — find the first rule that provided it.
            let rule_name = cfg
                .validate
                .rules
                .iter()
                .find(|r| r.frontmatter_defaults.contains_key(field))
                .and_then(|r| r.name.clone());
            field_sources.push(FieldSource {
                field: field.clone(),
                value: value.clone(),
                source: FieldSourceKind::SchemaDefault,
                rule: rule_name,
            });
        }
    }

    // ── Step 5: wikilink resolution warnings ──────────────────────────────────
    // Only fired when an index is available.
    let mut warnings: Vec<Warning> = Vec::new();

    if let Some(idx) = index {
        for (field, value) in &resolved_fm {
            if let Some(s) = value.as_str() {
                if s.starts_with("[[") && s.ends_with("]]") {
                    let set_warnings =
                        crate::set::validate::check_wikilink_resolution(idx, field, s);
                    for w in set_warnings {
                        match w {
                            crate::set::validate::SetWarning::WikilinkUnresolved {
                                field,
                                target,
                            } => warnings.push(Warning::UnresolvedWikilink { field, target }),
                            crate::set::validate::SetWarning::WikilinkAmbiguous {
                                field,
                                target,
                                ..
                            } => {
                                // Ambiguous is surfaced as UnresolvedWikilink for v1;
                                // a dedicated variant can be added later if the operator
                                // needs to distinguish not-found from multi-match.
                                warnings.push(Warning::UnresolvedWikilink { field, target })
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // ── Step 6: required-field check ─────────────────────────────────────────
    // For each rule that applied (applied_rule_names), check required_frontmatter.
    // Dedupe: if the same field is required by multiple rules, one Warning with all rule names.
    let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for rule in &cfg.validate.rules {
        let rule_name = rule.name.clone().unwrap_or_default();
        if !applied_rule_names.contains(&rule_name) {
            continue;
        }
        for required_field in &rule.required_frontmatter {
            if !resolved_fm.contains_key(required_field) {
                missing
                    .entry(required_field.clone())
                    .or_default()
                    .push(rule_name.clone());
            }
        }
    }
    for (field, rules) in missing {
        warnings.push(Warning::MissingRequiredField { field, rules });
    }

    // ── Step 7: stem collision warning ────────────────────────────────────────
    if let Some(idx) = index {
        let new_stem = args.path.file_stem().unwrap_or("").to_lowercase();
        let collisions: Vec<Utf8PathBuf> = idx
            .documents
            .iter()
            .filter(|d| d.path != args.path)
            .filter(|d| d.stem.to_lowercase() == new_stem)
            .map(|d| d.path.clone())
            .collect();
        if !collisions.is_empty() {
            warnings.push(Warning::StemCollision {
                stem: new_stem,
                locations: collisions,
            });
        }
    }

    // ── Step 8: synthesize PlannedChange ──────────────────────────────────────
    // Payload: { "frontmatter": <map>, "body": <body> }
    let fm_json: serde_json::Map<String, Value> = resolved_fm.into_iter().collect();
    let new_value = serde_json::json!({
        "frontmatter": Value::Object(fm_json),
        "body": body,
    });

    let change_id = derive_change_id(&args.path, "create_document");

    let change = crate::standards::PlannedChange {
        change_id,
        path: args.path.clone(),
        document_hash: String::new(), // no existing hash for a brand-new file
        finding_code: "operator-mutation".to_string(),
        finding_rule: None,
        repair_rule: "vault-new".to_string(),
        operation: "create_document".to_string(),
        field: None,
        expected_old_value: None,
        new_value: Some(new_value),
        destination: None,
        link_risk: None,
        warnings: vec![],
        force: args.force,
    };

    Ok(CreateDocumentPlan {
        change,
        warnings,
        field_sources,
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Split `KEY=VALUE` at the first `=`. Returns Err when no `=` is found.
fn split_kv(raw: &str) -> Result<(String, String), ()> {
    let (k, v) = raw.split_once('=').ok_or(())?;
    if k.is_empty() {
        return Err(());
    }
    Ok((k.to_string(), v.to_string()))
}

/// Derive a stable 8-byte hex change_id from path + operation code.
fn derive_change_id(path: &Utf8PathBuf, code: &str) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(path.as_str().as_bytes());
    h.update(b"\0");
    h.update(code.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standards::{parse_config_compiled, VaultConfig};
    use camino::Utf8Path;

    fn build(yaml: &str) -> (VaultConfig, crate::standards::CompiledConfig) {
        parse_config_compiled(yaml, Utf8Path::new(".norn/config.yaml")).unwrap()
    }

    fn args(path: &str, fields: Vec<&str>) -> crate::cli::NewArgs {
        crate::cli::NewArgs {
            path: path.into(),
            field: fields.iter().map(|s| s.to_string()).collect(),
            field_json: vec![],
            body_from_stdin: false,
            force: false,
            parents: false,
            yes: false,
            dry_run: false,
            format: crate::cli::NewFormat::Records,
        }
    }

    #[test]
    fn synth_happy_path_applies_schema_defaults() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: task-in-workspace
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      required_frontmatter: [type, status, workspace]
      frontmatter_defaults:
        type: task
        status: backlog
        workspace: "[[{{path.workspace}}]]"
"#,
        );
        let a = args("Workspaces/vault-cli/tasks/foo.md", vec![]);
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();

        // Operation
        assert_eq!(plan.change.operation, "create_document");
        assert_eq!(
            plan.change.path.as_str(),
            "Workspaces/vault-cli/tasks/foo.md"
        );

        // Frontmatter populated
        let nv = plan.change.new_value.as_ref().unwrap();
        let fm = &nv["frontmatter"];
        assert_eq!(fm["type"], serde_json::json!("task"));
        assert_eq!(fm["status"], serde_json::json!("backlog"));
        assert_eq!(fm["workspace"], serde_json::json!("[[vault-cli]]"));

        // No warnings expected when all required fields are filled.
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);
    }

    #[test]
    fn synth_operator_overrides_win() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
        status: backlog
"#,
        );
        let a = args("foo.md", vec!["type=custom"]);
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        assert_eq!(fm["type"], serde_json::json!("custom"));
        assert_eq!(fm["status"], serde_json::json!("backlog"));
    }

    #[test]
    fn synth_field_json_parses_arrays() {
        let (cfg, compiled) = build("validate: {}\n");
        let mut a = args("foo.md", vec![]);
        a.field_json = vec![r#"tags=["a","b"]"#.to_string()];
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        assert_eq!(fm["tags"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn synth_missing_required_field_warns() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      required_frontmatter: [type, status]
      frontmatter_defaults:
        type: note
"#,
        );
        let a = args("foo.md", vec![]);
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();
        let missing: Vec<&str> = plan
            .warnings
            .iter()
            .filter_map(|w| match w {
                Warning::MissingRequiredField { field, .. } => Some(field.as_str()),
                _ => None,
            })
            .collect();
        assert!(missing.contains(&"status"), "warnings: {:?}", plan.warnings);
    }

    #[test]
    fn synth_invalid_field_format_errors() {
        let (cfg, compiled) = build("validate: {}\n");
        let mut a = args("foo.md", vec![]);
        a.field = vec!["no_equals_sign".into()];
        let err = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap_err();
        assert!(matches!(err, SynthError::InvalidField(_)), "got: {err:?}");
    }

    #[test]
    fn synth_invalid_field_json_errors() {
        let (cfg, compiled) = build("validate: {}\n");
        let mut a = args("foo.md", vec![]);
        a.field_json = vec!["key={not valid json".into()];
        let err = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap_err();
        assert!(
            matches!(err, SynthError::InvalidFieldJson { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn synth_carries_body_in_new_value() {
        let (cfg, compiled) = build("validate: {}\n");
        let a = args("foo.md", vec![]);
        let plan = build_plan(&a, &cfg, &compiled, None, "# Hello\nbody\n".to_string()).unwrap();
        let nv = plan.change.new_value.as_ref().unwrap();
        assert_eq!(nv["body"].as_str().unwrap(), "# Hello\nbody\n");
    }

    #[test]
    fn synth_records_field_sources() {
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      frontmatter_defaults:
        type: note
"#,
        );
        let a = args("foo.md", vec!["title=My Note"]);
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();

        // Schema-default for `type`, operator-flag for `title`.
        let by_field: std::collections::HashMap<_, _> = plan
            .field_sources
            .iter()
            .map(|fs| (fs.field.clone(), fs.source.clone()))
            .collect();
        assert_eq!(by_field.get("type"), Some(&FieldSourceKind::SchemaDefault));
        assert_eq!(by_field.get("title"), Some(&FieldSourceKind::OperatorFlag));
    }

    // ── Coercion test ─────────────────────────────────────────────────────────

    #[test]
    fn synth_coerces_wikilink_field_on_operator_flag() {
        // A field declared as wikilink type should get auto-wrapped when supplied
        // without brackets via --field.
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      field_types:
        workspace: wikilink
"#,
        );
        let a = args("foo.md", vec!["workspace=vault-cli"]);
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        // Auto-wrapped: "vault-cli" → "[[vault-cli]]"
        assert_eq!(fm["workspace"], serde_json::json!("[[vault-cli]]"));
    }

    #[test]
    fn synth_force_skips_coercion() {
        // With --force, an invalid datetime value should be accepted as-is.
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: r
      match:
        path: "**/*.md"
      field_types:
        created: datetime
"#,
        );
        let mut a = args("foo.md", vec!["created=not-a-date"]);
        a.force = true;
        let plan = build_plan(&a, &cfg, &compiled, None, String::new()).unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        assert_eq!(fm["created"], serde_json::json!("not-a-date"));
    }
}
