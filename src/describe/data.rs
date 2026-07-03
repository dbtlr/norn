//! Vault contents-summary (`describe --data`): field distributions,
//! identity-skip, and (Task 3) date bounds. Pure over `DocumentSummary`.

// These items are pub for Task 3 wiring into `describe::mod` / the CLI; the
// binary doesn't call them yet.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::config_loader::LoadedConfig;
use crate::core::DocumentSummary;

/// Chronological min/max for one date/datetime field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct DateBounds {
    pub field: String,
    pub min: String,
    pub max: String,
}

/// The full vault contents-summary: field distributions, date bounds, and
/// identity-skipped fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct DataSummary {
    pub total: usize,
    pub fields: Vec<FieldDistribution>,
    pub dates: Vec<DateBounds>,
    pub skipped: Vec<SkippedField>,
}

/// The bucket key for a document that lacks the field. Excluded from the
/// distinct-value count and the identity-ratio denominator, but shown as a
/// bucket when the field survives.
pub const MISSING: &str = "(missing)";

#[derive(Debug, Clone)]
pub struct DataOptions {
    /// Explicit fields (`--by`); empty ⇒ auto-select by cardinality.
    pub by: Vec<String>,
    /// Max value-buckets shown per field; 0 ⇒ no cap.
    pub limit: usize,
    /// Skip a field from distributions when distinct/occurrences ≥ this ratio.
    /// The denominator is total value-occurrences (present-value count summed
    /// across docs), not docs-carrying-the-field: for multi-valued/array
    /// fields, flattening per-element can make distinct values equal or
    /// exceed the doc count, so docs-carrying would misclassify them as
    /// identity. For scalar fields the two denominators are identical.
    pub identity_ratio: f64,
}

impl Default for DataOptions {
    fn default() -> Self {
        Self {
            by: Vec::new(),
            limit: 20,
            identity_ratio: 0.9,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ValueCount {
    pub value: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct FieldDistribution {
    pub field: String,
    pub values: Vec<ValueCount>,
    pub more: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct SkippedField {
    pub field: String,
    pub distinct: usize,
    /// Total value-occurrences (not docs-carrying-the-field) — see
    /// `DataOptions::identity_ratio` for why.
    pub total: usize,
}

/// Sorted union of frontmatter keys present across `docs`, minus `exclude`.
pub fn auto_fields(docs: &[DocumentSummary], exclude: &BTreeSet<String>) -> Vec<String> {
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for doc in docs {
        if let Some(serde_json::Value::Object(map)) = &doc.frontmatter {
            for k in map.keys() {
                if !exclude.contains(k) {
                    keys.insert(k.clone());
                }
            }
        }
    }
    keys.into_iter().collect()
}

/// The present rendered values for `field` on `doc`: `None` when absent or
/// an empty array (no value, like an absent field), else ≥1 element (arrays
/// flattened per-element).
fn present_values(doc: &DocumentSummary, field: &str) -> Option<Vec<String>> {
    let v = doc.frontmatter.as_ref()?.get(field)?;
    match v {
        serde_json::Value::Array(items) if items.is_empty() => None,
        serde_json::Value::Array(items) => {
            Some(items.iter().map(crate::count::render_key).collect())
        }
        other => Some(vec![crate::count::render_key(other)]),
    }
}

pub fn field_distributions(
    docs: &[DocumentSummary],
    fields: &[String],
    explicit: bool,
    opts: &DataOptions,
) -> (Vec<FieldDistribution>, Vec<SkippedField>) {
    let mut dists = Vec::new();
    let mut skipped = Vec::new();

    for field in fields {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut distinct: BTreeSet<String> = BTreeSet::new();
        let mut missing = 0usize;

        for doc in docs {
            match present_values(doc, field) {
                Some(values) => {
                    for v in values {
                        distinct.insert(v.clone());
                        *counts.entry(v).or_insert(0) += 1;
                    }
                }
                None => missing += 1,
            }
        }

        // Total value-occurrences (sum of present-value counts, pre-`(missing)`
        // bucket). For scalar fields this equals docs-carrying-the-field, but
        // for multi-valued/array fields (flattened per-element) it does not:
        // docs-carrying would misclassify e.g. 2 docs whose flattened tags
        // produce 2 distinct values as 100% identity, when only 2 *documents*
        // carry the field. Using occurrences keeps `distinct <= total`.
        let occurrences: usize = counts.values().sum();

        // A field carried by nobody contributes nothing.
        if occurrences == 0 {
            continue;
        }

        // Identity-skip (auto only): distinct ≈ occurrences ⇒ identifier/free-text.
        let ratio = distinct.len() as f64 / occurrences as f64;
        if !explicit && ratio >= opts.identity_ratio {
            skipped.push(SkippedField {
                field: field.clone(),
                distinct: distinct.len(),
                total: occurrences,
            });
            continue;
        }

        if missing > 0 {
            counts.insert(MISSING.to_string(), missing);
        }

        // Sort by count desc, then value asc (deterministic tie-break).
        let mut buckets: Vec<ValueCount> = counts
            .into_iter()
            .map(|(value, count)| ValueCount { value, count })
            .collect();
        buckets.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));

        let total_buckets = buckets.len();
        let shown = if opts.limit == 0 {
            total_buckets
        } else {
            opts.limit.min(total_buckets)
        };
        let more = total_buckets - shown;
        buckets.truncate(shown);

        dists.push(FieldDistribution {
            field: field.clone(),
            values: buckets,
            more,
        });
    }

    (dists, skipped)
}

/// Fields declared `date` or `datetime` in any rule's `field_types`.
pub fn date_fields(config: &LoadedConfig) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for rule in &config.validate.rules {
        for (field, spec) in &rule.field_types {
            if matches!(spec.type_name(), Some("date") | Some("datetime")) {
                out.insert(field.clone());
            }
        }
    }
    out
}

/// Min/max per date field. Values are normalized via the same date/datetime
/// coercion `set` uses, so ISO lexical order == chronological order. Values
/// that fail to coerce fall back to the raw string (still ISO-comparable for
/// well-formed inputs); a field with no present values contributes no bounds.
pub fn date_bounds(docs: &[DocumentSummary], fields: &BTreeSet<String>) -> Vec<DateBounds> {
    let mut out = Vec::new();
    for field in fields {
        let mut min: Option<String> = None;
        let mut max: Option<String> = None;
        for doc in docs {
            let Some(raw) = doc
                .frontmatter
                .as_ref()
                .and_then(|fm| fm.get(field))
                .and_then(|v| v.as_str())
            else {
                continue;
            };
            // Normalize to ISO; skip unparseable. `datetime` accepts both.
            let normalized = crate::set::validate::coerce_value_for_type("datetime", raw, None)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| raw.to_string());
            if min.as_ref().is_none_or(|m| &normalized < m) {
                min = Some(normalized.clone());
            }
            if max.as_ref().is_none_or(|m| &normalized > m) {
                max = Some(normalized);
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

/// Assemble the full contents-summary. Auto mode excludes date fields from
/// distributions (they get bounds instead); `--by` mode summarizes exactly
/// the named fields and bypasses identity-skip.
pub fn summarize(
    docs: &[DocumentSummary],
    config: &LoadedConfig,
    opts: &DataOptions,
) -> DataSummary {
    let dfields = date_fields(config);
    let (fields, skipped) = if opts.by.is_empty() {
        let auto = auto_fields(docs, &dfields);
        field_distributions(docs, &auto, false, opts)
    } else {
        field_distributions(docs, &opts.by, true, opts)
    };
    let dates = date_bounds(docs, &dfields);
    DataSummary {
        total: docs.len(),
        fields,
        dates,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::DocumentSummary;
    use camino::Utf8PathBuf;

    fn doc(fm: serde_json::Value) -> DocumentSummary {
        DocumentSummary {
            path: Utf8PathBuf::from("x.md"),
            stem: "x".to_string(),
            hash: "h".to_string(),
            frontmatter: Some(fm),
            body_text: String::new(),
        }
    }

    #[test]
    fn distribution_counts_scalar_values_desc() {
        let docs = vec![
            doc(serde_json::json!({"type": "note"})),
            doc(serde_json::json!({"type": "note"})),
            doc(serde_json::json!({"type": "task"})),
        ];
        let opts = DataOptions::default();
        let (dists, skipped) = field_distributions(&docs, &["type".to_string()], false, &opts);
        assert!(skipped.is_empty());
        assert_eq!(dists.len(), 1);
        assert_eq!(dists[0].field, "type");
        assert_eq!(
            dists[0].values,
            vec![
                ValueCount {
                    value: "note".into(),
                    count: 2
                },
                ValueCount {
                    value: "task".into(),
                    count: 1
                },
            ]
        );
        assert_eq!(dists[0].more, 0);
    }

    #[test]
    fn arrays_flatten_per_element_counts_can_exceed_total() {
        let docs = vec![
            doc(serde_json::json!({"tags": ["rust", "design"]})),
            doc(serde_json::json!({"tags": ["rust"]})),
        ];
        let opts = DataOptions::default();
        let (dists, _) = field_distributions(&docs, &["tags".to_string()], false, &opts);
        // rust: 2, design: 1 — sum (3) > doc count (2).
        assert_eq!(
            dists[0].values,
            vec![
                ValueCount {
                    value: "rust".into(),
                    count: 2
                },
                ValueCount {
                    value: "design".into(),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn identity_field_skipped_by_ratio() {
        let docs = vec![
            doc(serde_json::json!({"title": "A"})),
            doc(serde_json::json!({"title": "B"})),
            doc(serde_json::json!({"title": "C"})),
        ];
        let opts = DataOptions::default();
        let (dists, skipped) = field_distributions(&docs, &["title".to_string()], false, &opts);
        assert!(dists.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].field, "title");
        assert_eq!(skipped[0].distinct, 3);
        assert_eq!(skipped[0].total, 3);
    }

    #[test]
    fn explicit_by_bypasses_identity_skip() {
        let docs = vec![
            doc(serde_json::json!({"title": "A"})),
            doc(serde_json::json!({"title": "B"})),
        ];
        let opts = DataOptions::default();
        let (dists, skipped) = field_distributions(&docs, &["title".to_string()], true, &opts);
        assert!(skipped.is_empty(), "explicit --by must not skip");
        assert_eq!(dists[0].values.len(), 2);
    }

    #[test]
    fn ratio_denominator_excludes_missing() {
        // slug present on 2 of 4 docs, both distinct → ratio 2/2 = 1.0 → skip.
        let docs = vec![
            doc(serde_json::json!({"slug": "a"})),
            doc(serde_json::json!({"slug": "b"})),
            doc(serde_json::json!({"other": 1})),
            doc(serde_json::json!({"other": 1})),
        ];
        let opts = DataOptions::default();
        let (_dists, skipped) = field_distributions(&docs, &["slug".to_string()], false, &opts);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].total, 2, "denominator = total value-occurrences");
    }

    #[test]
    fn empty_array_counts_as_missing() {
        // A doc carrying `field: []` has no value — treat it as missing, not
        // as a distinct value and not as an uncounted occurrence. Two docs
        // share the "rust" tag so the identity-ratio (distinct/occurrences)
        // stays below the default 0.9 skip threshold and the field survives
        // into a distribution, letting us inspect its buckets.
        let docs = vec![
            doc(serde_json::json!({"tags": []})),
            doc(serde_json::json!({"tags": ["rust"]})),
            doc(serde_json::json!({"tags": ["rust"]})),
        ];
        let opts = DataOptions::default();
        let (dists, skipped) = field_distributions(&docs, &["tags".to_string()], false, &opts);
        assert!(skipped.is_empty());
        assert_eq!(dists.len(), 1);
        let vals: Vec<_> = dists[0]
            .values
            .iter()
            .map(|v| (v.value.as_str(), v.count))
            .collect();
        assert!(vals.contains(&("rust", 2)));
        assert!(vals.contains(&(MISSING, 1)));
        assert_eq!(dists[0].values.len(), 2, "no distinct bucket for []");
    }

    #[test]
    fn missing_is_a_bucket_but_not_counted_as_distinct() {
        // status: 3 docs "done", 1 doc absent → survives (ratio 1/3), (missing) bucket present.
        let docs = vec![
            doc(serde_json::json!({"status": "done"})),
            doc(serde_json::json!({"status": "done"})),
            doc(serde_json::json!({"status": "done"})),
            doc(serde_json::json!({"other": 1})),
        ];
        let opts = DataOptions::default();
        let (dists, skipped) = field_distributions(&docs, &["status".to_string()], false, &opts);
        assert!(skipped.is_empty());
        let vals: Vec<_> = dists[0]
            .values
            .iter()
            .map(|v| (v.value.as_str(), v.count))
            .collect();
        assert!(vals.contains(&("done", 3)));
        assert!(vals.contains(&(MISSING, 1)));
    }

    #[test]
    fn limit_truncates_and_reports_more() {
        let docs: Vec<_> = ["a", "b", "c", "d", "e"]
            .iter()
            .enumerate()
            .flat_map(|(i, v)| {
                // give each value a distinct count so ordering is stable: a×5,b×4,...
                (0..(5 - i)).map(move |_| doc(serde_json::json!({"k": v})))
            })
            .collect();
        let opts = DataOptions {
            limit: 2,
            ..DataOptions::default()
        };
        let (dists, _) = field_distributions(&docs, &["k".to_string()], true, &opts);
        assert_eq!(dists[0].values.len(), 2);
        assert_eq!(
            dists[0].values[0],
            ValueCount {
                value: "a".into(),
                count: 5
            }
        );
        assert_eq!(dists[0].more, 3);
    }

    #[test]
    fn limit_zero_means_no_cap() {
        let docs: Vec<_> = ["a", "b", "c"]
            .iter()
            .map(|v| doc(serde_json::json!({"k": v})))
            .collect();
        let opts = DataOptions {
            limit: 0,
            ..DataOptions::default()
        };
        let (dists, _) = field_distributions(&docs, &["k".to_string()], true, &opts);
        assert_eq!(dists[0].values.len(), 3);
        assert_eq!(dists[0].more, 0);
    }

    #[test]
    fn auto_fields_unions_present_keys_minus_exclude() {
        let docs = vec![
            doc(serde_json::json!({"type": "note", "created": "2026-01-01"})),
            doc(serde_json::json!({"status": "done"})),
        ];
        let mut exclude = std::collections::BTreeSet::new();
        exclude.insert("created".to_string());
        let fields = auto_fields(&docs, &exclude);
        assert_eq!(fields, vec!["status".to_string(), "type".to_string()]);
    }

    use crate::config_loader::LoadedConfig;

    /// A `LoadedConfig` whose `validate` has one rule typing `field` as `ty`,
    /// exercised through the real loader (a temp `.norn/config.yaml` parsed
    /// by `load_config`) rather than a hand-built struct — the date-field
    /// detection must see genuine parsing, including `deny_unknown_fields`
    /// and the `field_types` untagged-enum shape.
    fn config_with_date_field(field: &str, ty: &str) -> LoadedConfig {
        let dir = tempfile::Builder::new()
            .prefix("norn-describe-data-config-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let config_dir = root.join(".norn");
        std::fs::create_dir_all(&config_dir).unwrap();
        let yaml = format!(
            "validate:\n  rules:\n    - name: r\n      field_types:\n        {field}: {ty}\n"
        );
        std::fs::write(config_dir.join("config.yaml"), yaml).unwrap();
        crate::config_loader::load_config(&root, None).expect("parse config")
    }

    #[test]
    fn date_fields_reads_date_and_datetime_types() {
        let cfg = config_with_date_field("created", "datetime");
        let fields = date_fields(&cfg);
        assert!(fields.contains("created"));
    }

    #[test]
    fn date_bounds_computes_chronological_min_max() {
        let docs = vec![
            doc(serde_json::json!({"created": "2026-05-10"})),
            doc(serde_json::json!({"created": "2026-07-03"})),
            doc(serde_json::json!({"created": "2026-06-01"})),
        ];
        let mut df = std::collections::BTreeSet::new();
        df.insert("created".to_string());
        let bounds = date_bounds(&docs, &df);
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].field, "created");
        assert_eq!(bounds[0].min, "2026-05-10");
        assert_eq!(bounds[0].max, "2026-07-03");
    }

    #[test]
    fn summarize_excludes_date_fields_from_distributions() {
        // Both docs share `type: note` (rather than the brief's distinct
        // note/task pair) so the non-date field's identity-ratio
        // (distinct/occurrences) stays below the default 0.9 skip threshold.
        // With 2 docs and 2 *distinct* type values the ratio would hit 1.0
        // and `type` would land in `skipped` via the Task-2 identity-skip —
        // a real interaction with that already-shipped heuristic, not a
        // Task-3 bug — so the fixture is widened here rather than weakening
        // the assertions below (`total`, `dates`, and the fields-membership
        // checks are unchanged from the brief).
        let docs = vec![
            doc(serde_json::json!({"type": "note", "created": "2026-05-10"})),
            doc(serde_json::json!({"type": "note", "created": "2026-07-03"})),
        ];
        let cfg = config_with_date_field("created", "datetime");
        let summary = summarize(&docs, &cfg, &DataOptions::default());
        assert_eq!(summary.total, 2);
        assert!(summary.dates.iter().any(|d| d.field == "created"));
        assert!(
            !summary.fields.iter().any(|f| f.field == "created"),
            "date field must not appear as a distribution"
        );
        assert!(summary.fields.iter().any(|f| f.field == "type"));
    }
}
