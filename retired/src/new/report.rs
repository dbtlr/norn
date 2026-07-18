//! `norn new` output report: serde-symmetric `NewReport` + JSON/records rendering.
//!
//! (NRN-230 PR B) Mirrors `set::report::SetReport`: `NewReport` is
//! `Serialize` + `Deserialize` so a future routing seam (PR C) can rebuild it
//! from a routed `vault.new`'s wire `structuredContent` and render it through
//! the SAME `render_json`/`render_records` the direct CLI path uses — the
//! load-bearing routed↔direct isomorphism (ADR 0005). `vault.new` also
//! returns `{ "report": <NewReport> }` (via `serde_json::to_value`), replacing
//! the old hand-built, asymmetric JSON envelope that only `render_json` could
//! produce and nothing could deserialize.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::new::synth::{CreateDocumentPlan, FieldSourceKind, Warning};

pub const NEW_REPORT_SCHEMA_VERSION: u32 = 2;

/// Serde-symmetric report for `norn new` / `vault.new` (NRN-230). One success
/// shape (any of the three creation modes, dry-run or applied) and one
/// refusal shape (a coded preflight/resolve/synth refusal) share this single
/// type — `render_json`/`render_records` both consume it, and a routing seam
/// (PR C) can rebuild it from a routed MCP response and render it through the
/// SAME renderers the direct path uses, for byte-identical output.
///
/// Field order here is for readability only: `render_json` serializes through
/// an intermediate `serde_json::Value` (a `BTreeMap`-backed `Map`, no
/// `preserve_order`), which re-sorts object keys alphabetically regardless of
/// this struct's declared field order — the same alphabetical order the prior
/// hand-built `json!` envelope produced. This is what keeps CLI output
/// byte-identical to before this refactor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewReport {
    pub schema_version: u32,
    pub operation: String,
    /// Vault-relative path of the created (or to-be-created) document. Always
    /// present on success (all three modes); `None` on a refusal whose coded
    /// error names no path (e.g. `unknown-rule`, before any path resolves) —
    /// omitted rather than an ambiguous empty string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub applied: bool,
    /// Machine-branchable apply outcome (NRN-220): `applied` on success and
    /// dry-run; `refused` when a coded refusal was captured (the code lives in
    /// `error`). Reuses the shared `ApplyOutcome` vocabulary so a consumer
    /// branches identically to the cascade tools.
    pub outcome: crate::apply_report::ApplyOutcome,
    /// Trace ID shared by every telemetry event emitted for this invocation.
    /// Empty on dry-run/preview/refusal (no event stream is persisted).
    pub trace_id: String,
    pub frontmatter_created: Vec<FrontmatterCreated>,
    pub body_bytes: usize,
    pub warnings: Vec<Warning>,
    /// NRN-101: for an unresolved `{{seq}}` target, a non-binding predicted id
    /// (filesystem max+1 at preview time). The real id is allocated at apply
    /// under the lock and can differ (concurrent create).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub predicted_path: Option<String>,
    /// Structured refusal envelope (kebab `code` + `message` + optional
    /// `path`) when `outcome` is `refused`; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::apply_report::ApplyError>,
}

/// Provenance record for one frontmatter field the plan scaffolded — the
/// serde-symmetric counterpart of `synth::FieldSource` (which carries a
/// non-serializable `PlannedChange` reference elsewhere in the plan and isn't
/// itself part of the wire report).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrontmatterCreated {
    pub field: String,
    pub value: Value,
    pub source: FieldSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
}

impl NewReport {
    /// Build a minimal refusal report (NRN-220): a coded preflight/resolve/
    /// synth refusal captured on the MCP path. Nothing was created, so the
    /// rich success detail (frontmatter, body sizing) is empty; `error`
    /// carries the stable machine code a consumer branches on. `path` mirrors
    /// `error.path` — present when the coded error names one, `None` (and so
    /// omitted from the wire) otherwise.
    pub fn refused(error: crate::apply_report::ApplyError) -> Self {
        NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: error.path.clone(),
            applied: false,
            outcome: crate::apply_report::ApplyOutcome::Refused,
            trace_id: String::new(),
            frontmatter_created: Vec::new(),
            body_bytes: 0,
            warnings: Vec::new(),
            predicted_path: None,
            error: Some(error),
        }
    }
}

/// Build a `NewReport` from the synthesized plan + apply-time facts.
///
/// `trace_id` is the telemetry trace ID for the invocation; pass `""` on
/// dry-run/preview paths where no event stream is persisted. `predicted_path`
/// is the NRN-101 non-binding `{{seq}}` prediction, `None` when the target has
/// no unresolved `{{seq}}`.
pub fn build_report(
    plan: &CreateDocumentPlan,
    path: &str,
    applied: bool,
    body_bytes: usize,
    trace_id: &str,
    predicted_path: Option<&str>,
) -> NewReport {
    NewReport {
        schema_version: NEW_REPORT_SCHEMA_VERSION,
        operation: "new".to_string(),
        path: Some(path.to_string()),
        applied,
        outcome: crate::apply_report::ApplyOutcome::Applied,
        trace_id: trace_id.to_string(),
        frontmatter_created: plan
            .field_sources
            .iter()
            .map(|fs| FrontmatterCreated {
                field: fs.field.clone(),
                value: fs.value.clone(),
                source: fs.source.clone(),
                rule: fs.rule.clone(),
            })
            .collect(),
        body_bytes,
        warnings: plan.warnings.clone(),
        predicted_path: predicted_path.map(|s| s.to_string()),
        error: None,
    }
}

// ── JSON envelope ────────────────────────────────────────────────────────────

/// Render a `NewReport` as a pretty-printed JSON envelope — byte-identical to
/// what the prior hand-built `json!` envelope produced for every success/
/// dry-run/refusal shape (see the struct doc for why field-declaration order
/// doesn't matter here).
pub fn render_json(report: &NewReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&serde_json::to_value(report)?)
}

fn source_kind_label(kind: &FieldSourceKind) -> &'static str {
    match kind {
        FieldSourceKind::SchemaDefault => "schema-default",
        FieldSourceKind::OperatorFlag => "operator-flag",
        FieldSourceKind::OperatorFlagJson => "operator-flag-json",
    }
}

// ── TTY records block ───────────────────────────────────────────────────────

/// Render a `NewReport` as a human-readable records block.
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
pub fn render_records(report: &NewReport) -> String {
    let mut out = String::new();

    // Label column width — "warnings" is the longest label (8 chars).
    const LABEL_W: usize = 11;

    macro_rules! row {
        ($label:expr, $value:expr) => {
            out.push_str(&format!("{:<LABEL_W$}{}\n", $label, $value));
        };
    }

    let path = report.path.as_deref().unwrap_or("");
    row!("path", path);
    // NRN-101: non-binding predicted id for an unresolved `{{seq}}` target.
    if let Some(pred) = &report.predicted_path {
        row!("predicted", format!("≈ {pred} (allocated at apply)"));
    }
    row!("operation", &report.operation);
    row!("applied", if report.applied { "true" } else { "false" });

    // Field rows
    if report.frontmatter_created.is_empty() {
        row!("fields", "none");
    } else {
        // Compute max field-name width for sub-column alignment.
        let max_field_w = report
            .frontmatter_created
            .iter()
            .map(|fc| fc.field.len())
            .max()
            .unwrap_or(0);

        for (i, fc) in report.frontmatter_created.iter().enumerate() {
            let value_repr = value_repr(&fc.value);
            let provenance = match &fc.rule {
                Some(rule) => format!("({}, {})", source_kind_label(&fc.source), rule),
                None => format!("({})", source_kind_label(&fc.source)),
            };
            let field_cell = format!("{:<width$}", fc.field, width = max_field_w);
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
    row!("body", format!("{} bytes", report.body_bytes));

    // Warnings rows
    if report.warnings.is_empty() {
        row!("warnings", "none");
    } else {
        let labels: Vec<String> = report.warnings.iter().map(warning_label).collect();
        row!("warnings", labels[0]);
        for label in &labels[1..] {
            out.push_str(&format!("{:<LABEL_W$}{}\n", "", label));
        }
    }

    // Dry-run next-step hint — mirrors set::report::render_records convention.
    if !report.applied {
        out.push('\n');
        out.push_str("Apply with --yes\n");
    }

    out
}

fn value_repr(v: &Value) -> String {
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
        Warning::UnknownField { field } => format!("unknown field: {field}"),
        Warning::ValidationFinding { code, message } => format!("{code}: {message}"),
        Warning::TitleIgnored { title } => {
            format!("title-ignored: --title '{title}' has no effect with an explicit path")
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
        let report = build_report(&plan, "Workspaces/foo/tasks/bar.md", true, 0, "", None);
        let out = render_json(&report).unwrap();
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
        let report = build_report(&plan, "p.md", true, 0, "", None);
        let out = render_json(&report).unwrap();
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
        let report = build_report(&plan, "p.md", true, 0, "", None);
        let out = render_json(&report).unwrap();
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
        let report = build_report(&plan, "p.md", true, 0, "", None);
        let out = render_json(&report).unwrap();
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
        let report = build_report(&plan, "p.md", false, 0, "", None);
        let out = render_json(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["applied"], serde_json::json!(false));
    }

    #[test]
    fn body_bytes_threaded_through() {
        let plan = plan_with_fields(vec![]);
        let report = build_report(&plan, "p.md", true, 1234, "", None);
        let out = render_json(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["body_bytes"], serde_json::json!(1234));
    }

    // ── Golden-string pins (NRN-230 PR B review note) ─────────────────────────
    //
    // The CLI's byte-identical JSON output relies on `render_json` serializing
    // through an intermediate `serde_json::Value`, whose Map is BTreeMap-backed
    // ONLY while the workspace resolves `serde_json` WITHOUT the additive
    // `preserve_order` feature. Any future dependency enabling that feature
    // (features unify workspace-wide) would silently flip the wire to
    // struct-declaration order — and the round-trip tests would shift along
    // with it, catching nothing. These two goldens pin the exact rendered
    // bytes (alphabetical keys, 2-space pretty indent) for one representative
    // success report and one refusal report, so an order flip fails loudly.

    #[test]
    fn golden_success_envelope_is_byte_exact() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("Workspaces/foo/tasks/bar.md".to_string()),
            applied: true,
            outcome: crate::apply_report::ApplyOutcome::Applied,
            // Fixed stand-in for the (normally random) telemetry trace id.
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            frontmatter_created: vec![
                FrontmatterCreated {
                    field: "type".to_string(),
                    value: serde_json::json!("task"),
                    source: FieldSourceKind::SchemaDefault,
                    rule: Some("task-rule".to_string()),
                },
                FrontmatterCreated {
                    field: "title".to_string(),
                    value: serde_json::json!("My Note"),
                    source: FieldSourceKind::OperatorFlag,
                    rule: None,
                },
            ],
            body_bytes: 42,
            warnings: vec![Warning::UnknownField {
                field: "staus".to_string(),
            }],
            predicted_path: None,
            error: None,
        };
        let expected = r#"{
  "applied": true,
  "body_bytes": 42,
  "frontmatter_created": [
    {
      "field": "type",
      "rule": "task-rule",
      "source": "schema-default",
      "value": "task"
    },
    {
      "field": "title",
      "source": "operator-flag",
      "value": "My Note"
    }
  ],
  "operation": "new",
  "outcome": "applied",
  "path": "Workspaces/foo/tasks/bar.md",
  "schema_version": 2,
  "trace_id": "0123456789abcdef0123456789abcdef",
  "warnings": [
    {
      "field": "staus",
      "kind": "unknown-field"
    }
  ]
}"#;
        assert_eq!(render_json(&report).unwrap(), expected);
    }

    #[test]
    fn golden_refusal_envelope_is_byte_exact() {
        let report = NewReport::refused(crate::apply_report::ApplyError {
            code: "destination-exists".to_string(),
            message: "destination already exists (use --force to overwrite): exists.md".to_string(),
            path: Some("exists.md".to_string()),
        });
        let expected = r#"{
  "applied": false,
  "body_bytes": 0,
  "error": {
    "code": "destination-exists",
    "message": "destination already exists (use --force to overwrite): exists.md",
    "path": "exists.md"
  },
  "frontmatter_created": [],
  "operation": "new",
  "outcome": "refused",
  "path": "exists.md",
  "schema_version": 2,
  "trace_id": "",
  "warnings": []
}"#;
        assert_eq!(render_json(&report).unwrap(), expected);
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
        let report = build_report(&p, "Workspaces/foo/tasks/bar.md", true, 0, "", None);
        let out = render_records(&report);
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
        let report = build_report(&p, "p.md", true, 0, "", None);
        let out = render_records(&report);
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
        let report = build_report(&p, "p.md", true, 1234, "", None);
        let out = render_records(&report);
        let s = strip_ansi(&out);
        assert!(s.contains("body"));
        assert!(s.contains("1234"));
    }

    #[test]
    fn renders_no_warnings_state() {
        let p = plan(vec![], vec![]);
        let report = build_report(&p, "p.md", true, 0, "", None);
        let out = render_records(&report);
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
        let report = build_report(&p, "p.md", true, 0, "", None);
        let out = render_records(&report);
        let s = strip_ansi(&out);
        assert!(s.contains("missing-required-field") || s.contains("status"));
        assert!(s.contains("unresolved-wikilink") || s.contains("missing-stem"));
    }

    #[test]
    fn dry_run_emits_apply_hint() {
        let p = plan(vec![], vec![]);
        let report = build_report(&p, "p.md", false, 0, "", None);
        let out = render_records(&report);
        let s = strip_ansi(&out);
        assert!(s.contains("--yes"), "dry-run should suggest --yes");
    }

    #[test]
    fn applied_omits_apply_hint() {
        let p = plan(vec![], vec![]);
        let report = build_report(&p, "p.md", true, 0, "", None);
        let out = render_records(&report);
        let s = strip_ansi(&out);
        assert!(!s.contains("--yes"), "applied run should NOT suggest --yes");
    }
}

// ── NRN-230: serde round-trip property tests ───────────────────────────────
//
// For every representative report shape the system can emit, prove
// `from_value(to_value(r)) == r` AND that rendering the round-tripped report
// is byte-identical to rendering the original — the acceptance bar for a
// "serde-symmetric" report (not just "serializes fine one-way").
#[cfg(test)]
mod round_trip_tests {
    use super::*;
    use crate::apply_report::{ApplyError, ApplyOutcome};
    use camino::Utf8PathBuf;

    /// Assert the fixed-point property for one `NewReport`: serializing then
    /// deserializing yields an equal value, AND both renderers produce
    /// byte-identical output for the original vs. the round-tripped report.
    fn assert_round_trips(report: &NewReport) {
        let value = serde_json::to_value(report).expect("serialize to Value");
        let round_tripped: NewReport =
            serde_json::from_value(value).expect("deserialize back to NewReport");
        assert_eq!(
            &round_tripped, report,
            "round-tripped report must equal the original"
        );

        let original_json = render_json(report).unwrap();
        let round_tripped_json = render_json(&round_tripped).unwrap();
        assert_eq!(
            original_json, round_tripped_json,
            "render_json must be byte-identical across the round-trip"
        );

        let original_records = render_records(report);
        let round_tripped_records = render_records(&round_tripped);
        assert_eq!(
            original_records, round_tripped_records,
            "render_records must be byte-identical across the round-trip"
        );
    }

    fn frontmatter(
        field: &str,
        value: Value,
        source: FieldSourceKind,
        rule: Option<&str>,
    ) -> FrontmatterCreated {
        FrontmatterCreated {
            field: field.to_string(),
            value,
            source,
            rule: rule.map(str::to_string),
        }
    }

    /// Mode A (explicit path): success, applied, schema-default + operator
    /// provenance mixed, one warning.
    #[test]
    fn mode_a_success_round_trips() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("notes/mode-a.md".to_string()),
            applied: true,
            outcome: ApplyOutcome::Applied,
            trace_id: "ec9fb3d7111a87cae58b13945af2bf7b".to_string(),
            frontmatter_created: vec![
                frontmatter(
                    "type",
                    serde_json::json!("note"),
                    FieldSourceKind::SchemaDefault,
                    Some("any"),
                ),
                frontmatter(
                    "title",
                    serde_json::json!("My Note"),
                    FieldSourceKind::OperatorFlag,
                    None,
                ),
            ],
            body_bytes: 42,
            warnings: vec![Warning::TitleIgnored {
                title: "Ignored".to_string(),
            }],
            predicted_path: None,
            error: None,
        };
        assert_round_trips(&report);
    }

    /// Mode B (rule-targeted): dry-run, tags via operator-flag-json.
    #[test]
    fn mode_b_dry_run_round_trips() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("Workspaces/norn/tasks/fix-it.md".to_string()),
            applied: false,
            outcome: ApplyOutcome::Applied,
            trace_id: String::new(),
            frontmatter_created: vec![frontmatter(
                "tags",
                serde_json::json!(["a", "b"]),
                FieldSourceKind::OperatorFlagJson,
                None,
            )],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: None,
            error: None,
        };
        assert_round_trips(&report);
    }

    /// Mode C (inbox fallback): applied, with a stem-collision + ambiguous-
    /// wikilink warning pair (multi-path Vec fields).
    #[test]
    fn mode_c_inbox_with_path_warnings_round_trips() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("Inbox/my-title.md".to_string()),
            applied: true,
            outcome: ApplyOutcome::Applied,
            trace_id: "abc123".to_string(),
            frontmatter_created: vec![],
            body_bytes: 0,
            warnings: vec![
                Warning::StemCollision {
                    stem: "foo".to_string(),
                    locations: vec![Utf8PathBuf::from("a/foo.md"), Utf8PathBuf::from("b/foo.md")],
                },
                Warning::AmbiguousWikilink {
                    field: "workspace".to_string(),
                    target: "norn".to_string(),
                    candidates: vec![
                        Utf8PathBuf::from("Workspaces/norn-a.md"),
                        Utf8PathBuf::from("Workspaces/norn-b.md"),
                    ],
                },
            ],
            predicted_path: None,
            error: None,
        };
        assert_round_trips(&report);
    }

    /// Dry-run with an unresolved `{{seq}}` target: `predicted_path` present
    /// (NRN-101).
    #[test]
    fn dry_run_with_predicted_path_round_trips() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("tasks/MMR-{{seq}}.md".to_string()),
            applied: false,
            outcome: ApplyOutcome::Applied,
            trace_id: String::new(),
            frontmatter_created: vec![frontmatter(
                "type",
                serde_json::json!("task"),
                FieldSourceKind::SchemaDefault,
                Some("task"),
            )],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: Some("tasks/MMR-1.md".to_string()),
            error: None,
        };
        assert_round_trips(&report);
    }

    /// Refusal shape: coded error WITH a path (e.g. `destination-exists`).
    #[test]
    fn refusal_with_path_round_trips() {
        let report = NewReport::refused(ApplyError {
            code: "destination-exists".to_string(),
            message: "destination already exists: exists.md".to_string(),
            path: Some("exists.md".to_string()),
        });
        assert_round_trips(&report);
    }

    /// Refusal shape: coded error WITHOUT a path (e.g. resolve-time
    /// `unknown-rule`, before any path resolves) — `path` must round-trip as
    /// `None` (omitted from the wire), not an empty string.
    #[test]
    fn refusal_without_path_round_trips() {
        let report = NewReport::refused(ApplyError {
            code: "unknown-rule".to_string(),
            message: "unknown rule `bogus-rule`".to_string(),
            path: None,
        });
        assert_round_trips(&report);
        assert!(report.path.is_none());
        let value = serde_json::to_value(&report).unwrap();
        assert!(
            value.get("path").is_none(),
            "refusal with no error.path must omit `path` entirely: {value}"
        );
    }

    /// NRN-114: the empty-frontmatter case — a schema that scaffolds nothing
    /// (no rule matched, or a match with no defaults) — `frontmatter_created`
    /// is an empty Vec, not absent.
    #[test]
    fn empty_frontmatter_case_round_trips() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("unscaffolded.md".to_string()),
            applied: true,
            outcome: ApplyOutcome::Applied,
            trace_id: "deadbeef".to_string(),
            frontmatter_created: vec![],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: None,
            error: None,
        };
        assert_round_trips(&report);
    }

    /// Every fixed `Warning` variant, for the guard + round-trip tests below.
    /// Keep in sync with the `Warning` enum (minus `ValidationFinding`, the
    /// dynamic catch-all).
    fn all_fixed_warning_variants() -> Vec<Warning> {
        vec![
            Warning::MissingRequiredField {
                field: "status".to_string(),
                rules: vec!["r1".to_string(), "r2".to_string()],
            },
            Warning::UnresolvedWikilink {
                field: "workspace".to_string(),
                target: "missing-stem".to_string(),
            },
            Warning::AmbiguousWikilink {
                field: "workspace".to_string(),
                target: "norn".to_string(),
                candidates: vec![Utf8PathBuf::from("a.md"), Utf8PathBuf::from("b.md")],
            },
            Warning::StemCollision {
                stem: "foo".to_string(),
                locations: vec![Utf8PathBuf::from("a/foo.md"), Utf8PathBuf::from("b/foo.md")],
            },
            Warning::PathVariableUnresolved {
                field: "workspace".to_string(),
                variable: "workspace".to_string(),
            },
            Warning::UnknownField {
                field: "staus".to_string(),
            },
            Warning::TitleIgnored {
                title: "Ignored Title".to_string(),
            },
        ]
    }

    /// GUARD (NRN-230 PR B review note): the `Warning` wire encoding
    /// disambiguates a dynamic `ValidationFinding` from the fixed variants by
    /// `message`-field PRESENCE — so no fixed variant may ever serialize a
    /// `message` key. A future fixed variant that carried one would silently
    /// misroute to `ValidationFinding` on deserialize (its own fields dropped),
    /// and only this test would catch it before the wire did.
    #[test]
    fn no_fixed_warning_variant_serializes_a_message_key() {
        for w in all_fixed_warning_variants() {
            let value = serde_json::to_value(&w).expect("serialize warning");
            let obj = value.as_object().expect("warning serializes as object");
            assert!(
                !obj.contains_key("message"),
                "fixed variant must NOT serialize a `message` key (it is the \
                 ValidationFinding disambiguator): {w:?} -> {value}"
            );
        }
    }

    /// Every fixed `Warning` variant round-trips, individually.
    #[test]
    fn every_fixed_warning_variant_round_trips() {
        for w in all_fixed_warning_variants() {
            let value = serde_json::to_value(&w).expect("serialize warning");
            let back: Warning = serde_json::from_value(value).expect("deserialize warning");
            assert_eq!(back, w, "warning must round-trip: {w:?}");
        }
    }

    /// A dynamic `ValidationFinding` catch-all round-trips, INCLUDING a code
    /// this build has never seen before (a hypothetical future `norn
    /// validate` finding) — the whole point of the message-presence
    /// disambiguation over a fixed `kind` allow-list.
    #[test]
    fn dynamic_validation_finding_round_trips_including_unknown_codes() {
        let known = Warning::ValidationFinding {
            code: "value-not-allowed".to_string(),
            message: "field `status` has disallowed value `someday`".to_string(),
        };
        let unknown_future_code = Warning::ValidationFinding {
            code: "some-future-finding-code-2027".to_string(),
            message: "a finding this build has never heard of".to_string(),
        };
        // Adversarial: a dynamic code that collides with a FIXED variant's own
        // `kind` literal. The `message` field must still win the
        // disambiguation, per the documented rule.
        let colliding_code = Warning::ValidationFinding {
            code: "unknown-field".to_string(),
            message: "a validate finding whose code happens to read unknown-field".to_string(),
        };
        for w in [known, unknown_future_code, colliding_code] {
            let value = serde_json::to_value(&w).expect("serialize warning");
            let back: Warning = serde_json::from_value(value).expect("deserialize warning");
            assert_eq!(back, w, "dynamic ValidationFinding must round-trip: {w:?}");
        }
    }

    /// A full report carrying the dynamic-code warning round-trips too,
    /// through the whole `NewReport` (not just the bare `Warning`).
    #[test]
    fn report_with_dynamic_validation_finding_round_trips() {
        let report = NewReport {
            schema_version: NEW_REPORT_SCHEMA_VERSION,
            operation: "new".to_string(),
            path: Some("foo.md".to_string()),
            applied: true,
            outcome: ApplyOutcome::Applied,
            trace_id: "trace".to_string(),
            frontmatter_created: vec![],
            body_bytes: 0,
            warnings: vec![Warning::ValidationFinding {
                code: "frontmatter-forbidden-field".to_string(),
                message: "field `legacy_field` is forbidden".to_string(),
            }],
            predicted_path: None,
            error: None,
        };
        assert_round_trips(&report);
    }
}
