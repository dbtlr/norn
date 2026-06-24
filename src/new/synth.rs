//! Synthesize a `create_document` RepairPlan from CLI args + schema.
//! Filled in Task 7.4.

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
    AmbiguousWikilink {
        field: String,
        target: String,
        candidates: Vec<camino::Utf8PathBuf>,
    },
    StemCollision {
        stem: String,
        locations: Vec<camino::Utf8PathBuf>,
    },
    // Reserved for future path-variable substitution diagnostics.
    #[allow(dead_code)]
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
/// # Parameters
/// - `doc_path`: the resolved concrete document path (vault-relative).
/// - `extra_path_vars`: caller-supplied variable bag (e.g. from `--var KEY=VALUE` in
///   rule/inbox mode). These are merged into the path-vars extracted from pattern
///   captures so that frontmatter defaults can reference `{{var.KEY}}` holes.
///
/// # Build sequence
/// 1. Extract path variables from matching rules; merge `extra_path_vars` (no override).
/// 2. Parse operator overrides (`--field`, `--field-json`).
/// 3. Apply schema-aware coercion to operator overrides (unless `--force`).
/// 4. Call `resolve_to_fixpoint` to expand schema defaults.
/// 5. Emit wikilink resolution warnings (requires index).
/// 6. Emit missing-required-field warnings.
/// 7. Emit stem-collision warnings (requires index).
/// 8. Construct the `PlannedChange`.
pub fn build_plan(
    args: &crate::cli::NewArgs,
    doc_path: &camino::Utf8Path,
    extra_path_vars: &BTreeMap<String, String>,
    cfg: &crate::standards::VaultConfig,
    compiled: &crate::standards::CompiledConfig,
    index: Option<&crate::core::GraphIndex>,
    body: String,
) -> Result<CreateDocumentPlan, SynthError> {
    let doc_path_buf = doc_path.to_owned();

    // ── Step 1: path variable extraction ─────────────────────────────────────
    // Walk compiled rules; for each whose path pattern matches, extract captures.
    // First-rule-wins on collisions.
    // Then merge extra_path_vars (caller-supplied via --var) so that
    // frontmatter defaults referencing {{var.KEY}} resolve correctly when the
    // path is generated from a rule target rather than parsed from a real path.
    let mut path_vars: BTreeMap<String, String> = BTreeMap::new();
    for compiled_rule in &compiled.rules {
        let captures = crate::standards::path_variables(compiled_rule, doc_path.as_str());
        for (k, v) in captures {
            path_vars.entry(k).or_insert(v);
        }
    }
    // Merge extra vars (--var bag). Pattern-derived captures take precedence;
    // extra_path_vars fill in any remaining holes.
    for (k, v) in extra_path_vars {
        path_vars.entry(k.clone()).or_insert_with(|| v.clone());
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
        crate::standards::applicable_rules(cfg, compiled, doc_path.as_str(), None);

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
        doc_path.as_str(),
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

    // Per-field provenance: credit the first rule that BOTH matches this
    // document (by path + resolved frontmatter) AND declares the field as a
    // default. Scanning every rule regardless of match would mis-credit a field
    // to a rule whose match never applies here — e.g. a tasks/ document credited
    // to a notes-only rule that merely declares the same default earlier in the
    // config. We re-derive the matched rule set against the fully-resolved
    // frontmatter (config order preserved) and search only within it.
    let resolved_fm_value = serde_json::Value::Object(
        resolved_fm
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    );
    let matched_rules = crate::standards::applicable_rules(
        cfg,
        compiled,
        doc_path.as_str(),
        Some(&resolved_fm_value),
    );

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
            // Schema default — credit the first MATCHING rule that declares it.
            let rule_name = matched_rules
                .iter()
                .find(|(r, _)| r.frontmatter_defaults.contains_key(field))
                .and_then(|(r, _)| r.name.clone());
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
                                candidates,
                            } => warnings.push(Warning::AmbiguousWikilink {
                                field,
                                target,
                                candidates: candidates
                                    .into_iter()
                                    .map(camino::Utf8PathBuf::from)
                                    .collect(),
                            }),
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
        let new_stem = doc_path.file_stem().unwrap_or("").to_lowercase();
        let collisions: Vec<Utf8PathBuf> = idx
            .documents
            .iter()
            .filter(|d| d.path != doc_path_buf)
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

    let change_id = derive_change_id(&doc_path_buf, "create_document");

    let change = crate::standards::PlannedChange {
        change_id,
        path: doc_path_buf,
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
        parents: false,
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

    fn args(path: &str, fields: Vec<&str>) -> (crate::cli::NewArgs, camino::Utf8PathBuf) {
        let a = crate::cli::NewArgs {
            path: Some(path.into()),
            as_rule: None,
            title: None,
            var: vec![],
            field: fields.iter().map(|s| s.to_string()).collect(),
            field_json: vec![],
            body_from_stdin: false,
            force: false,
            parents: false,
            yes: false,
            dry_run: false,
            format: crate::cli::NewFormat::Records,
        };
        let p = camino::Utf8PathBuf::from(path);
        (a, p)
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
        let (a, p) = args("Workspaces/norn/tasks/foo.md", vec![]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();

        // Operation
        assert_eq!(plan.change.operation, "create_document");
        assert_eq!(plan.change.path.as_str(), "Workspaces/norn/tasks/foo.md");

        // Frontmatter populated
        let nv = plan.change.new_value.as_ref().unwrap();
        let fm = &nv["frontmatter"];
        assert_eq!(fm["type"], serde_json::json!("task"));
        assert_eq!(fm["status"], serde_json::json!("backlog"));
        assert_eq!(fm["workspace"], serde_json::json!("[[norn]]"));

        // No warnings expected when all required fields are filled.
        assert!(plan.warnings.is_empty(), "warnings: {:?}", plan.warnings);
    }

    #[test]
    fn synth_default_provenance_credits_matching_rule_not_first_declared() {
        // A notes-only rule is declared BEFORE the tasks rule and also declares
        // `created`. For a tasks/ path the notes rule does not match, so `created`
        // must be credited to the rule that actually governs the path
        // (task-folder), not the earlier-declared note-folder.
        let (cfg, compiled) = build(
            r#"
validate:
  rules:
    - name: note-folder
      match:
        path: "Workspaces/{{workspace}}/notes/**/*.md"
      frontmatter_defaults:
        created: "2026-01-01T00:00"
    - name: task-folder
      match:
        path: "Workspaces/{{workspace}}/tasks/*.md"
      frontmatter_defaults:
        type: task
        created: "2026-01-01T00:00"
"#,
        );
        let (a, p) = args("Workspaces/norn/tasks/foo.md", vec![]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();
        let created_src = plan
            .field_sources
            .iter()
            .find(|fs| fs.field == "created")
            .expect("created field present");
        assert_eq!(
            created_src.rule.as_deref(),
            Some("task-folder"),
            "created should be credited to the matching rule, not the first-declared one"
        );
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
        let (a, p) = args("foo.md", vec!["type=custom"]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        assert_eq!(fm["type"], serde_json::json!("custom"));
        assert_eq!(fm["status"], serde_json::json!("backlog"));
    }

    #[test]
    fn synth_field_json_parses_arrays() {
        let (cfg, compiled) = build("validate: {}\n");
        let (mut a, p) = args("foo.md", vec![]);
        a.field_json = vec![r#"tags=["a","b"]"#.to_string()];
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();
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
        let (a, p) = args("foo.md", vec![]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();
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
        let (mut a, p) = args("foo.md", vec![]);
        a.field = vec!["no_equals_sign".into()];
        let err = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap_err();
        assert!(matches!(err, SynthError::InvalidField(_)), "got: {err:?}");
    }

    #[test]
    fn synth_invalid_field_json_errors() {
        let (cfg, compiled) = build("validate: {}\n");
        let (mut a, p) = args("foo.md", vec![]);
        a.field_json = vec!["key={not valid json".into()];
        let err = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap_err();
        assert!(
            matches!(err, SynthError::InvalidFieldJson { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn synth_carries_body_in_new_value() {
        let (cfg, compiled) = build("validate: {}\n");
        let (a, p) = args("foo.md", vec![]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            "# Hello\nbody\n".to_string(),
        )
        .unwrap();
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
        let (a, p) = args("foo.md", vec!["title=My Note"]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();

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
        let (a, p) = args("foo.md", vec!["workspace=norn"]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        // Auto-wrapped: "norn" → "[[norn]]"
        assert_eq!(fm["workspace"], serde_json::json!("[[norn]]"));
    }

    #[test]
    fn synth_routes_ambiguous_wikilink_to_its_own_variant() {
        // A wikilink stem resolving to >1 doc must map to Warning::AmbiguousWikilink
        // (carrying the candidates), NOT collapse into UnresolvedWikilink.
        let tmp = tempfile::Builder::new()
            .prefix("norn-new-ambiguous-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        for p in ["a/shared.md", "b/shared.md"] {
            let path = tmp.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "---\ntype: note\n---\n").unwrap();
        }
        let index = crate::graph::build_index(&root).unwrap();

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
        let (a, p) = args("foo.md", vec!["workspace=shared"]);
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            Some(&index),
            String::new(),
        )
        .unwrap();
        let ambiguous = plan
            .warnings
            .iter()
            .find_map(|w| match w {
                Warning::AmbiguousWikilink { candidates, .. } => Some(candidates),
                _ => None,
            })
            .expect("expected an AmbiguousWikilink warning");
        assert_eq!(ambiguous.len(), 2, "both shared-stem docs are candidates");
        assert!(!plan
            .warnings
            .iter()
            .any(|w| matches!(w, Warning::UnresolvedWikilink { .. })));
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
        let (mut a, p) = args("foo.md", vec!["created=not-a-date"]);
        a.force = true;
        let plan = build_plan(
            &a,
            &p,
            &BTreeMap::new(),
            &cfg,
            &compiled,
            None,
            String::new(),
        )
        .unwrap();
        let fm = &plan.change.new_value.as_ref().unwrap()["frontmatter"];
        assert_eq!(fm["created"], serde_json::json!("not-a-date"));
    }
}
