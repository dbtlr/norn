//! The `set` execute seam: schema-aware frontmatter mutation + wholesale body
//! replacement, built as a `MigrationPlan` against the warm cache and applied
//! through the shared `apply_migration_plan` executor.
//!
//! The synth
//! side produces BOTH the wire `FrontmatterChange` report rows AND the typed
//! frontmatter ops the plan applies; every clean pre-write decline returns a
//! `Refused` report, never a bare `Err`.

use std::collections::{BTreeMap, BTreeSet};

use super::coerce::{self, CoerceError};
use super::{owner_index_options, MutationExecution};
use crate::apply::{apply_migration_plan, ApplyContext};
use crate::domain::{Document, GraphIndex};
use crate::standards::VaultConfig;
use norn_wire::{ApplyOutcome, OpStatus};
use norn_wire::{
    CodedError, FrontmatterChange, MutationOutcome, MutationWarning, SetParams, SetReport,
};
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde_json::{Map, Value};

/// Execute a `set`: forecast (`confirm == false`) or apply (`confirm == true`).
pub fn execute(
    cache: &crate::cache::Cache,
    config: Option<&crate::standards::VaultConfig>,
    params: &norn_wire::SetParams,
    _today: &str,
    sink: &mut crate::telemetry::EventSink,
) -> anyhow::Result<MutationExecution<norn_wire::SetReport>> {
    let default_config = VaultConfig::default();
    let cfg = config.unwrap_or(&default_config);

    let index = cache.load_graph_index()?;
    let vault_root = cache.vault_root().to_string();

    // ── Target resolution ────────────────────────────────────────────────────
    // Refusal prose is end-user contract: `doc not found: <target>` for a
    // miss; the resolver's candidate list for an ambiguous stem.
    let target_path = match crate::target::resolve_target(&index, &params.target) {
        crate::target::TargetResolution::Resolved(p) => p,
        crate::target::TargetResolution::NotFound => {
            let (code, msg) = crate::target::target_refusal(
                crate::target::TargetRefusalFamily::NotFound,
                format!("doc not found: {}", params.target),
            );
            return Ok(refused(params.target.clone(), code, msg, None));
        }
        crate::target::TargetResolution::Ambiguous(candidates) => {
            let (code, msg) = crate::target::target_refusal(
                crate::target::TargetRefusalFamily::Ambiguous,
                format!(
                    "ambiguous document stem: {}; candidates: {}",
                    params.target,
                    candidates
                        .iter()
                        .map(|path| path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
            return Ok(refused(params.target.clone(), code, msg, None));
        }
    };
    let target_str = target_path.to_string();

    let doc = index
        .documents
        .iter()
        .find(|d| d.path == target_path)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("resolved target not in index: {target_path}"))?;

    // Current frontmatter: absent / empty (null) → empty mapping; a present
    // non-mapping (a list/scalar top level) → refuse.
    let current_fm = match &doc.frontmatter {
        Some(v) if v.is_object() => v.clone(),
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(_) => {
            return Ok(refused(
                target_str,
                "frontmatter-not-mapping",
                "frontmatter is not a top-level mapping",
                Some(target_path.to_string()),
            ));
        }
    };

    // ── Schema-aware synth (frontmatter) ─────────────────────────────────────
    let synthed = match synth(cfg, &index, &doc, &current_fm, params) {
        Ok(s) => s,
        Err(e) => {
            return Ok(refused(
                target_str,
                e.code(),
                e.to_string(),
                Some(target_path.to_string()),
            ));
        }
    };

    // ── Build the MigrationPlan ──────────────────────────────────────────────
    let mut operations: Vec<MigrationOp> = Vec::new();
    for op in &synthed.ops {
        let mut fields = Map::new();
        fields.insert("path".into(), Value::String(target_str.clone()));
        fields.insert("field".into(), Value::String(op.field.clone()));
        if let Some(nv) = &op.new_value {
            fields.insert("new_value".into(), nv.clone());
        }
        if let Some(old) = &op.expected_old {
            fields.insert("expected_old_value".into(), old.clone());
        }
        operations.push(MigrationOp {
            kind: op.kind.to_string(),
            id: None,
            requires: Vec::new(),
            fields: Value::Object(fields),
            footnote: None,
        });
    }

    // ── Body replacement (--body-from-stdin) ─────────────────────────────────
    let mut body_changed = false;
    let mut body_bytes_new: Option<usize> = None;
    let mut body_bytes_old: Option<usize> = None;
    if let Some(new_body) = &params.body {
        if new_body != &doc.body_text {
            let mut fields = Map::new();
            fields.insert("path".into(), Value::String(target_str.clone()));
            fields.insert("new_value".into(), Value::String(new_body.clone()));
            operations.push(MigrationOp {
                kind: "replace_body".to_string(),
                id: None,
                requires: Vec::new(),
                fields: Value::Object(fields),
                footnote: None,
            });
            body_changed = true;
            body_bytes_new = Some(new_body.len());
            body_bytes_old = Some(doc.body_text.len());
        }
    }

    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root,
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations,
        skipped: Vec::new(),
        plan_footnote: None,
    };

    // ── Apply (forecast writes nothing) ──────────────────────────────────────
    let ctx = ApplyContext {
        dry_run: !params.confirm,
        parents: false,
        verbose: false,
        refuse_as_report: true,
        owner_index_options: owner_index_options(config),
    };
    let apply_report = apply_migration_plan(&plan, &index, ctx, sink)?;

    // ── Map the ApplyReport → SetReport ──────────────────────────────────────
    if matches!(
        apply_report.outcome,
        ApplyOutcome::Refused | ApplyOutcome::Failed
    ) {
        let coded = apply_report
            .operations
            .iter()
            .find(|o| o.status == OpStatus::Failed)
            .and_then(|o| o.error.clone())
            .map(|e| CodedError {
                code: e.code,
                message: e.message,
                path: e.path,
            })
            .unwrap_or_else(|| CodedError {
                code: "internal-error".into(),
                message: "apply refused without a coded op error".into(),
                path: None,
            });
        return Ok(MutationExecution {
            report: SetReport {
                schema_version: 2,
                trace_id: String::new(),
                telemetry_degraded: false,
                operation: "set".into(),
                target: target_str,
                frontmatter_changes: Vec::new(),
                body_changed: false,
                body_bytes_new: None,
                body_bytes_old: None,
                applied: false,
                outcome: MutationOutcome::Refused,
                error: Some(coded),
                warnings: Vec::new(),
            },
            touched_paths: Vec::new(),
        });
    }

    let applied = params.confirm;
    let touched_paths = if applied {
        apply_report.touched_paths.clone()
    } else {
        Vec::new()
    };

    // Real trace id on a confirmed apply, empty on a forecast (NRN-400). The
    // shared `apply_migration_plan` executor mints the id from the EventSink on
    // a write and leaves it empty on a dry-run, so the report carries the id
    // that correlates to the durable telemetry line the same apply wrote.
    let trace_id = apply_report.trace_id.clone();
    Ok(MutationExecution {
        report: SetReport {
            schema_version: 2,
            trace_id,
            telemetry_degraded: apply_report.telemetry_degraded,
            operation: "set".into(),
            target: target_str,
            frontmatter_changes: synthed.report_changes,
            body_changed,
            body_bytes_new,
            body_bytes_old,
            applied,
            outcome: if applied {
                MutationOutcome::Applied
            } else {
                MutationOutcome::Forecast
            },
            error: None,
            warnings: synthed.warnings,
        },
        touched_paths,
    })
}

// ── Synth ────────────────────────────────────────────────────────────────────

/// A typed frontmatter op destined for the plan.
struct FmOp {
    kind: &'static str, // set_frontmatter | add_frontmatter | remove_frontmatter
    field: String,
    new_value: Option<Value>,
    expected_old: Option<Value>,
}

struct Synthed {
    ops: Vec<FmOp>,
    report_changes: Vec<FrontmatterChange>,
    warnings: Vec<MutationWarning>,
}

fn synth(
    cfg: &VaultConfig,
    index: &GraphIndex,
    doc: &Document,
    current_fm: &Value,
    params: &SetParams,
) -> Result<Synthed, SetError> {
    detect_cross_class_conflicts(params)?;

    let current_obj = current_fm
        .as_object()
        .ok_or(SetError::FrontmatterNotMapping)?;

    let force = params.force;
    let mut warnings: Vec<MutationWarning> = Vec::new();

    // Schema resolution runs against the POST-state doc (NRN-119).
    let effective =
        coerce::effective_match_doc(doc, current_fm, &params.fields, &params.field_json);

    let (fields_typed, w) = coerce_kv_slice(&params.fields, force, cfg, &effective, true, false)?;
    warnings.extend(w);
    // --push writes a new element into the list, so it enforces allowed_values
    // element-wise (NRN-430): a pushed value outside its field's allowed set
    // REFUSES, with --force the bypass. --pop only removes elements — a removal
    // can never create a violation — so it stays exempt (enforce_allowed=false).
    let (push_typed, w) = coerce_kv_slice(&params.push, force, cfg, &effective, true, true)?;
    warnings.extend(w);
    let (pop_typed, w) = coerce_kv_slice(&params.pop, force, cfg, &effective, false, true)?;
    warnings.extend(w);

    // --field-json: raw JSON, schema-validated unless --force.
    let mut field_json_typed: Vec<(String, Value)> = Vec::new();
    for kv in &params.field_json {
        let (key, raw_json) = coerce::split_kv(kv)
            .ok_or_else(|| SetError::AssignmentMalformed { raw: kv.clone() })?;
        let parsed: Value =
            serde_json::from_str(&raw_json).map_err(|e| SetError::FieldJsonInvalid {
                field: key.clone(),
                detail: e.to_string(),
            })?;
        if let Some(ty) = coerce::lookup_field_type(cfg, &effective, &key) {
            let max = coerce::lookup_field_max_length(cfg, &effective, &key);
            if !crate::standards::predicates::frontmatter_type_matches(&parsed, &ty, max) {
                if !force {
                    return Err(SetError::FieldJsonTypeInvalid {
                        field: key.clone(),
                        field_type: ty,
                    });
                }
                warnings.push(coerce::force_bypass_warning(&key, "type validation"));
            }
        } else if !coerce::is_known_field(cfg, &effective, &key) {
            warnings.push(unknown_field(&key));
        }
        if let Some(allowed) = coerce::lookup_allowed_values(cfg, &effective, &key) {
            if !coerce::value_in_allowed(&parsed, &allowed) {
                if !force {
                    return Err(SetError::FieldJsonNotAllowed {
                        field: key.clone(),
                        allowed: coerce::display_allowed(&allowed),
                    });
                }
                warnings.push(coerce::force_bypass_warning(
                    &key,
                    "allowed-values validation",
                ));
            }
        }
        field_json_typed.push((key, parsed));
    }

    // --remove: required-field protection.
    for key in &params.remove {
        if coerce::is_required_field(cfg, &effective, key) {
            if !force {
                return Err(SetError::RequiredFieldRemoved { field: key.clone() });
            }
            warnings.push(coerce::force_bypass_warning(
                key,
                "required-field protection",
            ));
        }
    }

    // ── Route to ops + report rows ───────────────────────────────────────────
    let mut ops: Vec<FmOp> = Vec::new();
    let mut report: Vec<FrontmatterChange> = Vec::new();

    // --field / --field-json (scalar set/add): group by key, accumulate array.
    let mut grouped_fields: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (k, v) in fields_typed.into_iter().chain(field_json_typed) {
        grouped_fields.entry(k).or_default().push(v);
    }
    for (key, values) in grouped_fields {
        let new_value = if values.len() == 1 {
            values.into_iter().next().unwrap()
        } else {
            Value::Array(values)
        };
        let old = current_obj.get(&key).cloned();
        let kind = if current_obj.contains_key(&key) {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        report.push(FrontmatterChange {
            op: "set".into(),
            field: key.clone(),
            old: old.clone(),
            new: Some(new_value.clone()),
            value: None,
            found: None,
        });
        ops.push(FmOp {
            kind,
            field: key,
            new_value: Some(new_value),
            expected_old: old,
        });
    }

    // --push: aggregate per key, append to the current array. The report row is
    // the RESULTING state, not per-element intent — one `op: "set"` row per key
    // whose `new` is the post-push array.
    let mut grouped_push: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (k, v) in &push_typed {
        grouped_push.entry(k.clone()).or_default().push(v.clone());
    }
    for (key, values) in &grouped_push {
        let current_val = current_obj.get(key).cloned();
        let mut new_array = match &current_val {
            Some(Value::Array(existing)) => existing.clone(),
            None => Vec::new(),
            Some(_) => return Err(SetError::PushOnScalar { key: key.clone() }),
        };
        new_array.extend(values.iter().cloned());
        let kind = if current_val.is_some() {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        let new_value = Value::Array(new_array);
        ops.push(FmOp {
            kind,
            field: key.clone(),
            new_value: Some(new_value.clone()),
            expected_old: current_val.clone(),
        });
        report.push(FrontmatterChange {
            op: "set".into(),
            field: key.clone(),
            old: current_val,
            new: Some(new_value),
            value: None,
            found: None,
        });
    }

    // --pop: drop matching elements. Report the RESULTING state as one
    // `op: "set"` row per key — and ONLY when the array actually changed. A
    // no-op pop (missing key, scalar value, or nothing matched) emits NO row at
    // all (the `frontmatter_changes` list is empty).
    let mut grouped_pop: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (k, v) in &pop_typed {
        grouped_pop.entry(k.clone()).or_default().push(v.clone());
    }
    for (key, drops) in &grouped_pop {
        if let Some(Value::Array(existing)) = current_obj.get(key) {
            let new_array: Vec<Value> = existing
                .iter()
                .filter(|v| !drops.contains(v))
                .cloned()
                .collect();
            if new_array.len() != existing.len() {
                let old = Value::Array(existing.clone());
                let new_value = Value::Array(new_array);
                ops.push(FmOp {
                    kind: "set_frontmatter",
                    field: key.clone(),
                    new_value: Some(new_value.clone()),
                    expected_old: Some(old.clone()),
                });
                report.push(FrontmatterChange {
                    op: "set".into(),
                    field: key.clone(),
                    old: Some(old),
                    new: Some(new_value),
                    value: None,
                    found: None,
                });
            }
        }
    }

    // --remove: only when the key exists.
    for key in &params.remove {
        if current_obj.contains_key(key) {
            let old = current_obj.get(key).cloned();
            report.push(FrontmatterChange {
                op: "remove".into(),
                field: key.clone(),
                old: old.clone(),
                new: None,
                value: None,
                found: None,
            });
            ops.push(FmOp {
                kind: "remove_frontmatter",
                field: key.clone(),
                new_value: None,
                expected_old: old,
            });
        }
    }

    // ── Wikilink resolution sweep (warn-class), post-state schema (NRN-119). ──
    for op in &ops {
        if op.kind == "remove_frontmatter" {
            continue;
        }
        let ft = coerce::lookup_field_type(cfg, &effective, &op.field);
        if !matches!(ft.as_deref(), Some("wikilink") | Some("wikilink_or_list")) {
            continue;
        }
        match &op.new_value {
            Some(Value::String(s)) => {
                warnings.extend(super::wikilink_warnings(index, &op.field, s))
            }
            Some(Value::Array(items)) => {
                for it in items {
                    if let Some(s) = it.as_str() {
                        warnings.extend(super::wikilink_warnings(index, &op.field, s));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Synthed {
        ops,
        report_changes: report,
        warnings,
    })
}

/// Typed `(KEY, Value)` pairs plus any warnings emitted while coercing a slice.
type CoercedKvs = (Vec<(String, Value)>, Vec<MutationWarning>);

/// Coerce a `KEY=raw` slice into typed `(KEY, Value)` pairs + warnings.
fn coerce_kv_slice(
    raw_kvs: &[String],
    force: bool,
    cfg: &VaultConfig,
    doc: &Document,
    enforce_allowed: bool,
    element_wise: bool,
) -> Result<CoercedKvs, SetError> {
    let mut out = Vec::new();
    let mut w = Vec::new();
    for kv in raw_kvs {
        let (key, raw) = coerce::split_kv(kv)
            .ok_or_else(|| SetError::AssignmentMalformed { raw: kv.clone() })?;
        let coerced = match coerce::lookup_field_type(cfg, doc, &key) {
            Some(ty) if !force => {
                let max = coerce::lookup_field_max_length(cfg, doc, &key);
                // --push/--pop operate on a single list ELEMENT, so a
                // list_of_strings field coerces as its element type (string).
                let effective_ty: &str = if element_wise && ty == "list_of_strings" {
                    "string"
                } else {
                    ty.as_str()
                };
                coerce::coerce_value_for_type(effective_ty, &raw, max).map_err(SetError::Coerce)?
            }
            Some(_) => {
                w.push(coerce::force_bypass_warning(&key, "type validation"));
                Value::String(raw.clone())
            }
            None => {
                if !coerce::is_known_field(cfg, doc, &key) {
                    w.push(unknown_field(&key));
                }
                coerce::infer_scalar(&raw)
            }
        };

        if enforce_allowed {
            if let Some(allowed) = coerce::lookup_allowed_values(cfg, doc, &key) {
                if !coerce::value_in_allowed(&coerced, &allowed) {
                    if !force {
                        return Err(SetError::ValueNotAllowed {
                            field: key.clone(),
                            value: coerce::display_value(&coerced),
                            allowed: coerce::display_allowed(&allowed),
                        });
                    }
                    w.push(coerce::force_bypass_warning(
                        &key,
                        "allowed-values validation",
                    ));
                }
            }
        }

        out.push((key, coerced));
    }
    Ok((out, w))
}

/// Refuse when any key appears across multiple mutation classes.
fn detect_cross_class_conflicts(params: &SetParams) -> Result<(), SetError> {
    let mut by_key: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();
    let mut record = |raw: &str, class: &'static str| -> Result<(), SetError> {
        let (k, _) = coerce::split_kv(raw)
            .ok_or_else(|| SetError::AssignmentMalformed { raw: raw.into() })?;
        by_key.entry(k).or_default().insert(class);
        Ok(())
    };
    for kv in &params.fields {
        record(kv, "--field")?;
    }
    for kv in &params.field_json {
        record(kv, "--field-json")?;
    }
    for kv in &params.push {
        record(kv, "--push")?;
    }
    for kv in &params.pop {
        record(kv, "--pop")?;
    }
    for k in &params.remove {
        by_key.entry(k.clone()).or_default().insert("--remove");
    }

    let conflicts: Vec<(String, Vec<&'static str>)> = by_key
        .into_iter()
        .filter(|(_, classes)| classes.len() > 1)
        .map(|(k, classes)| (k, classes.into_iter().collect()))
        .collect();
    if conflicts.is_empty() {
        return Ok(());
    }
    // Refusal prose is end-user contract: a header, one indented
    // `'key': --a + --b` line per conflict, then the trailing explainer.
    let mut msg = String::from("cross-class conflict on the same key:\n");
    for (k, classes) in &conflicts {
        msg.push_str(&format!("  '{k}': {}\n", classes.join(" + ")));
    }
    msg.push_str(
        "each key may be targeted by only one of --field/--field-json/--push/--pop/--remove per invocation",
    );
    Err(SetError::FieldConflict { message: msg })
}

fn unknown_field(key: &str) -> MutationWarning {
    MutationWarning {
        code: "unknown-field".into(),
        field: Some(key.to_string()),
        message: format!("field '{key}' not declared in schema"),
    }
}

fn refused(
    target: impl Into<String>,
    code: &str,
    message: impl Into<String>,
    path: Option<String>,
) -> MutationExecution<SetReport> {
    MutationExecution {
        report: SetReport {
            schema_version: 2,
            trace_id: String::new(),
            telemetry_degraded: false,
            operation: "set".into(),
            target: target.into(),
            frontmatter_changes: Vec::new(),
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied: false,
            outcome: MutationOutcome::Refused,
            error: Some(CodedError {
                code: code.into(),
                message: message.into(),
                path,
            }),
            warnings: Vec::new(),
        },
        touched_paths: Vec::new(),
    }
}

// ── Refusal vocabulary ─────────────────────────────────────────────────────────

#[derive(Debug)]
enum SetError {
    Coerce(CoerceError),
    ValueNotAllowed {
        field: String,
        value: String,
        allowed: String,
    },
    FieldJsonInvalid {
        field: String,
        detail: String,
    },
    FieldJsonTypeInvalid {
        field: String,
        field_type: String,
    },
    FieldJsonNotAllowed {
        field: String,
        allowed: String,
    },
    RequiredFieldRemoved {
        field: String,
    },
    AssignmentMalformed {
        raw: String,
    },
    FieldConflict {
        message: String,
    },
    PushOnScalar {
        key: String,
    },
    FrontmatterNotMapping,
}

impl SetError {
    fn code(&self) -> &'static str {
        match self {
            SetError::Coerce(e) => e.code(),
            SetError::ValueNotAllowed { .. } | SetError::FieldJsonNotAllowed { .. } => {
                "value-not-allowed"
            }
            SetError::FieldJsonInvalid { .. } => "field-json-invalid",
            SetError::FieldJsonTypeInvalid { .. } => "field-type-invalid",
            SetError::RequiredFieldRemoved { .. } => "required-field-removed",
            SetError::AssignmentMalformed { .. } => "assignment-malformed",
            SetError::FieldConflict { .. } => "field-conflict",
            SetError::PushOnScalar { .. } => "push-on-scalar",
            SetError::FrontmatterNotMapping => "frontmatter-not-mapping",
        }
    }
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::Coerce(e) => write!(f, "{e}"),
            SetError::ValueNotAllowed {
                field,
                value,
                allowed,
            } => write!(f, "{}", coerce::value_not_allowed_message(field, value, allowed)),
            SetError::FieldJsonInvalid { field, detail } => {
                write!(f, "--field-json value is not valid JSON ({field}): {detail}")
            }
            SetError::FieldJsonTypeInvalid { field, field_type } => write!(
                f,
                "--field-json value for '{field}' does not match schema type '{field_type}'"
            ),
            SetError::FieldJsonNotAllowed { field, allowed } => write!(
                f,
                "--field-json value for '{field}' is not allowed (allowed: {allowed}); use --force to override"
            ),
            SetError::RequiredFieldRemoved { field } => {
                write!(f, "cannot remove required field '{field}'; use --force to override")
            }
            SetError::AssignmentMalformed { raw } => write!(f, "expected KEY=VALUE, got: {raw}"),
            SetError::FieldConflict { message } => write!(f, "{message}"),
            SetError::PushOnScalar { key } => write!(
                f,
                "--push on key '{key}' requires an array-typed value (current is scalar)"
            ),
            SetError::FrontmatterNotMapping => write!(f, "frontmatter is not a top-level mapping"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-19";

    fn sink() -> crate::telemetry::EventSink {
        crate::telemetry::EventSink::discard(
            crate::telemetry::IdGen::with_seed(0),
            crate::telemetry::Clock::fixed("2026-07-19T00:00:00.000Z"),
        )
    }

    fn synth_vault(config: Option<&str>, docs: &[(&str, &str)]) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        if let Some(cfg) = config {
            std::fs::create_dir(root.join(".norn").as_std_path()).unwrap();
            std::fs::write(root.join(".norn/config.yaml").as_std_path(), cfg).unwrap();
        }
        for (path, contents) in docs {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent.as_std_path()).unwrap();
            }
            std::fs::write(full.as_std_path(), contents).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> crate::cache::Cache {
        let mut cache = crate::cache::Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    fn parse_cfg(cfg: &str) -> VaultConfig {
        crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap()
    }

    fn set_params(target: &str) -> SetParams {
        SetParams {
            target: target.into(),
            ..Default::default()
        }
    }

    #[test]
    fn add_vs_set_routing() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\nstatus: draft\n---\nbody\n")]);
        let cache = built(&root);
        // Existing key → set; absent key → add. Both land as report op "set".
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["status=done".into(), "priority=high".into()],
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        let r = &exec.report;
        assert_eq!(r.outcome, MutationOutcome::Forecast);
        assert!(!r.applied); // forecast
        let by_field: BTreeMap<_, _> = r
            .frontmatter_changes
            .iter()
            .map(|c| (c.field.as_str(), c))
            .collect();
        let status = by_field.get("status").unwrap();
        assert_eq!(status.op, "set");
        assert_eq!(status.old.as_ref().unwrap(), &Value::String("draft".into()));
        assert_eq!(status.new.as_ref().unwrap(), &Value::String("done".into()));
        let priority = by_field.get("priority").unwrap();
        assert_eq!(priority.op, "set");
        assert!(priority.old.is_none()); // absent → add → no old
    }

    #[test]
    fn remove_emits_remove_change() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\nstatus: draft\ntmp: x\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            remove: vec!["tmp".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        let changes = &exec.report.frontmatter_changes;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].op, "remove");
        assert_eq!(changes[0].field, "tmp");
    }

    #[test]
    fn remove_missing_key_is_silent() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\nstatus: draft\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            remove: vec!["nonexistent".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert!(exec.report.frontmatter_changes.is_empty());
    }

    #[test]
    fn field_conflict_refused() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntags: []\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["tags=foo".into()],
            push: vec!["tags=bar".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "field-conflict");
    }

    #[test]
    fn unknown_field_warns_but_applies() {
        let cfg = "validate:\n  rules:\n    - name: r\n      match:\n        path: \"**/*.md\"\n      allowed_values:\n        status: [draft, done]\n";
        let (_t, root) = synth_vault(Some(cfg), &[("a.md", "---\nstatus: draft\n---\n")]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["staus=done".into()], // typo, declared nowhere
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        // No `confirm` → a forecast; the unknown-field warning still surfaces.
        assert_eq!(exec.report.outcome, MutationOutcome::Forecast);
        assert!(exec
            .report
            .warnings
            .iter()
            .any(|w| w.code == "unknown-field"));
    }

    #[test]
    fn declared_type_coercion_wraps_wikilink() {
        let cfg = "validate:\n  rules:\n    - name: r\n      match:\n        path: \"**/*.md\"\n      field_types:\n        workspace: wikilink\n";
        let (_t, root) = synth_vault(Some(cfg), &[("a.md", "---\ntitle: A\n---\n")]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["workspace=norn".into()],
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        let ws = exec
            .report
            .frontmatter_changes
            .iter()
            .find(|c| c.field == "workspace")
            .unwrap();
        assert_eq!(ws.new.as_ref().unwrap(), &Value::String("[[norn]]".into()));
    }

    #[test]
    fn value_not_allowed_refused() {
        let cfg = "validate:\n  rules:\n    - name: r\n      match:\n        path: \"**/*.md\"\n      allowed_values:\n        status: [draft, done]\n";
        let (_t, root) = synth_vault(Some(cfg), &[("a.md", "---\nstatus: draft\n---\n")]);
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["status=bogus".into()],
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(
            exec.report.error.as_ref().unwrap().code,
            "value-not-allowed"
        );
    }

    // ── NRN-430: --push enforces allowed_values element-wise; --pop is exempt ──

    // A list field constrained by allowed_values: pushing an out-of-set element
    // refuses, --force bypasses, and popping an element never refuses.
    const PUSH_ALLOWED_CFG: &str = "validate:\n  rules:\n    - name: r\n      match:\n        path: \"**/*.md\"\n      field_types:\n        labels: list_of_strings\n      allowed_values:\n        labels: [red, green]\n";

    #[test]
    fn push_disallowed_element_refused() {
        let (_t, root) = synth_vault(
            Some(PUSH_ALLOWED_CFG),
            &[("a.md", "---\nlabels:\n  - red\n---\n")],
        );
        let cache = built(&root);
        let config = parse_cfg(PUSH_ALLOWED_CFG);
        let params = SetParams {
            target: "a.md".into(),
            push: vec!["labels=purple".into()],
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        let err = exec.report.error.as_ref().unwrap();
        assert_eq!(err.code, "value-not-allowed");
        assert!(err.message.contains("purple"), "{}", err.message);
        assert!(err.message.contains("--force"), "{}", err.message);
    }

    #[test]
    fn push_disallowed_element_force_bypasses() {
        let (_t, root) = synth_vault(
            Some(PUSH_ALLOWED_CFG),
            &[("a.md", "---\nlabels:\n  - red\n---\n")],
        );
        let cache = built(&root);
        let config = parse_cfg(PUSH_ALLOWED_CFG);
        let params = SetParams {
            target: "a.md".into(),
            push: vec!["labels=purple".into()],
            force: true,
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(exec
            .report
            .warnings
            .iter()
            .any(|w| w.code == "force-bypass" && w.field.as_deref() == Some("labels")));
    }

    #[test]
    fn push_allowed_element_applies() {
        let (_t, root) = synth_vault(
            Some(PUSH_ALLOWED_CFG),
            &[("a.md", "---\nlabels:\n  - red\n---\n")],
        );
        let cache = built(&root);
        let config = parse_cfg(PUSH_ALLOWED_CFG);
        let params = SetParams {
            target: "a.md".into(),
            push: vec!["labels=green".into()],
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
    }

    #[test]
    fn pop_is_exempt_from_allowed_values() {
        // A stored value already outside the allowed set can still be popped —
        // removal never creates a violation, so --pop stays unenforced.
        let (_t, root) = synth_vault(
            Some(PUSH_ALLOWED_CFG),
            &[("a.md", "---\nlabels:\n  - red\n  - purple\n---\n")],
        );
        let cache = built(&root);
        let config = parse_cfg(PUSH_ALLOWED_CFG);
        let params = SetParams {
            target: "a.md".into(),
            pop: vec!["labels=purple".into()],
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        let on_disk = std::fs::read_to_string(root.join("a.md").as_std_path()).unwrap();
        assert!(!on_disk.contains("purple"), "purple should be popped");
    }

    #[test]
    fn target_not_found_refused() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntype: note\n---\n")]);
        let cache = built(&root);
        let exec = execute(&cache, None, &set_params("nonexistent"), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "target-not-found");
    }

    #[test]
    fn target_ambiguous_refused() {
        let (_t, root) = synth_vault(
            None,
            &[
                ("a/shared.md", "---\ntype: note\n---\n"),
                ("b/shared.md", "---\ntype: note\n---\n"),
            ],
        );
        let cache = built(&root);
        let exec = execute(&cache, None, &set_params("shared"), TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.outcome, MutationOutcome::Refused);
        assert_eq!(exec.report.error.as_ref().unwrap().code, "target-ambiguous");
    }

    #[test]
    fn dry_run_writes_nothing_apply_writes_file() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\nstatus: draft\n---\nbody\n")]);
        let disk = root.join("a.md");

        // Forecast: file unchanged, touched_paths empty.
        let cache = built(&root);
        let forecast = SetParams {
            target: "a.md".into(),
            fields: vec!["status=done".into()],
            confirm: false,
            ..Default::default()
        };
        let exec = execute(&cache, None, &forecast, TODAY, &mut sink()).unwrap();
        assert!(!exec.report.applied);
        assert!(exec.touched_paths.is_empty());
        assert!(std::fs::read_to_string(disk.as_std_path())
            .unwrap()
            .contains("status: draft"));

        // Apply: file changed on disk, touched_paths names it.
        let apply = SetParams {
            confirm: true,
            ..forecast
        };
        let exec = execute(&cache, None, &apply, TODAY, &mut sink()).unwrap();
        assert!(exec.report.applied);
        assert_eq!(exec.report.outcome, MutationOutcome::Applied);
        assert!(!exec.touched_paths.is_empty());
        let on_disk = std::fs::read_to_string(disk.as_std_path()).unwrap();
        assert!(on_disk.contains("status: done"), "file should be updated");
    }

    #[test]
    fn body_from_stdin_replaces_body() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntype: note\n---\nold body\n")]);
        let disk = root.join("a.md");
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            body: Some("new body\n".into()),
            confirm: true,
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert!(exec.report.body_changed);
        assert_eq!(exec.report.body_bytes_new, Some("new body\n".len()));
        let on_disk = std::fs::read_to_string(disk.as_std_path()).unwrap();
        assert!(on_disk.contains("new body"));
        assert!(!on_disk.contains("old body"));
    }

    #[test]
    fn nrn371_new_then_set_roundtrip() {
        // Create a doc on a defaults-free path via `new`, then set a field on it.
        let (_t, root) = synth_vault(None, &[]);
        let mut cache = built(&root);

        let new_params = norn_wire::NewParams {
            path: Some("notes/fresh.md".into()),
            parents: true,
            confirm: true,
            ..Default::default()
        };
        let new_exec =
            crate::mutate::new::execute(&cache, None, &new_params, TODAY, &mut sink()).unwrap();
        assert!(new_exec.report.applied, "new should apply");
        assert!(root.join("notes/fresh.md").as_std_path().exists());

        // Refresh the cache so `set` sees the new document.
        cache.full_build(&root).unwrap();

        let set_params = SetParams {
            target: "notes/fresh.md".into(),
            fields: vec!["status=active".into()],
            confirm: true,
            ..Default::default()
        };
        let set_exec = execute(&cache, None, &set_params, TODAY, &mut sink()).unwrap();
        assert_eq!(set_exec.report.outcome, MutationOutcome::Applied);
        assert!(set_exec.report.applied);
        let on_disk = std::fs::read_to_string(root.join("notes/fresh.md").as_std_path()).unwrap();
        assert!(on_disk.contains("status: active"));
    }

    // ── Review fixes F2/F4/F5 ────────────────────────────────────────────────

    #[test]
    fn f2_field_conflict_message_keeps_body_and_explainer() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntags: []\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["tags=foo".into()],
            push: vec!["tags=bar".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        let msg = &exec.report.error.as_ref().unwrap().message;
        assert!(
            msg.starts_with("cross-class conflict on the same key:\n"),
            "{msg}"
        );
        assert!(msg.contains("  'tags': --field + --push\n"), "{msg}");
        assert!(
            msg.ends_with("each key may be targeted by only one of --field/--field-json/--push/--pop/--remove per invocation"),
            "{msg}"
        );
    }

    #[test]
    fn f2_value_not_allowed_message_includes_the_offending_value() {
        let cfg = "validate:\n  rules:\n    - name: task-rule\n      match:\n        frontmatter:\n          type: task\n      allowed_values:\n        status: [backlog, done]\n";
        let (_t, root) = synth_vault(
            Some(cfg),
            &[("a.md", "---\ntype: task\nstatus: backlog\n---\n")],
        );
        let cache = built(&root);
        let config = parse_cfg(cfg);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["status=bogus".into()],
            ..Default::default()
        };
        let exec = execute(&cache, Some(&config), &params, TODAY, &mut sink()).unwrap();
        let err = exec.report.error.as_ref().unwrap();
        assert_eq!(err.code, "value-not-allowed");
        assert!(
            err.message
                .starts_with("value 'bogus' is not allowed for 'status'"),
            "{}",
            err.message
        );
    }

    #[test]
    fn f4_push_collapses_to_a_single_set_row_with_the_resulting_array() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntags:\n  - x\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            push: vec!["tags=y".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert_eq!(exec.report.frontmatter_changes.len(), 1);
        let ch = &exec.report.frontmatter_changes[0];
        assert_eq!(ch.op, "set");
        assert_eq!(ch.field, "tags");
        assert_eq!(ch.old, Some(serde_json::json!(["x"])));
        assert_eq!(ch.new, Some(serde_json::json!(["x", "y"])));
        assert!(ch.value.is_none() && ch.found.is_none());
    }

    #[test]
    fn f4_pop_that_matches_nothing_emits_no_change_row() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntags:\n  - x\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            pop: vec!["tags=zzz".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        assert!(
            exec.report.frontmatter_changes.is_empty(),
            "a no-op pop reports no change row (empty frontmatter_changes)"
        );
    }

    #[test]
    fn f5_unknown_field_warning_carries_the_structured_field_member() {
        let (_t, root) = synth_vault(None, &[("a.md", "---\ntitle: A\n---\n")]);
        let cache = built(&root);
        let params = SetParams {
            target: "a.md".into(),
            fields: vec!["reviewer=me".into()],
            ..Default::default()
        };
        let exec = execute(&cache, None, &params, TODAY, &mut sink()).unwrap();
        let w = exec
            .report
            .warnings
            .iter()
            .find(|w| w.code == "unknown-field")
            .expect("an undeclared field warns");
        assert_eq!(w.field.as_deref(), Some("reviewer"));
        assert_eq!(w.message, "field 'reviewer' not declared in schema");
    }
}
