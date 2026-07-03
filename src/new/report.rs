//! JSON envelope + TTY records rendering for `norn new`.
//! Filled in Task 7.5 / 7.6.
//
// `dead_code` silenced: public functions will be wired into the orchestrator
// in Task 7.7. Remove this allow there.
#![allow(dead_code)]

use serde_json::{json, Value};

use crate::new::synth::{CreateDocumentPlan, FieldSourceKind, Warning};

// ── Task 7.5: JSON envelope ───────────────────────────────────────────────────

/// Render the `norn new` result as a pretty-printed JSON envelope.
///
/// The envelope schema_version tracks the envelope shape itself. `trace_id` is
/// the telemetry trace ID for the invocation (empty on dry-run/preview, where
/// no event stream is persisted).
pub fn render_json(
    plan: &CreateDocumentPlan,
    path: &str,
    applied: bool,
    body_bytes: usize,
    trace_id: &str,
    predicted_path: Option<&str>,
) -> Result<String, serde_json::Error> {
    let mut envelope = json!({
        "schema_version": 2,
        "operation": "new",
        "path": path,
        "applied": applied,
        "trace_id": trace_id,
        "frontmatter_created": plan.field_sources.iter().map(|fs| {
            let mut entry = serde_json::Map::new();
            entry.insert("field".into(), Value::String(fs.field.clone()));
            entry.insert("value".into(), fs.value.clone());
            entry.insert("source".into(), Value::String(source_kind_label(&fs.source).into()));
            if let Some(rule) = &fs.rule {
                entry.insert("rule".into(), Value::String(rule.clone()));
            }
            Value::Object(entry)
        }).collect::<Vec<_>>(),
        "body_bytes": body_bytes,
        "warnings": plan.warnings.iter().map(warning_to_json).collect::<Vec<_>>(),
    });
    // NRN-101: for an unresolved `{{seq}}` target, surface a non-binding
    // predicted id alongside the honest unresolved `path`. The real id is
    // allocated at apply under the lock and can differ (concurrent create).
    if let Some(pred) = predicted_path {
        if let Some(obj) = envelope.as_object_mut() {
            obj.insert("predicted_path".into(), Value::String(pred.to_string()));
        }
    }
    serde_json::to_string_pretty(&envelope)
}

fn source_kind_label(kind: &FieldSourceKind) -> &'static str {
    match kind {
        FieldSourceKind::SchemaDefault => "schema-default",
        FieldSourceKind::OperatorFlag => "operator-flag",
        FieldSourceKind::OperatorFlagJson => "operator-flag-json",
    }
}

fn warning_to_json(w: &Warning) -> Value {
    match w {
        Warning::MissingRequiredField { field, rules } => json!({
            "kind": "missing-required-field",
            "field": field,
            "rules": rules,
        }),
        Warning::UnresolvedWikilink { field, target } => json!({
            "kind": "unresolved-wikilink",
            "field": field,
            "target": target,
        }),
        Warning::AmbiguousWikilink {
            field,
            target,
            candidates,
        } => json!({
            "kind": "ambiguous-wikilink",
            "field": field,
            "target": target,
            "candidates": candidates.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        }),
        Warning::StemCollision { stem, locations } => json!({
            "kind": "stem-collision",
            "stem": stem,
            "locations": locations.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        }),
        Warning::PathVariableUnresolved { field, variable } => json!({
            "kind": "path-variable-unresolved",
            "field": field,
            "variable": variable,
        }),
    }
}

// ── Task 7.6: TTY records block ───────────────────────────────────────────────

/// Render the `norn new` result as a human-readable records block.
///
/// Shape (mirrors `set::report::render_records` conventions):
/// ```text
/// path        Workspaces/foo/tasks/bar.md
/// operation   new
/// applied     true
/// fields      type      = task          (schema-default, task-rule)
///             title     = My Note       (operator-flag)
/// body        0 bytes
/// warnings    none
/// ```
pub fn render_records(
    plan: &CreateDocumentPlan,
    path: &str,
    applied: bool,
    body_bytes: usize,
    predicted_path: Option<&str>,
) -> String {
    let mut out = String::new();

    // Label column width — "warnings" is the longest label (8 chars).
    const LABEL_W: usize = 11;

    macro_rules! row {
        ($label:expr, $value:expr) => {
            out.push_str(&format!("{:<LABEL_W$}{}\n", $label, $value));
        };
    }

    row!("path", path);
    // NRN-101: non-binding predicted id for an unresolved `{{seq}}` target.
    if let Some(pred) = predicted_path {
        row!("predicted", format!("≈ {pred} (allocated at apply)"));
    }
    row!("operation", "new");
    row!("applied", if applied { "true" } else { "false" });

    // Field rows
    if plan.field_sources.is_empty() {
        row!("fields", "none");
    } else {
        // Compute max field-name width for sub-column alignment.
        let max_field_w = plan
            .field_sources
            .iter()
            .map(|fs| fs.field.len())
            .max()
            .unwrap_or(0);

        for (i, fs) in plan.field_sources.iter().enumerate() {
            let value_repr = value_repr(&fs.value);
            let provenance = match &fs.rule {
                Some(rule) => format!("({}, {})", source_kind_label(&fs.source), rule),
                None => format!("({})", source_kind_label(&fs.source)),
            };
            let field_cell = format!("{:<width$}", fs.field, width = max_field_w);
            let row_body = format!("{} = {}  {}", field_cell, value_repr, provenance);
            if i == 0 {
                row!("fields", row_body);
            } else {
                // Continuation lines: blank label column
                out.push_str(&format!("{:<LABEL_W$}{}\n", "", row_body));
            }
        }
    }

    // Body bytes row
    row!("body", format!("{} bytes", body_bytes));

    // Warnings rows
    if plan.warnings.is_empty() {
        row!("warnings", "none");
    } else {
        let labels: Vec<String> = plan.warnings.iter().map(warning_label).collect();
        row!("warnings", labels[0]);
        for label in &labels[1..] {
            out.push_str(&format!("{:<LABEL_W$}{}\n", "", label));
        }
    }

    // Dry-run next-step hint — mirrors set::report::render_records convention.
    if !applied {
        out.push('\n');
        out.push_str("Apply with --yes\n");
    }

    out
}

fn value_repr(v: &serde_json::Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn warning_label(w: &Warning) -> String {
    match w {
        Warning::MissingRequiredField { field, rules } => {
            format!(
                "missing-required-field: {} (rules: {})",
                field,
                rules.join(", ")
            )
        }
        Warning::UnresolvedWikilink { field, target } => {
            format!("unresolved-wikilink: {} → {}", field, target)
        }
        Warning::AmbiguousWikilink {
            field,
            target,
            candidates,
        } => {
            format!(
                "ambiguous-wikilink: {} → \"{}\" (candidates: {})",
                field,
                target,
                candidates
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Warning::StemCollision { stem, locations } => {
            format!("stem-collision: {} ({} locations)", stem, locations.len())
        }
        Warning::PathVariableUnresolved { field, variable } => {
            format!("path-variable-unresolved: {} (var: {})", field, variable)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod render_json_tests {
    use super::*;
    use crate::new::synth::{CreateDocumentPlan, FieldSource, FieldSourceKind, Warning};
    use camino::Utf8PathBuf;

    fn plan_with_fields(fields: Vec<FieldSource>) -> CreateDocumentPlan {
        CreateDocumentPlan {
            change: crate::standards::PlannedChange {
                change_id: "abc12345".into(),
                path: "test/foo.md".into(),
                document_hash: "".into(),
                finding_code: "imperative-create".into(),
                finding_rule: None,
                repair_rule: "vault-new".into(),
                operation: "create_document".into(),
                field: None,
                expected_old_value: None,
                new_value: Some(serde_json::json!({"frontmatter": {}, "body": ""})),
                destination: None,
                link_risk: None,
                warnings: vec![],
                force: false,
                parents: false,
            },
            warnings: vec![],
            field_sources: fields,
        }
    }

    #[test]
    fn envelope_basic_shape() {
        let plan = plan_with_fields(vec![FieldSource {
            field: "type".into(),
            value: serde_json::json!("task"),
            source: FieldSourceKind::SchemaDefault,
            rule: Some("task-rule".into()),
        }]);
        let out = render_json(&plan, "Workspaces/foo/tasks/bar.md", true, 0, "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["schema_version"], serde_json::json!(2));
        assert_eq!(v["operation"], serde_json::json!("new"));
        assert_eq!(v["path"], serde_json::json!("Workspaces/foo/tasks/bar.md"));
        assert_eq!(v["applied"], serde_json::json!(true));
        assert_eq!(v["body_bytes"], serde_json::json!(0));
        assert!(v["warnings"].is_array());
    }

    #[test]
    fn frontmatter_created_has_source_provenance() {
        let plan = plan_with_fields(vec![
            FieldSource {
                field: "type".into(),
                value: serde_json::json!("task"),
                source: FieldSourceKind::SchemaDefault,
                rule: Some("r1".into()),
            },
            FieldSource {
                field: "title".into(),
                value: serde_json::json!("My Note"),
                source: FieldSourceKind::OperatorFlag,
                rule: None,
            },
        ]);
        let out = render_json(&plan, "p.md", true, 0, "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let fc = v["frontmatter_created"].as_array().unwrap();
        assert_eq!(fc.len(), 2);

        let type_entry = fc.iter().find(|e| e["field"] == "type").unwrap();
        assert_eq!(type_entry["value"], serde_json::json!("task"));
        assert_eq!(type_entry["source"], serde_json::json!("schema-default"));
        assert_eq!(type_entry["rule"], serde_json::json!("r1"));

        let title_entry = fc.iter().find(|e| e["field"] == "title").unwrap();
        assert_eq!(title_entry["source"], serde_json::json!("operator-flag"));
        assert!(title_entry.get("rule").is_none() || title_entry["rule"].is_null());
    }

    #[test]
    fn field_json_source_serializes_kebab() {
        let plan = plan_with_fields(vec![FieldSource {
            field: "tags".into(),
            value: serde_json::json!(["a", "b"]),
            source: FieldSourceKind::OperatorFlagJson,
            rule: None,
        }]);
        let out = render_json(&plan, "p.md", true, 0, "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["frontmatter_created"][0]["source"],
            serde_json::json!("operator-flag-json")
        );
    }

    #[test]
    fn warnings_emit_with_kebab_kind() {
        let mut plan = plan_with_fields(vec![]);
        plan.warnings = vec![
            Warning::MissingRequiredField {
                field: "status".into(),
                rules: vec!["r1".into()],
            },
            Warning::UnresolvedWikilink {
                field: "workspace".into(),
                target: "missing-stem".into(),
            },
            Warning::StemCollision {
                stem: "foo".into(),
                locations: vec![Utf8PathBuf::from("a/foo.md"), Utf8PathBuf::from("b/foo.md")],
            },
            Warning::PathVariableUnresolved {
                field: "workspace".into(),
                variable: "workspace".into(),
            },
        ];
        let out = render_json(&plan, "p.md", true, 0, "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let warnings = v["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 4);

        let kinds: Vec<&str> = warnings
            .iter()
            .map(|w| w["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.contains(&"missing-required-field"));
        assert!(kinds.contains(&"unresolved-wikilink"));
        assert!(kinds.contains(&"stem-collision"));
        assert!(kinds.contains(&"path-variable-unresolved"));

        let stem_warning = warnings
            .iter()
            .find(|w| w["kind"] == "stem-collision")
            .unwrap();
        let locs = stem_warning["locations"].as_array().unwrap();
        assert_eq!(locs.len(), 2);
    }

    #[test]
    fn dry_run_envelope_has_applied_false() {
        let plan = plan_with_fields(vec![]);
        let out = render_json(&plan, "p.md", false, 0, "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["applied"], serde_json::json!(false));
    }

    #[test]
    fn body_bytes_threaded_through() {
        let plan = plan_with_fields(vec![]);
        let out = render_json(&plan, "p.md", true, 1234, "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["body_bytes"], serde_json::json!(1234));
    }
}

#[cfg(test)]
mod render_records_tests {
    use super::*;
    use crate::new::synth::{CreateDocumentPlan, FieldSource, FieldSourceKind, Warning};

    fn plan(fields: Vec<FieldSource>, warnings: Vec<Warning>) -> CreateDocumentPlan {
        CreateDocumentPlan {
            change: crate::standards::PlannedChange {
                change_id: "abc12345".into(),
                path: "test/foo.md".into(),
                document_hash: "".into(),
                finding_code: "imperative-create".into(),
                finding_rule: None,
                repair_rule: "vault-new".into(),
                operation: "create_document".into(),
                field: None,
                expected_old_value: None,
                new_value: Some(serde_json::json!({"frontmatter": {}, "body": ""})),
                destination: None,
                link_risk: None,
                warnings: vec![],
                force: false,
                parents: false,
            },
            warnings,
            field_sources: fields,
        }
    }

    fn strip_ansi(s: &str) -> String {
        // Strip CSI sequences for test assertions.
        let re = regex::Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap();
        re.replace_all(s, "").to_string()
    }

    #[test]
    fn renders_path_operation_applied_labels() {
        let p = plan(vec![], vec![]);
        let out = render_records(&p, "Workspaces/foo/tasks/bar.md", true, 0, None);
        let s = strip_ansi(&out);
        assert!(s.contains("path"));
        assert!(s.contains("Workspaces/foo/tasks/bar.md"));
        assert!(s.contains("operation"));
        assert!(s.contains("new"));
        assert!(s.contains("applied"));
        assert!(s.contains("true"));
    }

    #[test]
    fn renders_each_field_with_provenance() {
        let fields = vec![
            FieldSource {
                field: "type".into(),
                value: serde_json::json!("task"),
                source: FieldSourceKind::SchemaDefault,
                rule: Some("task-rule".into()),
            },
            FieldSource {
                field: "title".into(),
                value: serde_json::json!("My Note"),
                source: FieldSourceKind::OperatorFlag,
                rule: None,
            },
        ];
        let p = plan(fields, vec![]);
        let out = render_records(&p, "p.md", true, 0, None);
        let s = strip_ansi(&out);
        assert!(s.contains("type"));
        assert!(s.contains("task"));
        assert!(s.contains("schema-default"));
        assert!(s.contains("task-rule"));
        assert!(s.contains("title"));
        assert!(s.contains("My Note"));
        assert!(s.contains("operator-flag"));
    }

    #[test]
    fn renders_body_bytes() {
        let p = plan(vec![], vec![]);
        let out = render_records(&p, "p.md", true, 1234, None);
        let s = strip_ansi(&out);
        assert!(s.contains("body"));
        assert!(s.contains("1234"));
    }

    #[test]
    fn renders_no_warnings_state() {
        let p = plan(vec![], vec![]);
        let out = render_records(&p, "p.md", true, 0, None);
        let s = strip_ansi(&out);
        assert!(s.contains("warnings"));
        assert!(s.contains("none") || s.contains("0"));
    }

    #[test]
    fn renders_warnings_when_present() {
        let warnings = vec![
            Warning::MissingRequiredField {
                field: "status".into(),
                rules: vec!["r1".into()],
            },
            Warning::UnresolvedWikilink {
                field: "workspace".into(),
                target: "missing-stem".into(),
            },
        ];
        let p = plan(vec![], warnings);
        let out = render_records(&p, "p.md", true, 0, None);
        let s = strip_ansi(&out);
        assert!(s.contains("missing-required-field") || s.contains("status"));
        assert!(s.contains("unresolved-wikilink") || s.contains("missing-stem"));
    }

    #[test]
    fn dry_run_emits_apply_hint() {
        let p = plan(vec![], vec![]);
        let out = render_records(&p, "p.md", false, 0, None);
        let s = strip_ansi(&out);
        assert!(s.contains("--yes"), "dry-run should suggest --yes");
    }

    #[test]
    fn applied_omits_apply_hint() {
        let p = plan(vec![], vec![]);
        let out = render_records(&p, "p.md", true, 0, None);
        let s = strip_ansi(&out);
        assert!(!s.contains("--yes"), "applied run should NOT suggest --yes");
    }
}
