//! The `describe` verb's execute seam (the 0016 Params/execute/Report vocabulary).
//!
//! The vault STRUCTURE (folders, declared
//! path rules, creatable rules, inbox, the full frontmatter schema) always, plus
//! a CONTENTS-SUMMARY (totals, per-field distributions, date bounds, identity
//! skips) when `--data`/`--stats` is set or `--by` is non-empty.
//!
//! The structure view needs the parsed [`VaultConfig`], which the owner retains
//! from warm-up and passes in (`None` when the vault runs under no config file —
//! then only `folders` is populated and `schema` is the default validate config).
//!
//! The `Ok(Ok)` / `Ok(Err)` / `Err` contract matches the other read verbs; a
//! malformed data-mode predicate is the only user error.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde_json::Value;

use norn_wire::{
    DataSummary, DateBounds, DescribeParams, DescribeReport, FieldDistribution, PathRule,
    SkippedField, ValueCount,
};

use crate::cache::Cache;
use crate::domain::DocumentSummary;
use crate::query::filter_args::build_document_query;
use crate::read::render_key;
use crate::standards::config::VaultConfig;

/// The auto identity-skip threshold: a field with (distinct / occurrences) at or
/// above this is treated as an identity column and dropped from the auto
/// distributions.
const IDENTITY_RATIO: f64 = 0.9;

/// Default per-field value-bucket cap when `--limit` is absent.
const DEFAULT_LIMIT: usize = 20;

/// Run a `describe` request against the warm cache + retained config.
pub fn execute(
    cache: &Cache,
    config: Option<&VaultConfig>,
    params: &DescribeParams,
    today: &str,
) -> Result<Result<DescribeReport, String>> {
    let folders = collect_folders(cache)?;
    let path_rules = config.map(path_rules).unwrap_or_default();
    let creatable_rules = config.map(creatable_rules).unwrap_or_default();
    let inbox = config.and_then(|c| c.inbox.path.clone());
    let schema = match config {
        Some(c) => serde_json::to_value(&c.validate).unwrap_or(Value::Null),
        None => {
            serde_json::to_value(crate::standards::ValidateConfig::default()).unwrap_or(Value::Null)
        }
    };

    // `--by` implies data — on the *normalized* by, so a comma/whitespace-only
    // `--by` does not turn data on.
    let by = normalize_by(&params.by);
    let want_data = params.data || !by.is_empty();

    let data = if want_data {
        let types = crate::query::filter_args::PredicateFieldTypes::from_config(config);
        let query = match build_document_query(&params.filter, today, &types) {
            Ok(mut q) => {
                if !params.filter.links_to.is_empty() {
                    let index = cache.load_graph_index()?;
                    for target in &params.filter.links_to {
                        match crate::target::resolve_target_path(&index, target) {
                            Ok(path) => q.links_to.push(path),
                            Err(e) => return Ok(Err(e.to_string())),
                        }
                    }
                }
                q
            }
            Err(e) => return Ok(Err(e.to_string())),
        };
        let docs = cache.documents_matching(&query)?;
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        Some(summarize(&docs, config, &by, limit))
    } else {
        None
    };

    Ok(Ok(DescribeReport {
        folders,
        path_rules,
        creatable_rules,
        inbox,
        schema,
        data,
    }))
}

/// Distinct vault-relative directories that currently hold documents, sorted; the
/// vault root is `""`. One SELECT (the all-docs scan); parents folded in memory.
fn collect_folders(cache: &Cache) -> Result<Vec<String>> {
    let docs = cache.documents_matching(&crate::query::DocumentQuery::default())?;
    let mut folders: BTreeSet<String> = BTreeSet::new();
    for doc in &docs {
        let path = doc.path.as_str();
        let parent = match path.rfind('/') {
            Some(idx) => &path[..idx],
            None => "",
        };
        folders.insert(parent.to_string());
    }
    Ok(folders.into_iter().collect())
}

/// Declared path rules: one per validate rule with a `match.path` glob.
fn path_rules(config: &VaultConfig) -> Vec<PathRule> {
    config
        .validate
        .rules
        .iter()
        .filter_map(|rule| {
            rule.r#match.path.as_ref().map(|glob| PathRule {
                glob: glob.clone(),
                name: rule.name.clone(),
                frontmatter_defaults: serde_json::to_value(&rule.frontmatter_defaults)
                    .unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Creatable rules: one per validate rule that declares BOTH a name and a target.
fn creatable_rules(config: &VaultConfig) -> Vec<norn_wire::CreatableRule> {
    config
        .validate
        .rules
        .iter()
        .filter_map(|rule| {
            let name = rule.name.as_ref()?;
            let target = rule.target.as_ref()?;
            Some(norn_wire::CreatableRule {
                name: name.clone(),
                target: target.clone(),
                required_vars: referenced_vars(target),
                frontmatter_defaults: serde_json::to_value(&rule.frontmatter_defaults)
                    .unwrap_or(Value::Null),
                body: rule.body.clone(),
            })
        })
        .collect()
}

/// Collect the `{{var.X}}` / `{{path.X}}` variable names a target template
/// references, in first-occurrence order.
fn referenced_vars(target: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = target;
    while let Some(open) = rest.find("{{") {
        // Quad-brace `{{{{` is a literal-`{{` escape — skip all four.
        if rest[open..].starts_with("{{{{") {
            rest = &rest[open + 4..];
            continue;
        }
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else { break };
        let inner = after[..close].trim();
        rest = &after[close + 2..];
        // Strip pipe transforms — only the variable portion matters.
        let token = inner.split('|').next().unwrap_or(inner).trim();
        if let Some(name) = token
            .strip_prefix("var.")
            .or_else(|| token.strip_prefix("path."))
        {
            let name = name.trim();
            if !name.is_empty() && !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

// ── Contents summary ──────────────────────────────────────────────────────────

/// Trim each `--by` entry and drop empties (clap's `value_delimiter` does not
/// trim). Idempotent — the CLI gate and this both apply it.
fn normalize_by(by: &[String]) -> Vec<String> {
    by.iter()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Build the contents-summary. `by` is already normalized; empty means auto mode.
fn summarize(
    docs: &[DocumentSummary],
    config: Option<&VaultConfig>,
    by: &[String],
    limit: usize,
) -> DataSummary {
    let dfields = date_fields(config);
    // Date bounds only in auto mode (`--by` returns no auto bounds — F4).
    let dates = if by.is_empty() {
        date_bounds(docs, &dfields)
    } else {
        Vec::new()
    };
    // Exclude from auto distributions only date fields that ACTUALLY produced
    // bounds (F3): a mistyped date field with no valid values stays a normal
    // distribution.
    let bounded: BTreeSet<String> = dates.iter().map(|d| d.field.clone()).collect();

    let (fields, skipped) = if by.is_empty() {
        let auto = auto_fields(docs, &bounded);
        field_distributions(docs, &auto, false, limit)
    } else {
        field_distributions(docs, by, true, limit)
    };

    DataSummary {
        total: docs.len(),
        fields,
        dates,
        skipped,
    }
}

/// Sorted union of every frontmatter object key across all docs, minus `exclude`.
fn auto_fields(docs: &[DocumentSummary], exclude: &BTreeSet<String>) -> Vec<String> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for doc in docs {
        if let Some(Value::Object(obj)) = &doc.frontmatter {
            for key in obj.keys() {
                if !exclude.contains(key) {
                    keys.insert(key.clone());
                }
            }
        }
    }
    keys.into_iter().collect()
}

/// The rendered present values for `field` in `doc`. `None` ONLY when the key is
/// truly absent (the sole `(missing)` contributor). An empty array is one `[]`
/// bucket; a non-empty array flattens to one bucket per element; a scalar (incl.
/// null) is one bucket.
fn present_values(doc: &DocumentSummary, field: &str) -> Option<Vec<String>> {
    let v = doc.frontmatter.as_ref()?.get(field)?;
    match v {
        Value::Array(items) if items.is_empty() => Some(vec![render_key(v)]),
        Value::Array(items) => Some(items.iter().map(render_key).collect()),
        other => Some(vec![render_key(other)]),
    }
}

/// Per-field distributions with the identity-skip (auto only), `(missing)`
/// bucket, count-desc / value-asc sort, and the limit cap.
fn field_distributions(
    docs: &[DocumentSummary],
    fields: &[String],
    explicit: bool,
    limit: usize,
) -> (Vec<FieldDistribution>, Vec<SkippedField>) {
    let mut out_fields: Vec<FieldDistribution> = Vec::new();
    let mut skipped: Vec<SkippedField> = Vec::new();

    for field in fields {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut distinct: BTreeSet<String> = BTreeSet::new();
        let mut missing = 0usize;

        for doc in docs {
            match present_values(doc, field) {
                None => missing += 1,
                Some(values) => {
                    for value in values {
                        distinct.insert(value.clone());
                        *counts.entry(value).or_insert(0) += 1;
                    }
                }
            }
        }

        let occurrences: usize = counts.values().sum();
        // A field carried by nobody contributes nothing.
        if occurrences == 0 {
            continue;
        }

        // Identity-skip (auto only): as many distinct values as occurrences.
        let ratio = distinct.len() as f64 / occurrences as f64;
        if !explicit && ratio >= IDENTITY_RATIO {
            skipped.push(SkippedField {
                field: field.clone(),
                distinct: distinct.len(),
                total: occurrences,
            });
            continue;
        }

        // `(missing)` is added AFTER the ratio test so it never affects distinct
        // or occurrences.
        if missing > 0 {
            counts.insert(crate::read::MISSING.to_string(), missing);
        }

        let mut values: Vec<ValueCount> = counts
            .into_iter()
            .map(|(value, count)| ValueCount { value, count })
            .collect();
        // Count descending, then value ascending as the deterministic tie-break.
        values.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));

        let total_buckets = values.len();
        let shown = if limit == 0 {
            total_buckets
        } else {
            limit.min(total_buckets)
        };
        let more = total_buckets - shown;
        values.truncate(shown);

        out_fields.push(FieldDistribution {
            field: field.clone(),
            values,
            more,
        });
    }

    (out_fields, skipped)
}

/// The set of fields declared `date` / `datetime` in any validate rule.
fn date_fields(config: Option<&VaultConfig>) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let Some(config) = config else {
        return out;
    };
    for rule in &config.validate.rules {
        for (field, spec) in &rule.field_types {
            if matches!(spec.type_name(), Some("date") | Some("datetime")) {
                out.insert(field.clone());
            }
        }
    }
    out
}

/// Lexical min/max over valid ISO date/datetime string values (NRN-110: lexical,
/// not chronological). String-typed values only; a field yields bounds only when
/// at least one valid value is present.
fn date_bounds(docs: &[DocumentSummary], fields: &BTreeSet<String>) -> Vec<DateBounds> {
    let mut out: Vec<DateBounds> = Vec::new();
    for field in fields {
        let mut min: Option<String> = None;
        let mut max: Option<String> = None;
        for doc in docs {
            let Some(raw) = doc
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.get(field))
                .and_then(Value::as_str)
            else {
                continue;
            };
            // Validity gate: the same predicates `set`/coercion use.
            if !crate::standards::predicates::is_date_string(raw)
                && !crate::standards::predicates::is_datetime_string(raw)
            {
                continue;
            }
            if min.as_deref().is_none_or(|m| raw < m) {
                min = Some(raw.to_string());
            }
            if max.as_deref().is_none_or(|m| raw > m) {
                max = Some(raw.to_string());
            }
        }
        if let (Some(min), Some(max)) = (min, max) {
            out.push(DateBounds {
                field: field.clone(),
                min,
                max,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use norn_wire::FilterParams;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-18";

    fn vault() -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        std::fs::create_dir(root.join("notes").as_std_path()).unwrap();
        for (path, body) in [
            ("notes/a.md", "---\ntype: note\nstatus: active\n---\n"),
            ("notes/b.md", "---\ntype: task\nstatus: active\n---\n"),
            ("c.md", "---\ntype: task\n---\n"),
        ] {
            std::fs::write(root.join(path).as_std_path(), body).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    #[test]
    fn structure_only_reports_folders_no_data() {
        let (_t, root) = vault();
        let cache = built(&root);
        let report = execute(&cache, None, &DescribeParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report.folders, vec!["".to_string(), "notes".to_string()]);
        assert!(report.data.is_none());
        assert!(report.path_rules.is_empty());
    }

    #[test]
    fn data_mode_distributes_and_buckets_missing() {
        let (_t, root) = vault();
        let cache = built(&root);
        let params = DescribeParams {
            data: true,
            ..Default::default()
        };
        let report = execute(&cache, None, &params, TODAY).unwrap().unwrap();
        let data = report.data.unwrap();
        assert_eq!(data.total, 3);
        let status = data.fields.iter().find(|f| f.field == "status").unwrap();
        // active 2, then (missing) 1.
        assert_eq!(status.values[0].value, "active");
        assert_eq!(status.values[0].count, 2);
        assert!(status
            .values
            .iter()
            .any(|v| v.value == "(missing)" && v.count == 1));
    }

    #[test]
    fn by_implies_data_and_bypasses_identity_skip() {
        let (_t, root) = vault();
        let cache = built(&root);
        let params = DescribeParams {
            by: vec!["type".into()],
            ..Default::default()
        };
        let report = execute(&cache, None, &params, TODAY).unwrap().unwrap();
        let data = report.data.unwrap();
        assert_eq!(data.fields.len(), 1);
        assert_eq!(data.fields[0].field, "type");
    }

    #[test]
    fn whitespace_only_by_does_not_enable_data() {
        let (_t, root) = vault();
        let cache = built(&root);
        let params = DescribeParams {
            by: vec![" ".into(), "".into()],
            ..Default::default()
        };
        let report = execute(&cache, None, &params, TODAY).unwrap().unwrap();
        assert!(
            report.data.is_none(),
            "comma/whitespace --by must not enable data"
        );
    }

    #[test]
    fn filter_narrows_data_docset() {
        let (_t, root) = vault();
        let cache = built(&root);
        let params = DescribeParams {
            data: true,
            filter: FilterParams {
                eq: vec!["type:task".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let report = execute(&cache, None, &params, TODAY).unwrap().unwrap();
        assert_eq!(report.data.unwrap().total, 2);
    }

    #[test]
    fn data_mode_refuses_bad_predicate_input() {
        // NRN-427/NRN-428: `describe` is the third `build_document_query`
        // consumer (data mode), so it must refuse a non-ISO date value and a
        // malformed `--path` glob identically to find/count — a user error, not a
        // silent empty/wrong summary.
        let (_t, root) = vault();
        let cache = built(&root);

        let bad_date = DescribeParams {
            data: true,
            filter: FilterParams {
                before: vec!["created:yesterday".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let outcome = execute(&cache, None, &bad_date, TODAY).unwrap();
        assert!(outcome.is_err(), "non-ISO --before value must refuse");

        let bad_glob = DescribeParams {
            data: true,
            filter: FilterParams {
                path: vec!["{unclosed".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let outcome = execute(&cache, None, &bad_glob, TODAY).unwrap();
        assert!(outcome.is_err(), "malformed --path glob must refuse");
        assert!(outcome.unwrap_err().contains("--path"));
    }

    #[test]
    fn referenced_vars_collects_var_and_path_tokens() {
        assert_eq!(
            referenced_vars("Workspaces/{{var.workspace}}/tasks/{{title|slugify}}.md"),
            vec!["workspace".to_string()]
        );
        assert_eq!(
            referenced_vars("{{path.year}}/{{path.year}}-note.md"),
            vec!["year".to_string()]
        );
    }
}
