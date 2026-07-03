//! Vault contents-summary (`describe --data`): field distributions,
//! identity-skip, and (Task 3) date bounds. Pure over `DocumentSummary`.

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
/// bucket when the field survives. Shared with `count::doc_key` so the two
/// surfaces cannot drift on the literal.
pub(crate) use crate::count::MISSING;

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
/// explicit `null`, or an empty array (no value, like an absent field), else
/// ≥1 element (arrays flattened per-element). Treating `null` as missing
/// matches the `has`/`missing` query-filter convention (null is absent),
/// even though `count::render_key` — used for `--by` grouping — renders null
/// as a `"(null)"` bucket on purpose; the two surfaces are allowed to diverge
/// here.
fn present_values(doc: &DocumentSummary, field: &str) -> Option<Vec<String>> {
    let v = doc.frontmatter.as_ref()?.get(field)?;
    match v {
        serde_json::Value::Null => None,
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

/// Min/max per date field, computed by lexical (string) comparison over
/// validated ISO values. This matches norn's substrate-wide date comparison:
/// `--before`/`--after`/`--on` also compare dates lexically via SQL string
/// `<`/`>` (`src/cache/query_documents.rs`), so `date_bounds` staying lexical
/// keeps `describe` consistent with the rest of norn rather than introducing
/// a second, chronological notion of date order. For uniform-offset or UTC
/// ISO-8601 values, lexical order equals chronological order; values with
/// mixed UTC offsets can sort out of true chronological order under lexical
/// comparison — this is a known, norn-wide limitation (not specific to
/// `describe`), tracked as NRN-110. Only values that validate as an ISO
/// `date` or `datetime` are included; malformed values are schema violations
/// (surfaced separately by `validate`) and are excluded rather than coerced
/// or compared lexically as raw strings — `describe` reports the vault, it
/// does not re-validate it. A field with no valid present values contributes
/// no bounds.
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
            // Include only well-formed ISO date/datetime values. Malformed
            // values are schema violations (surfaced by `validate`); excluding
            // them keeps bounds meaningful — comparison below is lexical, per
            // the function doc comment.
            let valid = crate::set::validate::coerce_value_for_type("date", raw, None).is_ok()
                || crate::set::validate::coerce_value_for_type("datetime", raw, None).is_ok();
            if !valid {
                continue;
            }
            if min.as_ref().is_none_or(|m| raw < m.as_str()) {
                min = Some(raw.to_string());
            }
            if max.as_ref().is_none_or(|m| raw > m.as_str()) {
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

/// Assemble the full contents-summary. Auto mode excludes date fields that
/// actually produced bounds from distributions (they get bounds instead; a
/// declared date field whose values are all non-ISO has no bounds and so
/// stays visible as a distribution rather than vanishing); `--by` mode
/// summarizes exactly the named fields, bypasses identity-skip, and — since
/// the user explicitly asked for these distributions — skips auto
/// date-bounds entirely so a named date field isn't double-rendered.
pub fn summarize(
    docs: &[DocumentSummary],
    config: &LoadedConfig,
    opts: &DataOptions,
) -> DataSummary {
    // F1: normalize `by` (trim + drop-empty) at this shared seam so the CLI
    // (`--by` via clap's `value_delimiter=','`, which does not trim) and the
    // MCP path (which already trims) cannot diverge — matches `count`'s own
    // `by` normalization in `src/count/mod.rs`.
    let by: Vec<String> = opts
        .by
        .iter()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .collect();

    let dfields = date_fields(config);
    // F4: only compute auto date-bounds in auto mode; `--by` mode is an
    // explicit request for field distributions, not bounds.
    let dates = if by.is_empty() {
        date_bounds(docs, &dfields)
    } else {
        Vec::new()
    };
    // F3: exclude from auto distributions only the date fields that actually
    // produced bounds (valid ISO values present), not every declared date
    // field — a mistyped date field (all non-ISO values) has no bounds and
    // must still surface as a distribution.
    let bounded: BTreeSet<String> = dates.iter().map(|d| d.field.clone()).collect();

    let (fields, skipped) = if by.is_empty() {
        let auto = auto_fields(docs, &bounded);
        field_distributions(docs, &auto, false, opts)
    } else {
        field_distributions(docs, &by, true, opts)
    };
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
    fn date_bounds_excludes_malformed_values() {
        // "not-a-date" validates as neither `date` nor `datetime` and must be
        // excluded rather than compared lexically as a raw string — sorting
        // it in would corrupt chronological ordering.
        let docs = vec![
            doc(serde_json::json!({"created": "2026-05-10"})),
            doc(serde_json::json!({"created": "not-a-date"})),
            doc(serde_json::json!({"created": "2026-07-03"})),
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
    fn summarize_normalizes_by_trimming_and_dropping_empties() {
        // F1: CLI's clap `value_delimiter=','` does not trim segments, so
        // `--by "type, status"` arrives as `["type", " status"]`. `summarize`
        // must normalize (trim + drop-empty) before selecting fields, so the
        // space-prefixed " status" is honored as `status`, not silently
        // dropped (which is what happened pre-fix: only `type` appeared).
        let docs = vec![
            doc(serde_json::json!({"type": "note", "status": "active"})),
            doc(serde_json::json!({"type": "task", "status": "backlog"})),
        ];
        let opts = DataOptions {
            by: vec!["type".to_string(), " status".to_string()],
            ..DataOptions::default()
        };
        let cfg = config_with_date_field("created", "datetime");
        let summary = summarize(&docs, &cfg, &opts);
        assert!(
            summary.fields.iter().any(|f| f.field == "type"),
            "expected `type` distribution, got: {:?}",
            summary.fields.iter().map(|f| &f.field).collect::<Vec<_>>()
        );
        assert!(
            summary.fields.iter().any(|f| f.field == "status"),
            "expected `status` distribution (trimmed from \" status\"), got: {:?}",
            summary.fields.iter().map(|f| &f.field).collect::<Vec<_>>()
        );
    }

    #[test]
    fn summarize_mistyped_date_field_stays_visible_as_distribution() {
        // F3: `due` is declared `date` but every value is free text ("someday",
        // "TBD") — none validate as ISO, so `date_bounds` produces no bounds
        // for it. Excluding ALL declared date fields from auto distributions
        // (the pre-fix behavior) then makes `due` vanish from both `dates`
        // and `fields`. The fix excludes only fields that actually produced
        // bounds, so a mistyped date field falls through to a normal
        // distribution instead of disappearing.
        let docs = vec![
            doc(serde_json::json!({"due": "someday"})),
            doc(serde_json::json!({"due": "someday"})),
            doc(serde_json::json!({"due": "TBD"})),
        ];
        let cfg = config_with_date_field("due", "date");
        let summary = summarize(&docs, &cfg, &DataOptions::default());
        assert!(
            summary.dates.is_empty(),
            "no valid ISO values ⇒ no bounds, got: {:?}",
            summary.dates
        );
        assert!(
            summary.fields.iter().any(|f| f.field == "due"),
            "mistyped date field must still appear as a distribution, got: {:?}",
            summary.fields.iter().map(|f| &f.field).collect::<Vec<_>>()
        );
    }

    #[test]
    fn summarize_by_mode_omits_auto_date_bounds() {
        // F4: in `--by` mode the user explicitly asked for a distribution of
        // the named field. Pre-fix, `date_bounds` ran unconditionally, so a
        // `--by created` on a `datetime` field rendered BOTH a distribution
        // (from the `--by` branch, which doesn't exclude date fields) AND a
        // `dates` entry — double-rendering. Fix: no auto date-bounds when
        // `by` is non-empty.
        let docs = vec![
            doc(serde_json::json!({"created": "2026-05-10"})),
            doc(serde_json::json!({"created": "2026-05-10"})),
            doc(serde_json::json!({"created": "2026-07-03"})),
        ];
        let cfg = config_with_date_field("created", "datetime");
        let opts = DataOptions {
            by: vec!["created".to_string()],
            ..DataOptions::default()
        };
        let summary = summarize(&docs, &cfg, &opts);
        assert!(
            summary.dates.is_empty(),
            "--by mode must not also emit auto date-bounds, got: {:?}",
            summary.dates
        );
        let matches: Vec<_> = summary
            .fields
            .iter()
            .filter(|f| f.field == "created")
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "`created` must appear exactly once (as a distribution), got: {:?}",
            summary.fields.iter().map(|f| &f.field).collect::<Vec<_>>()
        );
    }

    #[test]
    fn null_value_counts_as_missing_not_as_null_bucket() {
        // F5: explicit `null` must be treated as MISSING, consistent with
        // `has`/`missing` query filters (which treat null as absent) and with
        // the existing empty-array-as-missing handling. Two docs share
        // "rust" so the field survives identity-skip and we can inspect
        // buckets directly.
        let docs = vec![
            doc(serde_json::json!({"field": serde_json::Value::Null})),
            doc(serde_json::json!({"field": "rust"})),
            doc(serde_json::json!({"field": "rust"})),
        ];
        let opts = DataOptions::default();
        let (dists, skipped) = field_distributions(&docs, &["field".to_string()], false, &opts);
        assert!(skipped.is_empty());
        assert_eq!(dists.len(), 1);
        let vals: Vec<_> = dists[0]
            .values
            .iter()
            .map(|v| (v.value.as_str(), v.count))
            .collect();
        assert!(
            !vals.iter().any(|(v, _)| *v == "(null)"),
            "null must not appear as a \"(null)\" value bucket, got: {vals:?}"
        );
        assert!(
            vals.contains(&(MISSING, 1)),
            "null must be counted in the (missing) bucket, got: {vals:?}"
        );
        assert!(vals.contains(&("rust", 2)));
        assert_eq!(
            dists[0].values.len(),
            2,
            "only `rust` and `(missing)` buckets, no distinct null bucket, got: {vals:?}"
        );
    }

    #[test]
    fn summarize_excludes_date_fields_from_distributions() {
        // Both `type` and `created` need a ratio that survives identity-skip
        // (distinct/occurrences < 0.9) so this test genuinely depends on the
        // Task-3 date-exclusion wiring rather than passing by coincidence via
        // the Task-2 identity-skip heuristic:
        // - `type`: "note" repeats (2 of 3 docs) so its ratio (2/3 ≈ 0.67)
        //   stays below threshold and it lands in `fields` regardless of
        //   dates.
        // - `created`: "2026-05-10" repeats (2 of 3 docs) so its ratio
        //   (2/3 ≈ 0.67) also stays below threshold — specifically to defeat
        //   identity-skip — so if `summarize`'s date-field exclusion were
        //   deleted, `created` would survive identity-skip and appear in
        //   `summary.fields`, which is exactly what this test must catch.
        let docs = vec![
            doc(serde_json::json!({"type": "note", "created": "2026-05-10"})),
            doc(serde_json::json!({"type": "note", "created": "2026-05-10"})),
            doc(serde_json::json!({"type": "task", "created": "2026-07-03"})),
        ];
        let cfg = config_with_date_field("created", "datetime");
        let summary = summarize(&docs, &cfg, &DataOptions::default());
        assert_eq!(summary.total, 3);
        assert!(summary.dates.iter().any(|d| d.field == "created"));
        assert!(
            !summary.fields.iter().any(|f| f.field == "created"),
            "date field must not appear as a distribution"
        );
        assert!(summary.fields.iter().any(|f| f.field == "type"));
    }
}
