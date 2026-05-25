//! `vault set` plan synthesis: CLI args → RepairPlan.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use camino::Utf8PathBuf;
use serde_json::Value;
use vault_cache::Cache;
use vault_standards::PlannedChange;

/// Resolve the user-supplied DOC argument into a vault-relative path.
/// Accepts path, stem, or wikilink-shaped input (with or without [[]]).
/// Anchor / block-ref / pipe-alias suffixes are stripped before resolution.
///
/// Refuses (Err) when:
/// - The target doesn't resolve to any doc.
/// - The target resolves to multiple docs (ambiguous stem).
#[allow(dead_code)] // wired in when Command::Set handler lands (Task 2.2)
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

/// Split `KEY=VALUE` at the first `=`. Returns Err on missing `=` or empty KEY.
/// VALUE may contain additional `=` characters (preserved verbatim).
#[allow(dead_code)] // wired in during Task 2.6 (plan synthesis)
pub fn parse_kv(raw: &str) -> Result<(String, String)> {
    let (k, v) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("expected KEY=VALUE, got: {raw}"))?;
    if k.is_empty() {
        bail!("KEY cannot be empty in: {raw}");
    }
    Ok((k.to_string(), v.to_string()))
}

/// Refuse with a clear error if any key appears across multiple mutation
/// classes (--field/--field-json/--push/--pop/--remove). Within-class
/// multi-instance is fine (accumulation semantics).
///
/// --field and --field-json are treated as a single class for this purpose:
/// both write a value to the key, and using both for the same key is
/// ambiguous.
#[allow(dead_code)] // wired in during Task 2.6 (plan synthesis)
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
#[allow(dead_code)] // wired in when synth_frontmatter_ops is called from Command::Set handler
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
#[allow(dead_code)] // wired in when synth_frontmatter_ops is called from Command::Set handler
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
    }
}

/// Schema-silent plan synthesis from CLI args.
///
/// Uses light type inference for --field values (bool/int/null + string
/// fallback). Push/pop arrays are resolved against the current value at synth
/// time.
///
/// Refuses on:
/// - Malformed JSON in --field-json
///
/// Silent no-ops: none yet (push/pop/remove added in the next task).
#[allow(dead_code)] // wired in when Command::Set handler lands (Phase 5)
pub fn synth_frontmatter_ops(
    current_frontmatter: &Value,
    fields: &[String],
    field_json: &[String],
    push: &[String],
    pop: &[String],
    remove: &[String],
) -> Result<Vec<PlannedChange>> {
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

    // push/pop/remove args are accepted but not yet processed (added Task 3.2).
    let _ = (push, pop, remove);

    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vault_cache::Cache;

    fn fixture_cache() -> (tempfile::TempDir, Cache) {
        let tmp = tempfile::Builder::new()
            .prefix("vault-cli-set-resolve-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();

        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
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
            .prefix("vault-cli-set-ambig-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8Path::from_path(tmp.path())
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(tmp.path().join(".vault")).unwrap();
        std::fs::write(tmp.path().join(".vault/config.yaml"), "validate: {}\n").unwrap();
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
}
