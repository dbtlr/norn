//! Vault contents-summary (`describe --data`): field distributions,
//! identity-skip, and (Task 3) date bounds. Pure over `DocumentSummary`.

// These items are pub for Task 3 wiring into `describe::mod` / the CLI; the
// binary doesn't call them yet.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::core::DocumentSummary;

/// Placeholder root summary type; the full shape (incl. `dates`) lands in
/// Task 3. Kept minimal here so `DescribeOutput { data: Option<DataSummary> }`
/// (Task 1) keeps compiling.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DataSummary {
    pub total: usize,
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

/// The present rendered values for `field` on `doc`: `None` when absent,
/// else ≥1 element (arrays flattened per-element).
fn present_values(doc: &DocumentSummary, field: &str) -> Option<Vec<String>> {
    let v = doc.frontmatter.as_ref()?.get(field)?;
    match v {
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
        assert_eq!(skipped[0].total, 2, "denominator = docs carrying the field");
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
}
