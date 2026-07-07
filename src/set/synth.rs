//! `norn set` plan synthesis: CLI args → RepairPlan.

use std::collections::{BTreeMap, BTreeSet};

use crate::cache::Cache;
use crate::standards::PlannedChange;
use anyhow::{anyhow, bail, Result};
use camino::Utf8PathBuf;
use serde_json::Value;

/// Resolve the user-supplied DOC argument into a vault-relative path.
/// Accepts path, stem, or wikilink-shaped input (with or without [[]]).
/// Anchor / block-ref / pipe-alias suffixes are stripped before resolution.
///
/// Refuses (Err) when:
/// - The target doesn't resolve to any doc.
/// - The target resolves to multiple docs (ambiguous stem).
pub fn resolve_target(cache: &Cache, raw: &str) -> Result<Utf8PathBuf> {
    let resolved = crate::show::target::resolve_target(cache, raw)?;
    match resolved.paths.len() {
        0 => bail!("doc not found: {raw}"),
        1 => Ok(resolved.paths.into_iter().next().unwrap()),
        n => {
            let candidates: Vec<String> = resolved.paths.iter().map(|p| p.to_string()).collect();
            Err(anyhow!(
                "ambiguous doc target: '{raw}' matches {n} docs: {}",
                candidates.join(", ")
            ))
        }
    }
}

/// Split `KEY=VALUE` at the first separator. Canonically `=` (assignment
/// idiom); a `:` is also accepted (ADR 0010 separator forgiveness) — the split
/// point is the first `:` or `=`, whichever comes first. Returns Err on missing
/// separator or empty KEY. VALUE may contain additional separator characters
/// (preserved verbatim).
pub fn parse_kv(raw: &str) -> Result<(String, String)> {
    let (k, v) = crate::grammar::split_field_value(raw)
        .ok_or_else(|| anyhow!("expected KEY=VALUE, got: {raw}"))?;
    if k.is_empty() {
        bail!("KEY cannot be empty in: {raw}");
    }
    Ok((k.to_string(), v.to_string()))
}

/// Fold trailing `KEY=VALUE` positionals (ADR 0010, NRN-208) into the explicit
/// `--field` list so a positional `k=v` and a `--field k=v` are indistinguishable
/// downstream (same coercion, same accumulation). Each positional MUST carry a
/// `:` or `=` separator — inheriting batch A's [`split_field_value`] forgiveness
/// (first separator wins) — or it is a hard error. Explicit `--field` values
/// come first; positionals are appended in argv order, so a key present in both
/// accumulates into an array reading left-to-right.
pub fn desugar_positional_fields(positionals: &[String], fields: &[String]) -> Result<Vec<String>> {
    let mut combined = fields.to_vec();
    for token in positionals {
        if crate::grammar::split_field_value(token).is_none() {
            bail!("expected key=value, got '{token}'");
        }
        combined.push(token.clone());
    }
    Ok(combined)
}

/// Refuse with a clear error if any key appears across multiple mutation
/// classes (--field/--field-json/--push/--pop/--remove). Within-class
/// multi-instance is fine (accumulation semantics).
///
/// --field and --field-json are treated as a single class for this purpose:
/// both write a value to the key, and using both for the same key is
/// ambiguous.
pub fn detect_cross_class_conflicts(
    fields: &[String],
    field_json: &[String],
    push: &[String],
    pop: &[String],
    remove: &[String],
) -> Result<()> {
    let mut by_key: BTreeMap<String, BTreeSet<&'static str>> = BTreeMap::new();

    for kv in fields {
        let (k, _) = parse_kv(kv)?;
        by_key.entry(k).or_default().insert("--field");
    }
    for kv in field_json {
        let (k, _) = parse_kv(kv)?;
        by_key.entry(k).or_default().insert("--field-json");
    }
    for kv in push {
        let (k, _) = parse_kv(kv)?;
        by_key.entry(k).or_default().insert("--push");
    }
    for kv in pop {
        let (k, _) = parse_kv(kv)?;
        by_key.entry(k).or_default().insert("--pop");
    }
    for k in remove {
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

    let mut msg = String::from("cross-class conflict on the same key:\n");
    for (k, classes) in &conflicts {
        msg.push_str(&format!("  '{k}': {}\n", classes.join(" + ")));
    }
    msg.push_str("each key may be targeted by only one of --field/--field-json/--push/--pop/--remove per invocation");
    bail!("{msg}")
}

/// Light type inference for schema-silent values:
/// - "true"/"false" → bool
/// - integer-shaped → i64
/// - "null" → null
/// - everything else → string
///
/// Does NOT do YAML datetime/date inference (foot-gun).
pub fn infer_scalar(raw: &str) -> Value {
    match raw {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        s => {
            if let Ok(n) = s.parse::<i64>() {
                Value::from(n)
            } else {
                Value::String(s.to_string())
            }
        }
    }
}

/// Construct a partial PlannedChange with the operator-mutation defaults filled in.
///
/// path / document_hash / change_id are left empty for the caller to stamp.
pub fn make_planned_change(
    key: &str,
    op: &str,
    expected_old: Option<Value>,
    new_value: Option<Value>,
) -> PlannedChange {
    PlannedChange {
        change_id: String::new(),
        path: camino::Utf8PathBuf::new(),
        document_hash: String::new(),
        finding_code: "operator-mutation".to_string(),
        finding_rule: None,
        repair_rule: "vault-set".to_string(),
        operation: op.to_string(),
        field: Some(key.to_string()),
        expected_old_value: expected_old,
        new_value,
        destination: None,
        link_risk: None,
        warnings: vec![],
        force: false,
        parents: false,
    }
}

/// Schema-silent plan synthesis from CLI args.
///
/// Uses light type inference for --field values (bool/int/null + string
/// fallback). Push/pop arrays are resolved against the current value at synth
/// time.
///
/// Refuses on:
/// - Cross-class same-key conflicts (delegates to detect_cross_class_conflicts)
/// - Malformed JSON in --field-json
/// - --push against a current scalar value (schema-silent defensive check)
///
/// Silent no-ops:
/// - --pop on missing key, scalar value, or value not in array
/// - --remove on missing key
#[allow(dead_code)] // schema-silent path; only called from unit tests (production uses synth_frontmatter_ops_typed via synth_with_schema)
pub fn synth_frontmatter_ops(
    current_frontmatter: &Value,
    fields: &[String],
    field_json: &[String],
    push: &[String],
    pop: &[String],
    remove: &[String],
) -> Result<Vec<PlannedChange>> {
    detect_cross_class_conflicts(fields, field_json, push, pop, remove)?;

    let current_obj = current_frontmatter
        .as_object()
        .ok_or_else(|| anyhow!("frontmatter is not a top-level mapping"))?;

    let mut changes: Vec<PlannedChange> = Vec::new();

    // --field: group by key, multi-instance accumulates into array.
    let mut grouped_fields: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for kv in fields {
        let (k, v) = parse_kv(kv)?;
        grouped_fields.entry(k).or_default().push(infer_scalar(&v));
    }
    for (key, values) in &grouped_fields {
        let new_value = if values.len() == 1 {
            values[0].clone()
        } else {
            Value::Array(values.clone())
        };
        let op = if current_obj.contains_key(key) {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        let expected_old_value = current_obj.get(key).cloned();
        changes.push(make_planned_change(
            key,
            op,
            expected_old_value,
            Some(new_value),
        ));
    }

    // --field-json: raw JSON parsed verbatim.
    for kv in field_json {
        let (key, raw_json) = parse_kv(kv)?;
        let parsed: Value = serde_json::from_str(&raw_json)
            .map_err(|e| anyhow!("--field-json value is not valid JSON ({key}): {e}"))?;
        let op = if current_obj.contains_key(&key) {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        let expected_old_value = current_obj.get(&key).cloned();
        changes.push(make_planned_change(
            &key,
            op,
            expected_old_value,
            Some(parsed),
        ));
    }

    // --push: group by key, resolve against current array.
    let mut grouped_push: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for kv in push {
        let (k, v) = parse_kv(kv)?;
        grouped_push.entry(k).or_default().push(infer_scalar(&v));
    }
    for (key, values) in &grouped_push {
        let current_val = current_obj.get(key);
        let mut new_array = match current_val {
            Some(Value::Array(existing)) => existing.clone(),
            None => Vec::new(),
            Some(_) => {
                bail!("--push on key '{key}' requires an array-typed value (current is scalar)")
            }
        };
        new_array.extend(values.iter().cloned());
        let op = if current_val.is_some() {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        changes.push(make_planned_change(
            key,
            op,
            current_val.cloned(),
            Some(Value::Array(new_array)),
        ));
    }

    // --pop: group by key, resolve drops against current array.
    // Silent no-op when key missing, scalar value, or value not in array.
    let mut grouped_pop: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for kv in pop {
        let (k, v) = parse_kv(kv)?;
        grouped_pop.entry(k).or_default().push(infer_scalar(&v));
    }
    for (key, drops) in &grouped_pop {
        let current_val = current_obj.get(key);
        let Some(Value::Array(existing)) = current_val else {
            continue; // silent no-op
        };
        let new_array: Vec<Value> = existing
            .iter()
            .filter(|v| !drops.contains(v))
            .cloned()
            .collect();
        if new_array.len() == existing.len() {
            continue; // no actual change → silent no-op
        }
        changes.push(make_planned_change(
            key,
            "set_frontmatter",
            current_val.cloned(),
            Some(Value::Array(new_array)),
        ));
    }

    // --remove: emit only when the key actually exists.
    for key in remove {
        if !current_obj.contains_key(key) {
            continue; // silent no-op
        }
        changes.push(make_planned_change(
            key,
            "remove_frontmatter",
            current_obj.get(key).cloned(),
            None,
        ));
    }

    Ok(changes)
}

/// Typed-input variant of `synth_frontmatter_ops`: skips `parse_kv` + `infer_scalar`
/// since the caller has already coerced values per schema. Used by the
/// schema-aware path in `crate::set::validate::synth_with_schema`.
///
/// Each fields/push/pop entry is (key, typed_Value). --remove takes plain keys.
/// Same routing rules as `synth_frontmatter_ops`:
/// - existing key + value-replacement → set_frontmatter
/// - missing key + value-creation → add_frontmatter
/// - --push on scalar current → refuse
/// - --pop on missing/scalar/absent-value → silent no-op
/// - --remove on missing → silent no-op
pub fn synth_frontmatter_ops_typed(
    current_frontmatter: &Value,
    fields: &[(String, Value)],
    push: &[(String, Value)],
    pop: &[(String, Value)],
    remove: &[String],
) -> Result<Vec<PlannedChange>> {
    let current_obj = current_frontmatter
        .as_object()
        .ok_or_else(|| anyhow!("frontmatter is not a top-level mapping"))?;

    let mut changes: Vec<PlannedChange> = Vec::new();

    // Group fields by key. Multi-instance of same key accumulates into an array.
    let mut grouped_fields: BTreeMap<String, Vec<Value>> = Default::default();
    for (k, v) in fields {
        grouped_fields.entry(k.clone()).or_default().push(v.clone());
    }
    for (key, values) in &grouped_fields {
        let new_value = if values.len() == 1 {
            values[0].clone()
        } else {
            Value::Array(values.clone())
        };
        let op = if current_obj.contains_key(key) {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        changes.push(make_planned_change(
            key,
            op,
            current_obj.get(key).cloned(),
            Some(new_value),
        ));
    }

    // --push
    let mut grouped_push: BTreeMap<String, Vec<Value>> = Default::default();
    for (k, v) in push {
        grouped_push.entry(k.clone()).or_default().push(v.clone());
    }
    for (key, values) in &grouped_push {
        let current_val = current_obj.get(key);
        let mut new_array = match current_val {
            Some(Value::Array(existing)) => existing.clone(),
            None => Vec::new(),
            Some(_) => {
                bail!("--push on key '{key}' requires an array-typed value (current is scalar)")
            }
        };
        new_array.extend(values.iter().cloned());
        let op = if current_val.is_some() {
            "set_frontmatter"
        } else {
            "add_frontmatter"
        };
        changes.push(make_planned_change(
            key,
            op,
            current_val.cloned(),
            Some(Value::Array(new_array)),
        ));
    }

    // --pop
    let mut grouped_pop: BTreeMap<String, Vec<Value>> = Default::default();
    for (k, v) in pop {
        grouped_pop.entry(k.clone()).or_default().push(v.clone());
    }
    for (key, drops) in &grouped_pop {
        let current_val = current_obj.get(key);
        let Some(Value::Array(existing)) = current_val else {
            continue; // silent no-op
        };
        let new_array: Vec<Value> = existing
            .iter()
            .filter(|v| !drops.contains(v))
            .cloned()
            .collect();
        if new_array.len() == existing.len() {
            continue; // no actual change → silent no-op
        }
        changes.push(make_planned_change(
            key,
            "set_frontmatter",
            current_val.cloned(),
            Some(Value::Array(new_array)),
        ));
    }

    // --remove
    for key in remove {
        if !current_obj.contains_key(key) {
            continue; // silent no-op
        }
        changes.push(make_planned_change(
            key,
            "remove_frontmatter",
            current_obj.get(key).cloned(),
            None,
        ));
    }

    Ok(changes)
}

// ── Phase 5: preflight_and_plan ─────────────────────────────────────────────

/// Outcome of a successful preflight: a ready-to-apply RepairPlan plus metadata
/// needed for rendering (warnings, body-change sizing).
pub struct PreflightOutcome {
    pub plan: crate::standards::RepairPlan,
    pub warnings: Vec<crate::set::validate::SetWarning>,
    pub target: camino::Utf8PathBuf,
    pub body_changed: bool,
    pub body_bytes_new: Option<usize>,
    pub body_bytes_old: usize,
}

/// End-to-end plan synthesis for `norn set`:
/// resolve target → load doc → optionally read stdin → schema-aware synth →
/// wikilink resolution sweep → optional body op → stamp path/hash → wrap
/// into a RepairPlan.
pub fn preflight_and_plan(
    cwd: &camino::Utf8Path,
    cache: &crate::cache::Cache,
    index: &crate::core::GraphIndex,
    cfg: &crate::standards::VaultConfig,
    args: &crate::cli::SetArgs,
) -> anyhow::Result<PreflightOutcome> {
    use std::io::Read as _;

    // 1. Resolve target.
    let target_path = resolve_target(cache, &args.target)?;
    let full_path = cwd.join(&target_path);

    // 2. Load doc content.
    let content = std::fs::read_to_string(full_path.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to read {full_path}: {e}"))?;

    // 3. Parse frontmatter + body.
    let (current_fm, current_body) = parse_doc(&content)?;
    let current_body_len = current_body.len();

    // 4. Read stdin if --body-from-stdin.
    let new_body: Option<String> = if args.body_from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| anyhow::anyhow!("failed to read stdin: {e}"))?;
        Some(buf)
    } else {
        None
    };

    // 5. Find the doc in the index.
    let doc = index
        .documents
        .iter()
        .find(|d| d.path == target_path)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("doc not in index: {target_path}"))?;

    // 6. Schema-aware synth. Trailing `KEY=VALUE` positionals desugar into the
    // `--field` list (ADR 0010, NRN-208) so they flow through the identical
    // coercing path — a positional `k=v` is indistinguishable from `--field k=v`.
    let combined_fields = desugar_positional_fields(&args.field_pos, &args.fields)?;
    let synth_result = crate::set::validate::synth_with_schema(
        cfg,
        &doc,
        &current_fm,
        &combined_fields,
        &args.field_json,
        &args.push,
        &args.pop,
        &args.remove,
        args.force,
    )?;

    let mut all_changes = synth_result.changes;
    let mut warnings = synth_result.warnings;
    // The post-state document schema resolution already ran against (NRN-119),
    // reused here so the sweep resolves field types under the INCOMING type
    // without rebuilding the overlay + Document clone.
    let effective_doc = synth_result.effective_doc;

    // 7. Wikilink resolution sweep for wikilink-typed fields. Resolve field
    // types against the POST-state document so a type-changing batch does not
    // resolve a field as a wikilink under the outgoing type's schema — the same
    // pre-state-schema hazard fixed in synth_with_schema (NRN-119).
    for change in &all_changes {
        let Some(field_name) = change.field.as_deref() else {
            continue;
        };
        let Some(field_type) =
            crate::set::validate::lookup_field_type(cfg, &effective_doc, field_name)
        else {
            continue;
        };
        if field_type != "wikilink" && field_type != "wikilink_or_list" {
            continue;
        }
        if let Some(new_value) = &change.new_value {
            match new_value {
                serde_json::Value::String(s) => {
                    warnings.extend(crate::set::validate::check_wikilink_resolution(
                        index, field_name, s,
                    ));
                }
                serde_json::Value::Array(items) => {
                    for item in items {
                        if let Some(s) = item.as_str() {
                            warnings.extend(crate::set::validate::check_wikilink_resolution(
                                index, field_name, s,
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // 8. Body op (if --body-from-stdin and content differs).
    let body_change = new_body
        .as_deref()
        .and_then(|nb| synth_body_op(&current_body, nb));
    let body_changed = body_change.is_some();
    let body_bytes_new = body_change.as_ref().and_then(|c| {
        c.new_value
            .as_ref()
            .and_then(|v| v.as_str())
            .map(|s| s.len())
    });
    if let Some(op) = body_change {
        all_changes.push(op);
    }

    // 9. Stamp path + document_hash on every change.
    let doc_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    for change in all_changes.iter_mut() {
        change.path = target_path.clone();
        if change.document_hash.is_empty() {
            change.document_hash = doc_hash.clone();
        }
        // Derive a stable change_id when empty.
        if change.change_id.is_empty() {
            use sha2::Digest as _;
            let mut hasher = sha2::Sha256::new();
            hasher.update(change.path.as_str().as_bytes());
            hasher.update(b"\0");
            hasher.update(change.operation.as_bytes());
            hasher.update(b"\0");
            hasher.update(change.field.as_deref().unwrap_or("").as_bytes());
            let digest = hasher.finalize();
            change.change_id = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        }
    }

    // 10. Wrap into RepairPlan.
    let n_changes = all_changes.len();
    let plan = crate::standards::RepairPlan {
        schema_version: crate::standards::REPAIR_PLAN_SCHEMA_VERSION,
        vault_root: cwd.to_path_buf(),
        source_filters: crate::standards::RepairPlanFilters::default(),
        summary: crate::standards::RepairPlanSummary {
            findings: n_changes,
            planned_changes: n_changes,
            skipped: crate::standards::SkippedSummary::default(),
        },
        changes: all_changes,
        skipped_findings: Vec::new(),
        footnotes: Vec::new(),
    };

    Ok(PreflightOutcome {
        plan,
        warnings,
        target: target_path,
        body_changed,
        body_bytes_new,
        body_bytes_old: current_body_len,
    })
}

/// Parse a document's content into (frontmatter_value, body_string).
/// Frontmatter is returned as a JSON Value (Object if present, else empty Object).
/// Body is the portion of the file after the closing `---\n`.
fn parse_doc(content: &str) -> anyhow::Result<(serde_json::Value, String)> {
    let mut diagnostics = Vec::new();
    let (frontmatter, _frontmatter_range, _body_str, body_start) =
        crate::frontmatter::extract_frontmatter(content, &mut diagnostics);

    if !diagnostics.is_empty() {
        anyhow::bail!(
            "frontmatter parse errors: {}",
            diagnostics
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    let fm = frontmatter.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    let body = content[body_start..].to_string();
    Ok((fm, body))
}

/// Produce a `replace_body` PlannedChange if the new body differs from the
/// current body byte-for-byte. Returns None when content is identical (no-op
/// write — caller should report `body_changed: false`).
pub fn synth_body_op(current_body: &str, new_body: &str) -> Option<PlannedChange> {
    if current_body == new_body {
        return None;
    }
    Some(PlannedChange {
        change_id: String::new(),
        path: camino::Utf8PathBuf::new(),
        document_hash: String::new(),
        finding_code: "operator-mutation".to_string(),
        finding_rule: None,
        repair_rule: "vault-set".to_string(),
        operation: "replace_body".to_string(),
        field: None,
        expected_old_value: None,
        new_value: Some(serde_json::Value::String(new_body.to_string())),
        destination: None,
        link_risk: None,
        warnings: vec![],
        force: false,
        parents: false,
    })
}

/// Read the target on disk, synthesize a `replace_body` op for `new_body`, and
/// stamp it (path + blake3 doc hash + change_id) exactly as preflight step 9
/// does. No-op (leaves `body_changed = false`) when the body is unchanged.
/// Shared by `vault.set` (MCP `body`) and `norn edit`.
pub(crate) fn inject_body_change(
    cwd: &camino::Utf8Path,
    outcome: &mut PreflightOutcome,
    new_body: &str,
) -> anyhow::Result<()> {
    let full_path = cwd.join(&outcome.target);
    let content = std::fs::read_to_string(full_path.as_std_path())
        .map_err(|e| anyhow::anyhow!("failed to read {full_path}: {e}"))?;
    let current_body = body_after_frontmatter(&content);

    let Some(mut op) = synth_body_op(&current_body, new_body) else {
        return Ok(());
    };

    let doc_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    op.path = outcome.target.clone();
    op.document_hash = doc_hash;
    if op.change_id.is_empty() {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(op.path.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(op.operation.as_bytes());
        hasher.update(b"\0");
        hasher.update(op.field.as_deref().unwrap_or("").as_bytes());
        let digest = hasher.finalize();
        op.change_id = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    }

    outcome.body_changed = true;
    outcome.body_bytes_old = current_body.len();
    outcome.body_bytes_new = Some(new_body.len());
    outcome.plan.changes.push(op);
    outcome.plan.summary.findings = outcome.plan.changes.len();
    outcome.plan.summary.planned_changes = outcome.plan.changes.len();
    Ok(())
}

/// Body portion of `content` (everything after the closing frontmatter),
/// matching `parse_doc`'s body slice.
pub(crate) fn body_after_frontmatter(content: &str) -> String {
    let mut diagnostics = Vec::new();
    let (_fm, _range, _body, body_start) =
        crate::frontmatter::extract_frontmatter(content, &mut diagnostics);
    content[body_start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Cache;

    fn fixture_cache() -> (tempfile::TempDir, Cache) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-set-resolve-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();

        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("notes")).unwrap();
        std::fs::write(tmp.path().join("notes/foo.md"), "---\ntype: note\n---\n").unwrap();
        std::fs::write(tmp.path().join("notes/bar.md"), "---\ntype: note\n---\n").unwrap();

        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();
        (tmp, cache)
    }

    #[test]
    fn resolve_target_accepts_relative_path() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "notes/foo.md").expect("path should resolve");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_accepts_bare_stem() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "foo").expect("stem should resolve");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_accepts_wikilink_shape_with_brackets() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "[[foo]]").expect("wikilink should resolve");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_strips_anchor_and_pipe_suffixes() {
        let (_tmp, cache) = fixture_cache();
        let path = resolve_target(&cache, "foo#section|alias").expect("should strip suffixes");
        assert_eq!(path.as_str(), "notes/foo.md");
    }

    #[test]
    fn resolve_target_returns_error_when_not_found() {
        let (_tmp, cache) = fixture_cache();
        let result = resolve_target(&cache, "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found") || err.contains("nonexistent"));
    }

    #[test]
    fn resolve_target_returns_error_when_ambiguous() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-set-ambig-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".norn")).unwrap();
        std::fs::write(tmp.path().join(".norn/config.yaml"), "validate: {}\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();
        std::fs::write(tmp.path().join("a/shared.md"), "---\ntype: note\n---\n").unwrap();
        std::fs::write(tmp.path().join("b/shared.md"), "---\ntype: note\n---\n").unwrap();

        let mut cache = Cache::open(&root).unwrap();
        cache.rebuild(&root).unwrap();

        let result = resolve_target(&cache, "shared");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("ambiguous"));
        assert!(err.contains("a/shared.md") || err.contains("b/shared.md"));
    }

    #[test]
    fn parse_kv_splits_at_first_equals() {
        let (k, v) = parse_kv("status=active").expect("should split");
        assert_eq!(k, "status");
        assert_eq!(v, "active");
    }

    #[test]
    fn parse_kv_keeps_equals_in_value() {
        let (k, v) = parse_kv("note=key=value=embedded").expect("should split");
        assert_eq!(k, "note");
        assert_eq!(v, "key=value=embedded");
    }

    #[test]
    fn parse_kv_accepts_colon_separator() {
        // ADR 0010 separator forgiveness: an assignment token spelled with the
        // predicate idiom `:` parses identically to the canonical `=` form.
        let (k, v) = parse_kv("status:active").expect("should split");
        assert_eq!(k, "status");
        assert_eq!(v, "active");
    }

    #[test]
    fn parse_kv_first_separator_wins_equals_before_colon() {
        // `=` comes first, so a value-embedded `:` is preserved verbatim.
        let (k, v) = parse_kv("ref=note:anchor").expect("should split");
        assert_eq!(k, "ref");
        assert_eq!(v, "note:anchor");
    }

    #[test]
    fn parse_kv_rejects_missing_equals() {
        assert!(parse_kv("statusonly").is_err());
    }

    #[test]
    fn parse_kv_rejects_empty_key() {
        assert!(parse_kv("=value").is_err());
    }

    #[test]
    fn detect_conflicts_passes_when_keys_are_disjoint() {
        let report = detect_cross_class_conflicts(
            &["tags=foo".to_string()],
            &[],
            &["aliases=bar".to_string()],
            &[],
            &["old_key".to_string()],
        );
        assert!(report.is_ok());
    }

    #[test]
    fn detect_conflicts_refuses_field_plus_push_on_same_key() {
        let report = detect_cross_class_conflicts(
            &["tags=foo".to_string()],
            &[],
            &["tags=bar".to_string()],
            &[],
            &[],
        );
        assert!(report.is_err());
        let err = report.unwrap_err().to_string();
        assert!(err.contains("tags"));
        assert!(err.contains("--field") && err.contains("--push"));
    }

    #[test]
    fn detect_conflicts_refuses_field_plus_remove_on_same_key() {
        let report = detect_cross_class_conflicts(
            &["name=foo".to_string()],
            &[],
            &[],
            &[],
            &["name".to_string()],
        );
        assert!(report.is_err());
    }

    #[test]
    fn detect_conflicts_allows_within_class_multi_instance() {
        let report = detect_cross_class_conflicts(
            &["tags=foo".to_string(), "tags=bar".to_string()],
            &[],
            &[],
            &[],
            &[],
        );
        assert!(report.is_ok());
    }

    #[test]
    fn detect_conflicts_refuses_field_plus_field_json_on_same_key() {
        // --field and --field-json target the same logical operation (set the
        // value). Cross-instance on the same key is ambiguous; refuse.
        let report = detect_cross_class_conflicts(
            &["count=42".to_string()],
            &["count=43".to_string()],
            &[],
            &[],
            &[],
        );
        assert!(report.is_err());
    }

    #[test]
    fn detect_conflicts_refuses_push_plus_pop_on_same_key() {
        let report = detect_cross_class_conflicts(
            &[],
            &[],
            &["tags=add".to_string()],
            &["tags=drop".to_string()],
            &[],
        );
        assert!(report.is_err());
    }

    // ── synth_frontmatter_ops tests ──────────────────────────────────────────

    use serde_json::json;

    #[test]
    fn synth_field_for_new_key_emits_add_frontmatter() {
        let current_frontmatter = json!({"title": "Foo"});
        let changes = synth_frontmatter_ops(
            &current_frontmatter,
            &["status=active".to_string()],
            &[],
            &[],
            &[],
            &[],
        )
        .expect("synth should succeed");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].operation, "add_frontmatter");
        assert_eq!(changes[0].field.as_deref(), Some("status"));
        assert_eq!(changes[0].new_value, Some(json!("active")));
    }

    #[test]
    fn synth_field_for_existing_key_emits_set_frontmatter_with_old_value() {
        let current_frontmatter = json!({"status": "draft"});
        let changes = synth_frontmatter_ops(
            &current_frontmatter,
            &["status=active".to_string()],
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].operation, "set_frontmatter");
        assert_eq!(changes[0].expected_old_value, Some(json!("draft")));
        assert_eq!(changes[0].new_value, Some(json!("active")));
    }

    #[test]
    fn synth_field_multi_instance_same_key_accumulates_array() {
        let current_frontmatter = json!({});
        let changes = synth_frontmatter_ops(
            &current_frontmatter,
            &["tags=foo".to_string(), "tags=bar".to_string()],
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].field.as_deref(), Some("tags"));
        assert_eq!(changes[0].new_value, Some(json!(["foo", "bar"])));
    }

    #[test]
    fn synth_field_json_parses_raw_json() {
        let current_frontmatter = json!({});
        let changes = synth_frontmatter_ops(
            &current_frontmatter,
            &[],
            &["count=42".to_string()],
            &[],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(changes[0].new_value, Some(json!(42)));
    }

    #[test]
    fn synth_field_json_with_malformed_json_errors() {
        let current_frontmatter = json!({});
        let result = synth_frontmatter_ops(
            &current_frontmatter,
            &[],
            &["data={not valid".to_string()],
            &[],
            &[],
            &[],
        );
        assert!(result.is_err());
    }

    #[test]
    fn synth_field_with_schema_silent_path_infers_bool_int_null() {
        let current_frontmatter = json!({});
        let changes = synth_frontmatter_ops(
            &current_frontmatter,
            &[
                "active=true".to_string(),
                "count=42".to_string(),
                "missing=null".to_string(),
                "name=alpha".to_string(),
            ],
            &[],
            &[],
            &[],
            &[],
        )
        .unwrap();
        let by_field: std::collections::BTreeMap<_, _> = changes
            .iter()
            .map(|c| (c.field.as_deref().unwrap_or(""), c.new_value.clone()))
            .collect();
        assert_eq!(by_field.get("active"), Some(&Some(json!(true))));
        assert_eq!(by_field.get("count"), Some(&Some(json!(42))));
        assert_eq!(by_field.get("missing"), Some(&Some(json!(null))));
        assert_eq!(by_field.get("name"), Some(&Some(json!("alpha"))));
    }

    #[test]
    fn synth_push_on_existing_array_appends() {
        let current = json!({"aliases": ["foo", "bar"]});
        let changes =
            synth_frontmatter_ops(&current, &[], &[], &["aliases=baz".to_string()], &[], &[])
                .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].operation, "set_frontmatter");
        assert_eq!(changes[0].new_value, Some(json!(["foo", "bar", "baz"])));
    }

    #[test]
    fn synth_push_on_missing_key_creates_single_element_array() {
        let current = json!({});
        let changes =
            synth_frontmatter_ops(&current, &[], &[], &["aliases=foo".to_string()], &[], &[])
                .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].operation, "add_frontmatter");
        assert_eq!(changes[0].new_value, Some(json!(["foo"])));
    }

    #[test]
    fn synth_pop_drops_matching_value() {
        let current = json!({"aliases": ["foo", "bar", "baz"]});
        let changes =
            synth_frontmatter_ops(&current, &[], &[], &[], &["aliases=bar".to_string()], &[])
                .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_value, Some(json!(["foo", "baz"])));
    }

    #[test]
    fn synth_pop_of_missing_value_emits_no_op_change() {
        let current = json!({"aliases": ["foo"]});
        let changes = synth_frontmatter_ops(
            &current,
            &[],
            &[],
            &[],
            &["aliases=missing".to_string()],
            &[],
        )
        .unwrap();
        assert_eq!(
            changes.len(),
            0,
            "pop of missing value should be a no-op (no change emitted)"
        );
    }

    #[test]
    fn synth_push_on_scalar_typed_field_returns_error() {
        let current = json!({"name": "scalar"});
        let result =
            synth_frontmatter_ops(&current, &[], &[], &["name=extra".to_string()], &[], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn synth_multi_push_same_key_accumulates() {
        let current = json!({"aliases": ["x"]});
        let changes = synth_frontmatter_ops(
            &current,
            &[],
            &[],
            &["aliases=a".to_string(), "aliases=b".to_string()],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].new_value, Some(json!(["x", "a", "b"])));
    }

    #[test]
    fn synth_remove_drops_key() {
        let current = json!({"priority": "high", "status": "draft"});
        let changes =
            synth_frontmatter_ops(&current, &[], &[], &[], &[], &["priority".to_string()]).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].operation, "remove_frontmatter");
        assert_eq!(changes[0].field.as_deref(), Some("priority"));
        assert_eq!(changes[0].expected_old_value, Some(json!("high")));
    }

    #[test]
    fn synth_remove_of_missing_key_emits_no_op() {
        let current = json!({"status": "draft"});
        let changes =
            synth_frontmatter_ops(&current, &[], &[], &[], &[], &["nonexistent".to_string()])
                .unwrap();
        assert_eq!(changes.len(), 0);
    }

    #[test]
    fn synth_refuses_cross_class_conflict() {
        let current = json!({});
        let result = synth_frontmatter_ops(
            &current,
            &["tags=foo".to_string()],
            &[],
            &["tags=bar".to_string()],
            &[],
            &[],
        );
        assert!(
            result.is_err(),
            "cross-class conflict on 'tags' should refuse"
        );
    }

    // ── synth_body_op tests ──────────────────────────────────────────────────

    #[test]
    fn synth_body_op_emitted_when_stdin_differs_from_current() {
        let current_body = "old body\n";
        let new_body = "new body\n";
        let op = synth_body_op(current_body, new_body);
        assert!(op.is_some());
        let op = op.unwrap();
        assert_eq!(op.operation, "replace_body");
        assert_eq!(op.new_value, Some(serde_json::json!("new body\n")));
    }

    #[test]
    fn synth_body_op_omitted_when_stdin_matches_current_byte_for_byte() {
        let current_body = "same body\n";
        let new_body = "same body\n";
        assert!(synth_body_op(current_body, new_body).is_none());
    }
}
