//! The `find` / `count` request (`Params`) and response (`Report`) vocabulary.
//!
//! Pure serde types — the typed `find`/`count` request/response
//! envelopes. The owner executes a request against its warm cache and answers
//! with the matching `Report`; the CLI renders that `Report` into the
//! user-facing formats. No logic, no IO, no dependency on any other norn crate.
//!
//! Two request shapes, two response shapes:
//!
//! - [`FindParams`] → [`FindReport`]: the filtered/sorted/paged document set,
//!   carried as flat [`FindDoc`] projections (path, stem, hash, frontmatter,
//!   body). Column selection and output formatting are the CLI's job — the
//!   report carries the whole matched row so every `--col`/`--format` choice is
//!   a pure presentation transform over it.
//! - [`CountParams`] → [`CountReport`]: a total, a single-field distribution, or
//!   a nested multi-field group tree. [`CountReport`] is `#[serde(untagged)]` so
//!   its serialization IS the `count --format json` contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::{FilterParams, Finding, MigrationPlan, Note, SortPaginateParams};

/// A `find` request: the shared filter + sort/paging vocabulary. The find-only
/// `--limit` default (10) and the `--all` help-gate are applied CLI-side and
/// verb-side respectively, never encoded here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FindParams {
    #[serde(skip_serializing_if = "is_default_filter")]
    pub filter: FilterParams,
    #[serde(skip_serializing_if = "is_default_paging")]
    pub paging: SortPaginateParams,
    /// Load each match's deep connection facets (headings + the three link
    /// sets) alongside the flat projection. Off by default — a plain `find`
    /// never pays the per-match connection load; the CLI turns this on only
    /// when a deep `--col` facet (`.headings` / `.outgoing_links` /
    /// `.unresolved_links` / `.incoming_links`) or `--all-cols` is requested,
    /// so the empty-vs-loaded distinction is the CLI's: it renders a deep facet
    /// only when it asked for one (and so it was loaded), never a misleading
    /// empty array for an unrequested facet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub with_connections: bool,
    /// The dynamically-desugared field keys the CLI expanded from forgiving
    /// `--field value` predicates (ADR 0010) — the input to the owner-side
    /// field-universe gate (NRN-367). Canonical `--eq`/`--in` keys are never
    /// listed here (they bypass the gate by design), so the owner rejects a
    /// genuinely-unknown dynamic field with a did-you-mean while leaving an
    /// explicit `--eq` predicate untouched. Empty (and absent on the wire) for
    /// the common no-dynamic-predicate request, so this stays backward
    /// compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_keys: Vec<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_default_filter(f: &FilterParams) -> bool {
    *f == FilterParams::default()
}

fn is_default_paging(p: &SortPaginateParams) -> bool {
    *p == SortPaginateParams::default()
}

/// A `find` response: the flat document projections plus the paging envelope
/// (`total` before limit/offset, `returned` after, `starts_at`, `truncated`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindReport {
    pub documents: Vec<FindDoc>,
    pub total: usize,
    pub returned: usize,
    pub starts_at: usize,
    pub truncated: bool,
    /// Whether the vault carries any error-severity diagnostic (e.g. an
    /// unreadable document) — scoped to the WHOLE vault, not this query's
    /// matches. An off-filesystem consumer (the MCP find tool) reads it to
    /// reproduce the direct path's diagnostic-error signal; the CLI's own exit
    /// derivation is a separate concern. Additive and omitted-when-false, so a
    /// clean vault's report is byte-unchanged from before the field existed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_diagnostic_errors: bool,
}

/// One matched document. Frontmatter and body are carried as-parsed so the CLI
/// can project any `--col` field or emit the whole block. The deep connection
/// facets (headings + the three link sets) are carried as pre-serialized JSON
/// values (`serde_json::to_value` of the cache's `Heading` / `Link` /
/// `IncomingLink`), so the CLI's `--format json` emission matches
/// the cache's own serialization exactly; they are populated only when the request set
/// [`FindParams::with_connections`], and are empty otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindDoc {
    pub path: String,
    pub stem: String,
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body_text: String,
    /// Serialized `Heading` values (`{ level, slug, text, source_span }`),
    /// document order. Empty unless connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<Value>,
    /// Serialized resolved `Link` values. Empty unless connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outgoing_links: Vec<Value>,
    /// Serialized unresolved `Link` values. Empty unless connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_links: Vec<Value>,
    /// Serialized `IncomingLink` values (`{ source_path, link }`). Empty unless
    /// connections were loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incoming_links: Vec<Value>,
}

/// A `get` request: the targets to resolve plus the sort/paging surface. `--col`
/// / `--all-cols` are NOT sent — the owner always returns each resolved
/// document's full facet set (frontmatter, headings, the three link sets, body,
/// hash, stem) and the CLI projects. `sections` is the `--section` list to
/// resolve owner-side; the CLI sends it empty for formats that ignore sections
/// (`paths` / `markdown`), so resolving one never pushes an exit-flipping error
/// note for a format that documents `--section` as ignored. `markdown` selects
/// the exact-source path (the owner reads the file — ADR 0014: markdown does not
/// participate in the relational snapshot).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetParams {
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "is_default_paging")]
    pub paging: SortPaginateParams,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<String>,
    /// Whether the whole-body field should be DISPLAYED (`--all-cols` or
    /// `--col .body`). The owner carries body on `GetRecord.body` only when set,
    /// so the CLI's records/json body gate is exactly "did the record carry a
    /// body". Body is still LOADED (but not returned) when `sections` is
    /// non-empty, so `--section` can resolve its spans without leaking the body.
    #[serde(skip_serializing_if = "is_false")]
    pub with_body: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub markdown: bool,
}

/// A `get` response: the resolved records plus non-fatal typed notes (ambiguity
/// warnings, missing-target errors). Each [`Note`] carries its own
/// [`Severity`](crate::Severity); an `error`-severity note drives the exit-1 /
/// `isError` signal, decided from the typed field, never from message text
/// (ADR 0022). The CLI renders each note as a POSIX-shaped stderr line and MCP
/// passes the typed notes through. For `--format markdown`, `records` carries the
/// resolved paths (for the CLI's count/warnings) and `markdown_content` carries
/// the single doc's exact source bytes when exactly one resolved.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GetReport {
    pub records: Vec<GetRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    /// The exact source bytes for a single-doc `--format markdown` request.
    /// `None` for structured formats, or when markdown resolved zero / more than
    /// one doc (the CLI derives the error from `records.len()`), or when the
    /// owner could not read the source file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown_content: Option<String>,
}

/// One resolved document with its full facet set. The deep facets (headings +
/// the three link sets) are pre-serialized JSON values matching the
/// cache's own serialization exactly. `frontmatter` keys the absent-vs-null distinction
/// on key presence: an absent key is a source with no frontmatter block, a
/// present `null` is an empty `---`/`---` block. `hash` / `stem` / `body` are
/// always carried; the CLI emits them only when the projection asks (opt-in
/// `--col .document_hash` / `.stem`, and body via `--all-cols` / `.body`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetRecord {
    pub path: String,
    pub stem: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontmatter: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headings: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outgoing_links: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_links: Vec<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incoming_links: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Resolved `--section` spans as ordered `(heading, content)` pairs, request
    /// order. `None` when `--section` was not requested (or the format ignores
    /// it); `Some(vec![])` when requested but zero headings resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<(String, String)>>,
}

/// A `describe` request: the structure view always, plus a contents-summary when
/// `data` is set (`--data`/`--stats`) OR `by` is non-empty (`--by` implies data,
/// on the *normalized* `by`). `limit` caps value-buckets per field (`None` →
/// default 20; `Some(0)` → uncapped). `--format` is a CLI-side presentation
/// choice over the report, never sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DescribeParams {
    #[serde(skip_serializing_if = "is_false")]
    pub data: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "is_default_filter")]
    pub filter: FilterParams,
    /// The dynamically-desugared field keys expanded from forgiving
    /// `--field value` predicates (ADR 0010), gated owner-side against the field
    /// universe (NRN-367/NRN-374) — mirrors [`FindParams::dynamic_keys`] /
    /// [`CountParams::dynamic_keys`]. The `--by` grouping keys are NOT listed
    /// here; only desugared filter predicates are gated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_keys: Vec<String>,
}

/// A `describe` response. Field order is part of the JSON contract, since
/// `--format json` serializes the struct directly.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DescribeReport {
    pub folders: Vec<String>,
    pub path_rules: Vec<PathRule>,
    pub creatable_rules: Vec<CreatableRule>,
    pub inbox: Option<String>,
    pub schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataSummary>,
}

/// A declared path rule: which glob gets which frontmatter defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathRule {
    pub glob: String,
    pub name: Option<String>,
    pub frontmatter_defaults: Value,
}

/// A rule usable with `new { rule: name }` — declares both a name and a target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatableRule {
    pub name: String,
    pub target: String,
    pub required_vars: Vec<String>,
    pub frontmatter_defaults: Value,
    pub body: Option<String>,
}

/// The contents-summary: totals, per-field value distributions, date bounds, and
/// the identity-skipped fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataSummary {
    pub total: usize,
    pub fields: Vec<FieldDistribution>,
    pub dates: Vec<DateBounds>,
    pub skipped: Vec<SkippedField>,
}

/// One field's value distribution: the shown buckets plus how many were capped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldDistribution {
    pub field: String,
    pub values: Vec<ValueCount>,
    pub more: usize,
}

/// One value bucket: the rendered value and its occurrence count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueCount {
    pub value: String,
    pub count: usize,
}

/// Lexical min/max bounds for a date-typed field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DateBounds {
    pub field: String,
    pub min: String,
    pub max: String,
}

/// A field dropped from the auto distributions for being (near-)identity: as many
/// distinct values as occurrences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedField {
    pub field: String,
    pub distinct: usize,
    pub total: usize,
}

/// A `count` request: the `--by` grouping fields plus the shared filter surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CountParams {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<String>,
    #[serde(skip_serializing_if = "is_default_filter")]
    pub filter: FilterParams,
    /// The dynamically-desugared field keys expanded from forgiving
    /// `--field value` predicates (ADR 0010), gated owner-side against the field
    /// universe (NRN-367) — mirrors [`FindParams::dynamic_keys`]. The `--by`
    /// grouping keys are NOT listed here; only desugared filter predicates are
    /// GATED (a hard rejection). An unknown `--by` key is a separate, softer
    /// surface (NRN-374): the CLI display layer warns (never rejects) when a
    /// `--by` field groups every matched document into `(missing)` — see
    /// `norn_cli::display::emit::warn_unknown_by_count`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dynamic_keys: Vec<String>,
}

/// A `count` response. `#[serde(untagged)]` so the serialized form is exactly
/// the `count --format json` output:
///
/// - no `--by` → `{"total":N}`
/// - one `--by` field → `{"by":"status","total":N,"groups":{…}}`
/// - many `--by` fields → `{"by":["type","status"],"total":N,"groups":{…nested…}}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CountReport {
    Grouped {
        by: String,
        total: usize,
        groups: BTreeMap<String, usize>,
    },
    GroupedMulti {
        by: Vec<String>,
        total: usize,
        groups: BTreeMap<String, GroupNode>,
    },
    Total {
        total: usize,
    },
}

/// One node in a multi-field count group tree: a terminal count, or a nested
/// map one grouping level deeper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupNode {
    Leaf(usize),
    Branch(BTreeMap<String, GroupNode>),
}

/// A `validate` request: the triage filters (each a repeatable, comma-splittable
/// `--code`/`--severity`/… list), `--verbose`, and `--summary`. Validate is
/// read-only; the finding set is computed against the warm graph and filtered
/// owner-side. `summary` is sent (not a pure CLI concern) so the grouped-count
/// JSON body — which needs the typed finding model that never crosses the wire —
/// is computed owner-side ONLY when requested. `--format` stays a pure CLI-side
/// presentation choice over the returned report, never sent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidateParams {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub codes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub severities: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// Keep graph-diagnostic `detail` in findings (default trims to the concise
    /// coded form — the `--verbose` gate).
    #[serde(skip_serializing_if = "is_false")]
    pub verbose: bool,
    /// Compute the grouped-count summary body (`--summary`). When false the owner
    /// skips `summarize()` entirely and leaves `ValidateReport::summary_json` `None`.
    #[serde(skip_serializing_if = "is_false")]
    pub summary: bool,
}

/// A `validate` response: the post-filter findings as the typed flat
/// [`Finding`] contract (ADR 0022 — no pre-serialized string tunnel), plus the
/// pre-rendered `--summary` JSON body and the run header counts. `has_errors` is
/// the single exit-code driver (a document carried an error-severity graph
/// diagnostic), computed over the whole index BEFORE triage filtering so a
/// `--code` narrow never changes the exit code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidateReport {
    /// The post-filter findings. Renderers serialize these at the edge:
    /// `--format jsonl` writes one finding per line, `--format json` wraps them
    /// in `{total, findings}`, and `records`/`paths` read the typed fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// The exact `serde_json::to_string_pretty` of the grouped summary over the
    /// filtered findings — what `--format json --summary` prints verbatim.
    /// `Some` only when the request set `--summary`; `None` otherwise (the owner
    /// skips the fold entirely).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_json: Option<String>,
    pub total_docs: usize,
    pub rules_count: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_errors: bool,
}

/// A `repair` request: run the standards engine over the warm graph, triage-filter
/// the findings, and turn them into a deterministic `MigrationPlan` — WITHOUT
/// applying it. Read-only, like `validate` (`repair` emits a plan; `apply`
/// executes it). The triage filters mirror [`ValidateParams`]; `confidence_high`
/// is the `--confidence high` band gate (drop Medium closest-match proposals) and
/// `skip_reasons` narrows the plan's skipped-findings list by reason-code glob.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairParams {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub severities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// `--skip-reason` reason-code globs — narrows the plan's skipped list only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_reasons: Vec<String>,
    /// `--confidence high`: keep only High-confidence closest-match proposals.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confidence_high: bool,
    /// Keep graph-diagnostic `detail` in findings (the `--verbose` gate);
    /// mirrors [`ValidateParams::verbose`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub verbose: bool,
}

/// A `repair` response: the deterministic [`MigrationPlan`](crate::MigrationPlan)
/// the findings produced, carried TYPED (it lives in this crate — NRN-405 part b).
/// The CLI serializes it with `serde_json::to_string_pretty` for the verbatim
/// `--format json` passthrough (identical to what `norn repair --plan
/// --format json` prints) and reads its fields directly for the `report` / `paths`
/// formats and the bare-summary counts. The bare-`norn repair` summary also needs
/// the by-code finding tally and the run header counts, which the plan does not
/// carry, so those ride alongside. `has_diagnostic_errors` is the single exit-code
/// driver (an error-severity graph diagnostic anywhere in the FULL index, computed
/// BEFORE triage filtering so a `--code` narrow never changes the exit code).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RepairReport {
    /// The deterministic [`MigrationPlan`](crate::MigrationPlan) the findings
    /// produced. The CLI `to_string_pretty`-serializes it for the verbatim
    /// `--format json` passthrough and reads its ops/skipped for the other
    /// projections.
    pub plan: MigrationPlan,
    /// The rich skip detail for the findings the planner chose not to act on —
    /// a sibling to the plan, mirroring `plan.skipped` one-for-one and in the same
    /// order, but carrying the candidate paths and next actions the LEAN plan
    /// `SkippedFinding` deliberately omits (ADR 0024: rich skip detail is a
    /// planner-REPORT output, not an apply output — the plan stays lean). Additive
    /// and omitted-when-empty, so a report with no skips is byte-unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_detail: Vec<RepairSkipDetail>,
    /// Sorted `(code, count)` over the triage-filtered findings — the bare
    /// summary's per-code tally (a `BTreeMap` collected in code order).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings_by_code: Vec<(String, usize)>,
    pub findings_total: usize,
    pub total_docs: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_diagnostic_errors: bool,
}

/// Rich detail for one skipped finding on the [`RepairReport`] — the candidate
/// paths and operator next-actions the planner computed but the LEAN plan
/// `SkippedFinding` does not carry (ADR 0024). `finding_code` + `path` +
/// `reason_code` correlate it back to the matching `plan.skipped` entry; the two
/// nested lists are omitted when empty so a detail with neither adds only the
/// three scalar keys.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairSkipDetail {
    /// The underlying validation finding code (e.g. `link-target-missing`).
    pub finding_code: String,
    /// The skipped document's vault-relative path.
    pub path: String,
    /// The kebab-case skip-reason code (e.g. `ambiguous-target`) — identical to
    /// the correlated `plan.skipped` entry's `reason`.
    pub reason_code: String,
    /// Candidate resolution paths (plain vault-path strings) the operator can pick
    /// from — populated e.g. for an ambiguous link target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    /// Suggested operator next actions for resolving the skipped finding by hand.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_actions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repair_report_omits_skip_detail_when_empty() {
        // Additive contract (ADR 0024): a report with no skips is byte-unchanged —
        // the `skipped_detail` key is absent, exactly as before the field existed.
        let report = RepairReport {
            findings_total: 3,
            total_docs: 10,
            ..Default::default()
        };
        let value = serde_json::to_value(&report).unwrap();
        assert!(
            value.get("skipped_detail").is_none(),
            "empty skip detail must be omitted, got {value}"
        );
    }

    #[test]
    fn repair_report_skip_detail_round_trips_and_omits_empty_lists() {
        let report = RepairReport {
            skipped_detail: vec![
                RepairSkipDetail {
                    finding_code: "link-ambiguous".into(),
                    path: "notes/a.md".into(),
                    reason_code: "ambiguous-target".into(),
                    candidates: vec!["x/Daily.md".into(), "y/Daily.md".into()],
                    next_actions: vec!["change the link to an explicit path".into()],
                },
                RepairSkipDetail {
                    finding_code: "value-not-allowed".into(),
                    path: "notes/b.md".into(),
                    reason_code: "no-rule-matched".into(),
                    candidates: vec![],
                    next_actions: vec![],
                },
            ],
            ..Default::default()
        };
        let value = serde_json::to_value(&report).unwrap();
        // The rich detail is present, candidates are plain path strings.
        assert_eq!(
            value["skipped_detail"][0]["reason_code"],
            "ambiguous-target"
        );
        assert_eq!(value["skipped_detail"][0]["candidates"][1], "y/Daily.md");
        // A detail carrying neither list omits both keys (only the three scalars).
        let second = &value["skipped_detail"][1];
        assert!(second.get("candidates").is_none());
        assert!(second.get("next_actions").is_none());
        assert_eq!(second.as_object().unwrap().len(), 3);
        // Round-trips.
        let back: RepairReport = serde_json::from_value(value).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn default_find_params_serialize_empty() {
        assert_eq!(
            serde_json::to_value(FindParams::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn dynamic_keys_are_absent_when_empty_and_carried_when_present() {
        // Backward compatible: the common no-dynamic-predicate request omits the
        // field entirely (NRN-367), so an older `{ … }`-only frame still parses.
        let mut params = FindParams::default();
        assert!(!serde_json::to_string(&params)
            .unwrap()
            .contains("dynamic_keys"));

        // Present → carried on the wire and round-trips for the owner-side gate.
        params.dynamic_keys = vec!["titel".to_string()];
        let line = serde_json::to_string(&params).unwrap();
        assert!(line.contains(r#""dynamic_keys":["titel"]"#), "{line}");
        assert_eq!(
            serde_json::from_str::<FindParams>(&line)
                .unwrap()
                .dynamic_keys,
            vec!["titel".to_string()]
        );
    }

    #[test]
    fn describe_dynamic_keys_are_absent_when_empty_and_carried_when_present() {
        // Same backward-compat shape as `FindParams`/`CountParams` (NRN-374): the
        // common no-dynamic-predicate request omits the field entirely.
        let mut params = DescribeParams::default();
        assert!(!serde_json::to_string(&params)
            .unwrap()
            .contains("dynamic_keys"));

        params.dynamic_keys = vec!["titel".to_string()];
        let line = serde_json::to_string(&params).unwrap();
        assert!(line.contains(r#""dynamic_keys":["titel"]"#), "{line}");
        assert_eq!(
            serde_json::from_str::<DescribeParams>(&line)
                .unwrap()
                .dynamic_keys,
            vec!["titel".to_string()]
        );
    }

    #[test]
    fn count_total_serializes_as_bare_total() {
        let r = CountReport::Total { total: 7 };
        assert_eq!(serde_json::to_value(&r).unwrap(), json!({ "total": 7 }));
    }

    #[test]
    fn count_grouped_serializes_with_by_and_groups() {
        let mut groups = BTreeMap::new();
        groups.insert("active".to_string(), 3usize);
        groups.insert("done".to_string(), 1usize);
        let r = CountReport::Grouped {
            by: "status".to_string(),
            total: 4,
            groups,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({ "by": "status", "total": 4, "groups": { "active": 3, "done": 1 } })
        );
    }

    #[test]
    fn count_grouped_multi_nests() {
        let mut inner = BTreeMap::new();
        inner.insert("active".to_string(), GroupNode::Leaf(2));
        let mut groups = BTreeMap::new();
        groups.insert("task".to_string(), GroupNode::Branch(inner));
        let r = CountReport::GroupedMulti {
            by: vec!["type".to_string(), "status".to_string()],
            total: 2,
            groups,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "by": ["type", "status"],
                "total": 2,
                "groups": { "task": { "active": 2 } }
            })
        );
    }

    #[test]
    fn count_report_round_trips_through_json() {
        for r in [
            CountReport::Total { total: 0 },
            CountReport::Grouped {
                by: "k".into(),
                total: 1,
                groups: BTreeMap::from([("v".to_string(), 1usize)]),
            },
        ] {
            let v = serde_json::to_value(&r).unwrap();
            let back: CountReport = serde_json::from_value(v).unwrap();
            assert_eq!(back, r);
        }
    }

    #[test]
    fn find_report_round_trips() {
        let r = FindReport {
            documents: vec![FindDoc {
                path: "a.md".into(),
                stem: "a".into(),
                hash: "h".into(),
                frontmatter: Some(json!({"type": "note"})),
                body_text: "body".into(),
                headings: vec![],
                outgoing_links: vec![],
                unresolved_links: vec![],
                incoming_links: vec![],
            }],
            total: 1,
            returned: 1,
            starts_at: 1,
            truncated: false,
            has_diagnostic_errors: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        let back: FindReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn find_report_omits_diagnostic_errors_when_clean_carries_when_set() {
        // Additive contract: a clean vault's report omits the key entirely (a
        // pre-field report JSON still deserializes), while a vault with an
        // error-severity diagnostic carries `has_diagnostic_errors: true`.
        let clean = FindReport {
            documents: vec![],
            total: 0,
            returned: 0,
            starts_at: 1,
            truncated: false,
            has_diagnostic_errors: false,
        };
        let v = serde_json::to_value(&clean).unwrap();
        assert!(
            v.get("has_diagnostic_errors").is_none(),
            "clean report must omit the key, got {v}"
        );
        let back: FindReport = serde_json::from_value(v).unwrap();
        assert_eq!(back, clean);

        let dirty = FindReport {
            has_diagnostic_errors: true,
            ..clean
        };
        let v = serde_json::to_value(&dirty).unwrap();
        assert_eq!(v["has_diagnostic_errors"], serde_json::json!(true));
    }
}
