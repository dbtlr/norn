pub mod closest_match;
pub mod destination;
pub mod link_risk;
pub mod warnings;

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::Severity;
use camino::Utf8PathBuf;
use norn_wire::{MigrationOp, MigrationPlan, MIGRATION_PLAN_SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::standards::config::{RepairAction, RepairConfig, RepairRule, RepairRuleMatch};
use crate::standards::findings::Finding;
use crate::standards::op::ApplyOp;

pub const REPAIR_PLAN_SCHEMA_VERSION: u32 = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceFilter {
    High,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepairPlanFilters {
    pub code: Vec<String>,
    pub severity: Vec<String>,
    pub field: Vec<String>,
    pub rule: Vec<String>,
    pub path: Vec<String>,
    pub target: Vec<String>,
    pub reason: Vec<String>,
    pub skip_reason: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub confidence: Option<ConfidenceFilter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
// NRN-190: the `skip_reason` enum VALUE is canonically kebab, identical to
// `SkipReason::code()` — no snake/kebab split for the same concept.
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// Frontmatter field is missing and the configured repair rule has no deterministic default.
    MissingDefault,
    /// Broken link has no deterministic path/link rewrite; operator must decide.
    LinkDecisionNeeded,
    /// Finding has no matching repair rule in the configured rule set.
    NoRuleMatched,
    /// Graph-derived diagnostic (e.g. dangling reference detected at graph build) without a repair path.
    GraphDiagnostic,
    /// Link-ambiguous: multiple resolution candidates, manual decision required.
    AmbiguousTarget,
    /// Index has no current hash for the finding's path (file removed between
    /// indexing and planning, or path didn't normalize the same way).
    MissingHash,
    /// Rule matched but a precondition blocked producing a change. Emitted when
    /// `move_document` placeholder substitution fails (missing frontmatter field,
    /// non-scalar value, unknown placeholder).
    PreconditionFailed,
}

impl SkipReason {
    pub fn code(self) -> &'static str {
        match self {
            SkipReason::MissingDefault => "missing-default",
            SkipReason::LinkDecisionNeeded => "link-decision-needed",
            SkipReason::NoRuleMatched => "no-rule-matched",
            SkipReason::GraphDiagnostic => "graph-diagnostic",
            SkipReason::AmbiguousTarget => "ambiguous-target",
            SkipReason::MissingHash => "missing-hash",
            SkipReason::PreconditionFailed => "precondition-failed",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct SkippedFinding {
    pub path: Utf8PathBuf,
    pub code: String,
    pub severity: Severity,
    pub message: String,
    /// Stable kebab-case skip-reason identifier. Serializes identically to
    /// `SkipReason::code()`; the redundant precomputed `reason_code` string was
    /// collapsed into this single canonical field (NRN-190).
    pub skip_reason: SkipReason,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub candidates: Vec<Utf8PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub next_actions: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SkippedSummary {
    /// Map from reason code (kebab-case) to count. By convention zero-count entries
    /// are not inserted; `SkippedSummary::from_skipped` guarantees this.
    pub by_reason: BTreeMap<String, usize>,
    pub total: usize,
}

impl SkippedSummary {
    pub(crate) fn from_skipped(findings: &[SkippedFinding]) -> Self {
        let mut by_reason: BTreeMap<String, usize> = BTreeMap::new();
        for f in findings {
            *by_reason
                .entry(f.skip_reason.code().to_string())
                .or_insert(0) += 1;
        }
        SkippedSummary {
            by_reason,
            total: findings.len(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepairPlanSummary {
    pub findings: usize,
    pub planned_changes: usize,
    pub skipped: SkippedSummary,
}

/// What the repair planner emits: a `MigrationPlan` of typed ops built natively
/// (ADR 0024 — a repair plan IS a migration plan), plus the two carriage roles the
/// former `RepairPlan` served that the wire plan does not — the rich `skipped`
/// findings (candidate paths, next actions, skip-reason code) the planner's REPORT
/// surfaces, and the run `summary` tallies. Repair no longer produces
/// `ApplyOp`/`RepairPlan` as its output contract; the ops carry finding
/// linkage (`finding_code`/`repair_rule`) and per-op footnotes directly.
///
/// `plan.generator`/`plan.generated_at` are left unset here (the planner is a pure
/// function of its inputs, so its unit tests stay wall-clock-free); the findings
/// entry point [`plan_from_findings`](crate::planner::findings::plan_from_findings)
/// stamps that provenance.
pub struct RepairPlanResult {
    pub plan: MigrationPlan,
    /// The rich skip detail (a superset of the wire plan's lean `skipped`) for the
    /// planner report — mirrors `plan.skipped` one-for-one, in the same order.
    pub(crate) skipped: Vec<SkippedFinding>,
    pub summary: RepairPlanSummary,
}

/// Build a wire op's `fields` object from an interior `ApplyOp`, natively,
/// replacing a former `serde_json::to_value(&change)` round
/// trip (which serialized the whole struct, dropped `operation`, and remapped
/// `path`/`destination`→`src`/`dst` for moves). `serde_json`'s `Map` is a
/// `BTreeMap`, so the emitted key ORDER is the sorted set regardless of insertion
/// order; only the key set and values are load-bearing for plan bytes. `operation`
/// is intentionally omitted — it becomes the [`MigrationOp::kind`].
fn op_fields_from_change(change: &ApplyOp) -> Value {
    let is_move = change.operation == "move_document";
    let mut fields = serde_json::Map::new();
    fields.insert("change_id".into(), Value::String(change.change_id.clone()));
    // move_document speaks the unified `src`/`dst` vocabulary; every other op
    // keeps `path`.
    let path_key = if is_move { "src" } else { "path" };
    fields.insert(path_key.into(), Value::String(change.path.to_string()));
    fields.insert(
        "document_hash".into(),
        Value::String(change.document_hash.clone()),
    );
    // Linkage is `Option<String>` on the interior op; the repair planner always
    // populates `finding_code` / `repair_rule` (a finding's code and its rule
    // name), so the conditional insert fires for every repair-emitted op — the
    // emitted key set matches the former unconditional insert.
    if let Some(finding_code) = &change.finding_code {
        fields.insert("finding_code".into(), Value::String(finding_code.clone()));
    }
    if let Some(finding_rule) = &change.finding_rule {
        fields.insert("finding_rule".into(), Value::String(finding_rule.clone()));
    }
    if let Some(repair_rule) = &change.repair_rule {
        fields.insert("repair_rule".into(), Value::String(repair_rule.clone()));
    }
    if let Some(field) = &change.field {
        fields.insert("field".into(), Value::String(field.clone()));
    }
    if let Some(expected) = &change.expected_old_value {
        fields.insert("expected_old_value".into(), expected.clone());
    }
    if let Some(new_value) = &change.new_value {
        fields.insert("new_value".into(), new_value.clone());
    }
    if let Some(destination) = &change.destination {
        // The `dst` remap belongs to move ops alone; a non-move op carrying a
        // destination (none exists today) must keep the former serde path's
        // literal `destination` key rather than silently borrowing the move
        // vocabulary.
        let key = if is_move { "dst" } else { "destination" };
        fields.insert(key.into(), Value::String(destination.to_string()));
    }
    if let Some(link_risk) = &change.link_risk {
        fields.insert(
            "link_risk".into(),
            serde_json::to_value(link_risk).expect("LinkRisk must serialize"),
        );
    }
    if !change.warnings.is_empty() {
        fields.insert(
            "warnings".into(),
            serde_json::to_value(&change.warnings).expect("PlanWarnings must serialize"),
        );
    }
    if change.force {
        fields.insert("force".into(), Value::Bool(true));
    }
    if change.parents {
        fields.insert("parents".into(), Value::Bool(true));
    }
    Value::Object(fields)
}

/// The per-op footnote STRING for a built-in link rewrite. The former
/// `plan_from_findings` built this off the `RepairPlan.footnotes` list keyed by
/// `change_id`; it now attaches directly to the op at construction. Its bytes
/// ride the plan (`repair --plan --format report` renders it).
fn render_plan_footnote(footnote: &PlanFootnote) -> String {
    match &footnote.details {
        FootnoteDetails::ClosestMatch(d) => {
            let confidence_label = match footnote.confidence {
                Confidence::High => "high",
                Confidence::Medium => "medium",
            };
            format!(
                "closest-match suggestion (confidence: {}): \"{}\" → \"{}\" (edit distance: {})",
                confidence_label, d.original_target, d.candidate_stem, d.normalized_distance,
            )
        }
        FootnoteDetails::Alias(d) => format!(
            "alias rewrite (deterministic): \"{}\" → \"{}\" (unique alias of {})",
            d.original_target, d.candidate_stem, d.alias_doc,
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FootnoteKind {
    ClosestMatchSuggestion,
    /// Deterministic rewrite of a dangling wikilink that uniquely matches one
    /// document's `aliases` entry, to that document's canonical stem (NRN-455).
    AliasSuggestion,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FootnoteDetails {
    ClosestMatch(ClosestMatchDetails),
    Alias(AliasDetails),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClosestMatchDetails {
    pub original_target: String,
    pub normalized_target: String,
    pub candidate_stem: String,
    pub normalized_distance: usize,
    pub slug_normalized_identity: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AliasDetails {
    /// The dangling wikilink target as authored (the alias).
    pub original_target: String,
    /// The canonical stem the link is rewritten to.
    pub candidate_stem: String,
    /// The document that uniquely declares the alias.
    pub alias_doc: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlanFootnote {
    pub change_id: String,
    pub kind: FootnoteKind,
    pub confidence: Confidence,
    pub details: FootnoteDetails,
}

fn derive_change_id(
    path: &Utf8PathBuf,
    finding_code: &str,
    expected_old_value: Option<&Value>,
    occurrence_index: u32,
) -> String {
    // BLAKE3 derives the id (`sha2` isn't a norn-core dependency; `blake3`
    // is), taking the first 16 hex chars of its hex digest (a
    // 64-bit-equivalent width). change_ids are internal identifiers, not a
    // wire contract, so only the determinism and width need to be stable, not
    // the algorithm.
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(finding_code.as_bytes());
    hasher.update(b"\0");
    if let Some(v) = expected_old_value {
        hasher.update(v.to_string().as_bytes());
    }
    hasher.update(b"\0");
    hasher.update(&occurrence_index.to_le_bytes());
    let digest = hasher.finalize();
    digest.to_hex()[..16].to_string()
}

const DEFAULT_MEDIUM_THRESHOLD: f64 = 0.7;

enum ClosestMatchOutcome {
    Change {
        change: Box<ApplyOp>,
        footnote: Box<PlanFootnote>,
    },
    TiedSkip {
        skipped: Box<SkippedFinding>,
    },
    NoMatch,
}

/// The deterministic alias-hint (NRN-455): if the dangling target uniquely
/// matches exactly ONE document's `aliases` entry (case-folded, mirroring the
/// resolver's case-insensitive stem posture), AND that document's stem is
/// itself unique across the vault, propose a `rewrite_link` to that document's
/// canonical stem. Non-unique alias matches (two docs claim the alias) yield
/// `None`, as does a unique alias match whose candidate stem is shared by
/// other documents — rewriting to a non-unique stem would trade a dangling
/// link for an ambiguous one, which is not an improvement. Checked BEFORE
/// fuzzy closest-match: an exact alias match is a certain rewrite target, so
/// it wins over edit-distance guessing.
fn handle_alias_match(
    finding: &Finding,
    by_alias: &BTreeMap<String, Vec<(Utf8PathBuf, String)>>,
    stem_counts: &BTreeMap<String, u32>,
    document_hashes: &BTreeMap<Utf8PathBuf, String>,
    occurrence_counts: &mut BTreeMap<(Utf8PathBuf, String, String), u32>,
) -> Option<(ApplyOp, PlanFootnote)> {
    let broken_target = finding.target.as_deref()?;
    let key = broken_target.to_lowercase();
    let matches = by_alias.get(&key)?;
    // Uniqueness gate: exactly one distinct doc claims the alias.
    let [(alias_doc, candidate_stem)] = matches.as_slice() else {
        return None;
    };
    // Stem-uniqueness gate: the candidate stem must itself resolve to exactly
    // one document, or the rewrite trades a dangling link for an ambiguous one.
    if stem_counts.get(candidate_stem).copied().unwrap_or(0) != 1 {
        return None;
    }
    let document_hash = document_hashes.get(&finding.path).cloned()?;

    let expected_old_value = Some(Value::String(broken_target.to_string()));
    let occ_key = (
        finding.path.clone(),
        finding.code.clone(),
        expected_old_value
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default(),
    );
    let occurrence_index = *occurrence_counts
        .entry(occ_key)
        .and_modify(|n| *n += 1)
        .or_insert(0);
    let change_id = derive_change_id(
        &finding.path,
        &finding.code,
        expected_old_value.as_ref(),
        occurrence_index,
    );

    let change = ApplyOp {
        change_id: change_id.clone(),
        path: finding.path.clone(),
        document_hash,
        finding_code: Some(finding.code.clone()),
        finding_rule: None,
        repair_rule: Some("built-in:alias-stem".to_string()),
        operation: "rewrite_link".to_string(),
        field: None,
        expected_old_value,
        new_value: Some(Value::String(candidate_stem.clone())),
        destination: None,
        link_risk: None,
        warnings: vec![],
        force: false,
        parents: false,
    };
    let footnote = PlanFootnote {
        change_id,
        kind: FootnoteKind::AliasSuggestion,
        confidence: Confidence::High,
        details: FootnoteDetails::Alias(AliasDetails {
            original_target: broken_target.to_string(),
            candidate_stem: candidate_stem.clone(),
            alias_doc: alias_doc.to_string(),
        }),
    };
    Some((change, footnote))
}

fn handle_closest_match(
    finding: &Finding,
    stem_corpus: &[&str],
    documents: &[crate::domain::Document],
    document_hashes: &BTreeMap<Utf8PathBuf, String>,
    occurrence_counts: &mut BTreeMap<(Utf8PathBuf, String, String), u32>,
    medium_threshold: f64,
) -> ClosestMatchOutcome {
    // Only reached for `link-target-missing` findings, which always carry a
    // target; a finding lacking one has no broken link to closest-match.
    let Some(broken_target) = finding.target.as_deref() else {
        return ClosestMatchOutcome::NoMatch;
    };

    let outcome = closest_match::closest_match(broken_target, stem_corpus, medium_threshold);

    match outcome {
        closest_match::MatchOutcome::High { ref candidate_stem }
        | closest_match::MatchOutcome::Medium {
            ref candidate_stem, ..
        } => {
            let candidate_stem = candidate_stem.clone();
            let Some(document_hash) = document_hashes.get(&finding.path).cloned() else {
                return ClosestMatchOutcome::NoMatch;
            };
            let normalized_target = closest_match::normalize_for_match(broken_target);
            let (confidence, normalized_distance, slug_normalized_identity) = match &outcome {
                closest_match::MatchOutcome::High { .. } => (Confidence::High, 0, true),
                closest_match::MatchOutcome::Medium {
                    normalized_distance,
                    ..
                } => (Confidence::Medium, *normalized_distance, false),
                _ => unreachable!(),
            };

            let expected_old_value = Some(Value::String(broken_target.to_string()));
            let occ_key = (
                finding.path.clone(),
                finding.code.clone(),
                expected_old_value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            );
            let occurrence_index = *occurrence_counts
                .entry(occ_key)
                .and_modify(|n| *n += 1)
                .or_insert(0);

            let change_id = derive_change_id(
                &finding.path,
                &finding.code,
                expected_old_value.as_ref(),
                occurrence_index,
            );

            let change = ApplyOp {
                change_id: change_id.clone(),
                path: finding.path.clone(),
                document_hash,
                finding_code: Some(finding.code.clone()),
                finding_rule: None,
                repair_rule: Some("built-in:closest-match-stem".to_string()),
                operation: "rewrite_link".to_string(),
                field: None,
                expected_old_value,
                new_value: Some(Value::String(candidate_stem.clone())),
                destination: None,
                link_risk: None,
                warnings: vec![],
                force: false,
                parents: false,
            };

            let footnote = PlanFootnote {
                change_id,
                kind: FootnoteKind::ClosestMatchSuggestion,
                confidence,
                details: FootnoteDetails::ClosestMatch(ClosestMatchDetails {
                    original_target: broken_target.to_string(),
                    normalized_target,
                    candidate_stem,
                    normalized_distance,
                    slug_normalized_identity,
                }),
            };

            ClosestMatchOutcome::Change {
                change: Box::new(change),
                footnote: Box::new(footnote),
            }
        }
        closest_match::MatchOutcome::Tied { candidate_stems } => {
            // Resolve tied stems back to doc paths via the documents slice.
            // Multiple docs can share a stem (different directories) — include all unique.
            // Use BTreeSet to dedupe by path: the algorithm can return duplicate stems
            // (one entry per scored doc), and the flat_map would otherwise produce
            // duplicate paths for each stem repetition.
            let candidates: Vec<Utf8PathBuf> = candidate_stems
                .iter()
                .flat_map(|stem| {
                    documents
                        .iter()
                        .filter(move |d| &d.stem == stem)
                        .map(|d| d.path.clone())
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let mut skipped = skipped_finding(finding, SkipReason::AmbiguousTarget, None);
            skipped.candidates = candidates;
            ClosestMatchOutcome::TiedSkip {
                skipped: Box::new(skipped),
            }
        }
        closest_match::MatchOutcome::NoMatch => ClosestMatchOutcome::NoMatch,
    }
}

pub fn plan_repairs(
    vault_root: Utf8PathBuf,
    filters: RepairPlanFilters,
    findings: Vec<Finding>,
    config: &RepairConfig,
    index: &crate::domain::GraphIndex,
) -> RepairPlanResult {
    let document_hashes: BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();
    let stem_corpus: Vec<&str> = index.documents.iter().map(|d| d.stem.as_str()).collect();

    // Alias index for the deterministic alias-hint (NRN-455): a lowercased alias
    // maps to the (path, stem) of each document that declares it, deduplicated per
    // document so a doc listing an alias twice counts once. A dangling wikilink
    // whose target uniquely matches ONE entry here is rewritten to that doc's
    // canonical stem; a non-unique match yields no hint.
    let mut by_alias: BTreeMap<String, Vec<(Utf8PathBuf, String)>> = BTreeMap::new();
    for doc in &index.documents {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for alias in &doc.aliases {
            if seen.insert(alias.as_str()) {
                by_alias
                    .entry(alias.clone())
                    .or_default()
                    .push((doc.path.clone(), doc.stem.clone()));
            }
        }
    }
    // How many documents share each stem, so the alias-hint can require its
    // candidate stem to resolve to exactly one document (see handle_alias_match).
    let mut stem_counts: BTreeMap<String, u32> = BTreeMap::new();
    for doc in &index.documents {
        *stem_counts.entry(doc.stem.clone()).or_insert(0) += 1;
    }

    let mut changes = Vec::new();
    let mut skipped: Vec<SkippedFinding> = Vec::new();
    let mut footnotes: Vec<PlanFootnote> = Vec::new();
    let mut occurrence_counts: BTreeMap<(Utf8PathBuf, String, String), u32> = BTreeMap::new();

    for finding in &findings {
        // Typed-reference violations are never auto-repairable: the right fix
        // is a judgment call (repoint the reference vs retype the target), so
        // they must not be matchable by field-/rule-scoped repair rules —
        // a code-less `match: { field: parent }` rule would otherwise
        // overwrite the reference with a fixed value.
        if finding.code == "frontmatter-reference-type" {
            skipped.push(skipped_finding(finding, SkipReason::NoRuleMatched, None));
            continue;
        }
        match matching_repair_rule(finding, &config.rules) {
            Some((rule, action)) => {
                let occ_key = (
                    finding.path.clone(),
                    finding.code.clone(),
                    finding_actual_value(finding)
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
                let occurrence_index = *occurrence_counts
                    .entry(occ_key)
                    .and_modify(|n| *n += 1)
                    .or_insert(0);
                match planned_change(
                    finding,
                    rule,
                    &action,
                    &document_hashes,
                    &index.documents,
                    occurrence_index,
                ) {
                    Ok(change) => changes.push(change),
                    Err((skip, reason)) => skipped.push(skipped_finding(finding, skip, reason)),
                }
            }
            None => {
                if finding.code == "link-target-missing" {
                    // Deterministic alias-hint first (NRN-455): a unique alias
                    // match is a certain rewrite target, so it takes priority over
                    // fuzzy closest-match. Falls through when there is no unique
                    // alias match.
                    if let Some((change, footnote)) = handle_alias_match(
                        finding,
                        &by_alias,
                        &stem_counts,
                        &document_hashes,
                        &mut occurrence_counts,
                    ) {
                        changes.push(change);
                        footnotes.push(footnote);
                        continue;
                    }
                    match handle_closest_match(
                        finding,
                        &stem_corpus,
                        &index.documents,
                        &document_hashes,
                        &mut occurrence_counts,
                        DEFAULT_MEDIUM_THRESHOLD,
                    ) {
                        ClosestMatchOutcome::Change { change, footnote } => {
                            changes.push(*change);
                            footnotes.push(*footnote);
                        }
                        ClosestMatchOutcome::TiedSkip { skipped: tied_skip } => {
                            skipped.push(*tied_skip);
                        }
                        ClosestMatchOutcome::NoMatch => {
                            skipped.push(skipped_finding(
                                finding,
                                SkipReason::LinkDecisionNeeded,
                                None,
                            ));
                        }
                    }
                } else if finding.code == "bom-marker" {
                    // (NRN-385) Built-in, no configured rule needed — mirrors the
                    // closest-match-stem precedent above: a user rule matching
                    // `code: bom-marker` still wins (checked first, above), this is
                    // only the fallback when none does.
                    match document_hashes.get(&finding.path) {
                        Some(document_hash) => {
                            let occ_key =
                                (finding.path.clone(), finding.code.clone(), String::new());
                            let occurrence_index = *occurrence_counts
                                .entry(occ_key)
                                .and_modify(|n| *n += 1)
                                .or_insert(0);
                            let change_id = derive_change_id(
                                &finding.path,
                                &finding.code,
                                None,
                                occurrence_index,
                            );
                            changes.push(ApplyOp {
                                change_id,
                                path: finding.path.clone(),
                                document_hash: document_hash.clone(),
                                finding_code: Some(finding.code.clone()),
                                finding_rule: None,
                                repair_rule: Some("built-in:strip-bom".to_string()),
                                operation: "strip_bom".to_string(),
                                field: None,
                                expected_old_value: None,
                                new_value: None,
                                destination: None,
                                link_risk: None,
                                warnings: vec![],
                                force: false,
                                parents: false,
                            });
                        }
                        None => {
                            skipped.push(skipped_finding(finding, SkipReason::MissingHash, None));
                        }
                    }
                } else {
                    let skip = skip_reason_for_finding(finding);
                    skipped.push(skipped_finding(finding, skip, None));
                }
            }
        }
    }

    // Apply --confidence filter to closest-match proposals.
    if let Some(ConfidenceFilter::High) = filters.confidence {
        let medium_ids: BTreeSet<String> = footnotes
            .iter()
            .filter(|f| matches!(f.confidence, Confidence::Medium))
            .map(|f| f.change_id.clone())
            .collect();
        changes.retain(|c| !medium_ids.contains(&c.change_id));
        footnotes.retain(|f| !matches!(f.confidence, Confidence::Medium));
    }

    let skipped_summary = SkippedSummary::from_skipped(&skipped);
    let summary = RepairPlanSummary {
        findings: findings.len(),
        planned_changes: changes.len(),
        skipped: skipped_summary,
    };

    // Emit wire ops natively (ADR 0024). Each interior `ApplyOp` becomes a
    // `MigrationOp`: `operation` → `kind`, the remaining fields → the `fields`
    // object (matching the former serde round trip), and the 1:1 built-in link
    // footnote (closest-match or the deterministic alias-hint) — keyed by
    // `change_id` — is resolved to its string and attached to the op.
    let footnote_by_change_id: std::collections::HashMap<&str, &PlanFootnote> = footnotes
        .iter()
        .map(|f| (f.change_id.as_str(), f))
        .collect();
    let operations: Vec<MigrationOp> = changes
        .iter()
        .map(|change| MigrationOp {
            kind: change.operation.clone(),
            id: None,
            requires: Vec::new(),
            fields: op_fields_from_change(change),
            footnote: footnote_by_change_id
                .get(change.change_id.as_str())
                .map(|f| render_plan_footnote(f)),
        })
        .collect();

    // The wire plan's lean `skipped` (finding_code / path / reason-code / footnote)
    // — the rich detail (candidates, next actions) rides `RepairPlanResult.skipped`
    // for the planner report, one-for-one and in the same order.
    let wire_skipped: Vec<norn_wire::SkippedFinding> = skipped
        .iter()
        .map(|sf| norn_wire::SkippedFinding {
            finding_code: sf.code.clone(),
            path: sf.path.to_string(),
            reason: sf.skip_reason.code().to_string(),
            footnote: None,
        })
        .collect();

    let plan = MigrationPlan {
        schema_version: MIGRATION_PLAN_SCHEMA_VERSION,
        vault_root: vault_root.to_string(),
        generator: None,
        generated_at: None,
        preconditions: Vec::new(),
        operations,
        skipped: wire_skipped,
        plan_footnote: None,
    };

    RepairPlanResult {
        plan,
        skipped,
        summary,
    }
}

fn matching_repair_rule<'a>(
    finding: &Finding,
    rules: &'a [RepairRule],
) -> Option<(&'a RepairRule, RepairAction)> {
    rules
        .iter()
        .find(|rule| repair_match_applies(finding, &rule.r#match))
        .map(|rule| {
            let action = rule.action();
            (rule, action)
        })
}

fn repair_match_applies(finding: &Finding, rule_match: &RepairRuleMatch) -> bool {
    rule_match
        .code
        .as_ref()
        .is_none_or(|code| code == &finding.code)
        && rule_match
            .rule
            .as_ref()
            .is_none_or(|rule| finding_rule(finding).as_ref() == Some(rule))
        && rule_match
            .field
            .as_ref()
            .is_none_or(|field| finding_field(finding).as_ref() == Some(field))
        && rule_match
            .actual_value
            .as_ref()
            .is_none_or(|actual_value| finding_actual_value(finding) == Some(actual_value))
}

fn planned_change(
    finding: &Finding,
    rule: &RepairRule,
    action: &RepairAction,
    document_hashes: &BTreeMap<Utf8PathBuf, String>,
    documents: &[crate::domain::Document],
    occurrence_index: u32,
) -> Result<ApplyOp, (SkipReason, Option<String>)> {
    let repair_rule = rule
        .name
        .clone()
        .unwrap_or_else(|| "unnamed-repair-rule".to_string());
    let document_hash = document_hashes
        .get(&finding.path)
        .ok_or((SkipReason::MissingHash, None))?
        .clone();
    let change_id = derive_change_id(
        &finding.path,
        &finding.code,
        finding_actual_value(finding),
        occurrence_index,
    );
    Ok(match action {
        RepairAction::SetFrontmatter { field, value } => ApplyOp {
            change_id: change_id.clone(),
            path: finding.path.clone(),
            document_hash,
            finding_code: Some(finding.code.clone()),
            finding_rule: finding_rule(finding),
            repair_rule: Some(repair_rule),
            operation: "set_frontmatter".to_string(),
            field: Some(field.clone()),
            expected_old_value: finding_actual_value(finding).cloned(),
            new_value: Some(value.clone()),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        },
        RepairAction::RemoveFrontmatter { field } => ApplyOp {
            change_id: change_id.clone(),
            path: finding.path.clone(),
            document_hash,
            finding_code: Some(finding.code.clone()),
            finding_rule: finding_rule(finding),
            repair_rule: Some(repair_rule),
            operation: "remove_frontmatter".to_string(),
            field: Some(field.clone()),
            expected_old_value: finding_actual_value(finding).cloned(),
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        },
        RepairAction::AddFrontmatter { field, value } => ApplyOp {
            change_id: change_id.clone(),
            path: finding.path.clone(),
            document_hash,
            finding_code: Some(finding.code.clone()),
            finding_rule: finding_rule(finding),
            repair_rule: Some(repair_rule),
            operation: "add_frontmatter".to_string(),
            field: Some(field.clone()),
            expected_old_value: None,
            new_value: Some(value.clone()),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        },
        RepairAction::MoveDocument { destination } => {
            let source_doc = documents.iter().find(|d| d.path == finding.path);
            let frontmatter = source_doc.and_then(|d| d.frontmatter.as_ref());

            let new_path = match crate::standards::repair::destination::resolve_destination(
                destination,
                &finding.path,
                frontmatter,
            ) {
                Ok(p) => p,
                Err(e) => {
                    return Err((
                        SkipReason::PreconditionFailed,
                        Some(format!("placeholder substitution failed: {e}")),
                    ));
                }
            };

            let link_risk = crate::standards::repair::link_risk::classify(
                &finding.path,
                &new_path,
                documents,
                &[],
            );

            let mut warnings = Vec::new();
            if let Some(w) = crate::standards::repair::warnings::detect_stem_collision(
                &finding.path,
                &new_path,
                documents,
            ) {
                warnings.push(w);
            }

            ApplyOp {
                change_id,
                path: finding.path.clone(),
                document_hash,
                finding_code: Some(finding.code.clone()),
                finding_rule: finding_rule(finding),
                repair_rule: Some(repair_rule),
                operation: "move_document".to_string(),
                field: None,
                expected_old_value: None,
                new_value: None,
                destination: Some(new_path),
                link_risk: Some(link_risk),
                warnings,
                force: false,
                parents: false,
            }
        }
    })
}

/// Derive the fine-grained `SkipReason` from a finding's code (ADR 0022: repair
/// keys on codes, not on a variant body). Link findings split on the ambiguous
/// code; every code that is neither a known frontmatter/link/alias/nonportable
/// finding is a graph diagnostic (the open-set fallback).
fn skip_reason_for_finding(finding: &Finding) -> SkipReason {
    match finding.code.as_str() {
        "link-ambiguous" => SkipReason::AmbiguousTarget,
        "link-target-missing" | "link-anchor-missing" | "link-block-missing" => {
            SkipReason::LinkDecisionNeeded
        }
        "frontmatter-required-field-missing" => SkipReason::MissingDefault,
        "value-not-allowed"
        | "field-type-invalid"
        | "frontmatter-exceeds-max-length"
        | "frontmatter-forbidden-field"
        | "document-misrouted"
        | "frontmatter-reference-type"
        | "nonportable-filename" => SkipReason::NoRuleMatched,
        _ => SkipReason::GraphDiagnostic,
    }
}

fn skipped_finding(
    finding: &Finding,
    skip_reason: SkipReason,
    reason_override: Option<String>,
) -> SkippedFinding {
    let field = finding.field.as_deref().unwrap_or("");
    let (reason, next_actions) = match finding.code.as_str() {
        "link-ambiguous" => (
            "ambiguous link target".to_string(),
            vec![
                "change the link to an explicit path".to_string(),
                "rename one duplicate candidate".to_string(),
                "rerun repair plan after disambiguation".to_string(),
            ],
        ),
        "link-target-missing" | "link-anchor-missing" | "link-block-missing" => (
            "link repair requires an explicit path/link decision".to_string(),
            vec![
                "create the missing target or target anchor".to_string(),
                "rewrite the link manually".to_string(),
                "rerun validate after resolving the link".to_string(),
            ],
        ),
        "frontmatter-required-field-missing" => (
            "missing field has no configured deterministic default".to_string(),
            vec![
                format!("add a repair rule that sets {field} when safe"),
                "fill the field manually and rerun validate".to_string(),
            ],
        ),
        "value-not-allowed"
        | "field-type-invalid"
        | "frontmatter-exceeds-max-length"
        | "frontmatter-forbidden-field" => (
            "no configured deterministic repair rule matched".to_string(),
            vec![
                format!("add a repair rule for field {field}"),
                "rerun repair plan after updating config".to_string(),
            ],
        ),
        "frontmatter-reference-type" => (
            "typed-reference violation cannot be repaired deterministically".to_string(),
            vec![
                format!(
                    "repoint '{field}' at a document whose type is one of: {}",
                    finding.allowed_types.join(", ")
                ),
                "or change the target document's type if the reference is right".to_string(),
                "rerun validate after fixing the reference".to_string(),
            ],
        ),
        "document-misrouted" => (
            "no configured move_document repair rule matched this misrouted document".to_string(),
            vec![
                "review allowed_paths and current document location".to_string(),
                "add a move_document repair rule matching this finding's code".to_string(),
            ],
        ),
        "nonportable-filename" => (
            "filename portability is diagnosed, not auto-repaired (a rename cascades every backlink; that is a move, out of repair's scope)".to_string(),
            vec![
                format!("resolve manually: {}", finding.issues.join("; ")),
                "rename the file, then rerun validate".to_string(),
            ],
        ),
        // Graph diagnostics (open code set) and any unrecognized code.
        _ => (
            "graph diagnostic cannot be repaired deterministically".to_string(),
            vec![
                "inspect the diagnostic detail".to_string(),
                "fix the document manually and rerun validate".to_string(),
            ],
        ),
    };

    // MissingHash overrides the default reason since the cause is upstream of the rule.
    let (reason, next_actions) = if matches!(skip_reason, SkipReason::MissingHash) {
        (
            "document hash not present in index — file may have been removed or renamed"
                .to_string(),
            vec!["rebuild the index and rerun repair plan".to_string()],
        )
    } else {
        (reason, next_actions)
    };

    // Explicit override takes precedence (e.g., MoveDocument substitution failure).
    let reason = reason_override.unwrap_or(reason);

    SkippedFinding {
        path: finding.path.clone(),
        code: finding.code.clone(),
        severity: finding.severity,
        message: finding.message.clone(),
        skip_reason,
        reason,
        rule: finding_rule(finding),
        field: finding_field(finding),
        target: finding_target(finding),
        candidates: finding_candidates(finding),
        next_actions,
    }
}

fn finding_rule(finding: &Finding) -> Option<String> {
    finding.rule.clone()
}

fn finding_field(finding: &Finding) -> Option<String> {
    finding.field.clone()
}

fn finding_actual_value(finding: &Finding) -> Option<&Value> {
    finding.actual_value.as_ref()
}

fn finding_target(finding: &Finding) -> Option<String> {
    finding.target.clone()
}

fn finding_candidates(finding: &Finding) -> Vec<Utf8PathBuf> {
    finding.candidates.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Link, LinkKind, LinkStatus, Severity, UnresolvedReason};
    use crate::standards::config::{RepairAction, RepairRule, RepairRuleMatch};
    use crate::standards::findings::Finding;
    use serde_json::json;

    fn vault_root() -> Utf8PathBuf {
        "/vault".into()
    }

    /// A native op's `fields` value by key — the emission path now pins op fields
    /// where the tests once read `ApplyOp` members.
    fn op_field<'a>(result: &'a RepairPlanResult, i: usize, key: &str) -> Option<&'a Value> {
        result.plan.operations[i].fields.get(key)
    }

    /// A native op's `fields` string value by key.
    fn op_str<'a>(result: &'a RepairPlanResult, i: usize, key: &str) -> Option<&'a str> {
        op_field(result, i, key).and_then(Value::as_str)
    }

    /// How many emitted ops carry a footnote — the closest-match footnote count the
    /// tests once read off `RepairPlan.footnotes`.
    fn footnote_count(result: &RepairPlanResult) -> usize {
        result
            .plan
            .operations
            .iter()
            .filter(|op| op.footnote.is_some())
            .count()
    }

    fn finding_disallowed_value(path: &str, field: &str, value: serde_json::Value) -> Finding {
        Finding::frontmatter_disallowed_value(
            path.into(),
            Some("task-status".into()),
            field.into(),
            value,
            vec![json!("backlog"), json!("completed")],
        )
    }

    fn finding_link_ambiguous(path: &str, target: &str, candidates: Vec<&str>) -> Finding {
        let link = Link {
            source_path: path.into(),
            raw: format!("[[{target}]]"),
            kind: LinkKind::Wikilink,
            target: target.into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: None,
            unresolved_reason: Some(UnresolvedReason::Ambiguous),
            candidates: candidates.into_iter().map(Into::into).collect(),
            status: LinkStatus::Ambiguous,
        };
        Finding::from_link(path.into(), link)
    }

    fn finding_link_unresolved(path: &str, target: &str) -> Finding {
        // Emits link-target-missing (post-split). Helper name kept for diff simplicity.
        let link = Link {
            source_path: path.into(),
            raw: format!("[[{target}]]"),
            kind: LinkKind::Wikilink,
            target: target.into(),
            label: None,
            anchor: None,
            block_ref: None,
            source_span: None,
            source_context: None,
            resolved_path: None,
            unresolved_reason: Some(UnresolvedReason::TargetMissing),
            candidates: vec![],
            status: LinkStatus::Unresolved,
        };
        Finding::from_link(path.into(), link)
    }

    fn make_rule(
        name: &str,
        match_code: &str,
        match_field: Option<&str>,
        match_actual: Option<serde_json::Value>,
        action: RepairAction,
    ) -> RepairRule {
        let (set_frontmatter, remove_frontmatter, add_frontmatter, move_document) = match action {
            RepairAction::SetFrontmatter { field, value } => (
                Some(crate::standards::config::SetFrontmatterAction { field, value }),
                None,
                None,
                None,
            ),
            RepairAction::RemoveFrontmatter { field } => (
                None,
                Some(crate::standards::config::RemoveFrontmatterAction { field }),
                None,
                None,
            ),
            RepairAction::AddFrontmatter { field, value } => (
                None,
                None,
                Some(crate::standards::config::AddFrontmatterAction { field, value }),
                None,
            ),
            RepairAction::MoveDocument { destination } => {
                let (to_directory, to_path) = match destination {
                    crate::standards::config::DestinationSpec::Directory { to_directory } => {
                        (Some(to_directory), None)
                    }
                    crate::standards::config::DestinationSpec::Path { to_path } => {
                        (None, Some(to_path))
                    }
                };
                (
                    None,
                    None,
                    None,
                    Some(crate::standards::config::MoveDocumentAction {
                        to_directory,
                        to_path,
                    }),
                )
            }
        };
        RepairRule {
            name: Some(name.into()),
            r#match: RepairRuleMatch {
                code: Some(match_code.into()),
                rule: None,
                field: match_field.map(Into::into),
                actual_value: match_actual,
            },
            set_frontmatter,
            remove_frontmatter,
            add_frontmatter,
            move_document,
        }
    }

    fn doc(path: &str, hash: &str) -> crate::domain::Document {
        crate::domain::Document {
            path: path.into(),
            stem: camino::Utf8Path::new(path).file_stem().unwrap().to_string(),
            hash: hash.to_string(),
            frontmatter: None,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    fn index_for(paths: &[&str]) -> crate::domain::GraphIndex {
        let documents = paths.iter().map(|p| doc(p, &format!("hash-{p}"))).collect();
        crate::domain::GraphIndex {
            root: vault_root(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    /// Build an index from (path, stem) pairs, using the path as the hash key.
    /// Unlike `index_for`, the stem is specified explicitly rather than derived
    /// from the filename — needed when we want docs in subdirectories where the
    /// file stem differs from the vault-level stem we're testing against.
    fn test_index_with_stems(pairs: &[(&str, &str)]) -> crate::domain::GraphIndex {
        let documents = pairs
            .iter()
            .map(|(path, stem)| {
                let mut d = doc(path, &format!("hash-{path}"));
                d.stem = stem.to_string();
                d
            })
            .collect();
        crate::domain::GraphIndex {
            root: vault_root(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    #[test]
    fn matching_rule_produces_planned_change() {
        let finding = finding_disallowed_value("task.md", "status", json!("someday"));
        let config = RepairConfig {
            rules: vec![make_rule(
                "fix-someday",
                "value-not-allowed",
                Some("status"),
                Some(json!("someday")),
                RepairAction::SetFrontmatter {
                    field: "status".into(),
                    value: json!("backlog"),
                },
            )],
        };
        let index = index_for(&["task.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 1);
        assert_eq!(result.skipped.len(), 0);
        assert_eq!(result.plan.operations[0].kind, "set_frontmatter");
        assert_eq!(op_str(&result, 0, "field"), Some("status"));
        assert_eq!(op_field(&result, 0, "new_value"), Some(&json!("backlog")));
        assert_eq!(
            op_field(&result, 0, "expected_old_value"),
            Some(&json!("someday"))
        );
        assert_eq!(op_str(&result, 0, "document_hash"), Some("hash-task.md"));
    }

    #[test]
    fn reference_type_finding_never_matches_a_repair_rule() {
        // A field-scoped repair rule (no code selector) must NOT capture the
        // typed-reference finding — the documented contract is "not
        // auto-repairable": auto-overwriting the reference with a fixed value
        // would silently destroy a link that merely pointed at the wrong
        // document type.
        let finding = Finding::frontmatter_reference_type(
            "task.md".into(),
            Some("task-refs".into()),
            "parent".into(),
            "[[note-1]]".into(),
            "note-1.md".into(),
            "note".into(),
            vec!["phase".into()],
        );
        let config = RepairConfig {
            rules: vec![make_rule(
                "reset-parent",
                "frontmatter-reference-type",
                Some("parent"),
                None,
                RepairAction::SetFrontmatter {
                    field: "parent".into(),
                    value: json!("[[inbox]]"),
                },
            )],
        };
        let index = index_for(&["task.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(
            result.plan.operations.len(),
            0,
            "reference-type findings must never plan changes: {:?}",
            result.plan.operations
        );
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::NoRuleMatched);
        assert_eq!(result.skipped[0].field.as_deref(), Some("parent"));
    }

    #[test]
    fn unmatched_finding_routes_to_skipped_with_no_rule_matched_reason() {
        let finding = finding_disallowed_value("task.md", "status", json!("someday"));
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["task.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::NoRuleMatched);
        assert_eq!(
            result.summary.skipped.by_reason.get("no-rule-matched"),
            Some(&1)
        );
        assert_eq!(
            result.summary.skipped.by_reason.get("ambiguous-target"),
            None
        );
    }

    #[test]
    fn ambiguous_link_finding_routes_to_skipped_with_ambiguous_target_reason() {
        let finding = finding_link_ambiguous(
            "note.md",
            "Daily",
            vec!["Calendar/Daily.md", "Templates/Daily.md"],
        );
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["note.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::AmbiguousTarget);
        assert_eq!(result.skipped[0].candidates.len(), 2);
        assert_eq!(
            result.summary.skipped.by_reason.get("ambiguous-target"),
            Some(&1)
        );
        assert_eq!(
            result.summary.skipped.by_reason.get("no-rule-matched"),
            None
        );
    }

    #[test]
    fn unresolved_link_finding_routes_to_skipped_with_link_decision_needed_reason() {
        let finding = finding_link_unresolved("note.md", "missing");
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["note.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(
            result.skipped[0].skip_reason,
            SkipReason::LinkDecisionNeeded
        );
        assert_eq!(
            result.summary.skipped.by_reason.get("link-decision-needed"),
            Some(&1)
        );
    }

    /// Build an index whose docs carry aliases (path == stem via `doc()`), for the
    /// alias-hint tests.
    fn index_with_aliases(specs: &[(&str, &[&str])]) -> crate::domain::GraphIndex {
        let documents = specs
            .iter()
            .map(|(path, aliases)| {
                let mut d = doc(path, &format!("hash-{path}"));
                d.aliases = aliases.iter().map(|a| a.to_string()).collect();
                d
            })
            .collect();
        crate::domain::GraphIndex {
            root: vault_root(),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    #[test]
    fn alias_hint_rewrites_dangling_link_to_unique_alias_stem() {
        // NRN-455: `[[Bee]]` is dangling (no path/stem `bee`), but `beta.md`
        // uniquely declares alias `bee` — repair proposes the deterministic
        // rewrite to the canonical stem `beta`. Case-folded match (authored `Bee`).
        let finding = finding_link_unresolved("src.md", "Bee");
        let config = RepairConfig { rules: vec![] };
        let index = index_with_aliases(&[("src.md", &[]), ("beta.md", &["bee"])]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 1, "one rewrite op expected");
        assert!(
            result.skipped.is_empty(),
            "the dangling link is repaired, not skipped"
        );
        assert_eq!(result.plan.operations[0].kind, "rewrite_link");
        assert_eq!(op_str(&result, 0, "new_value"), Some("beta"));
        assert_eq!(op_str(&result, 0, "expected_old_value"), Some("Bee"));
        assert_eq!(
            op_str(&result, 0, "repair_rule"),
            Some("built-in:alias-stem")
        );
        assert_eq!(footnote_count(&result), 1);
        let footnote = result.plan.operations[0].footnote.as_deref().unwrap();
        assert!(
            footnote.contains("alias rewrite") && footnote.contains("beta"),
            "footnote should describe the deterministic alias rewrite; got: {footnote}"
        );
    }

    #[test]
    fn alias_hint_skips_when_two_docs_claim_the_alias() {
        // Non-unique alias match (both docs claim `bee`) yields NO hint — the link
        // stays plain-dangling (no unique stem, no fuzzy match here → skipped).
        let finding = finding_link_unresolved("src.md", "bee");
        let config = RepairConfig { rules: vec![] };
        let index = index_with_aliases(&[
            ("src.md", &[]),
            ("beta.md", &["bee"]),
            ("gamma.md", &["bee"]),
        ]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert!(
            result.plan.operations.iter().all(|op| op
                .fields
                .get("repair_rule")
                .and_then(Value::as_str)
                != Some("built-in:alias-stem")),
            "no alias-stem op when the alias is non-unique"
        );
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0].skip_reason,
            SkipReason::LinkDecisionNeeded
        );
    }

    #[test]
    fn alias_hint_skips_when_candidate_stem_is_shared_by_another_document() {
        // NRN-455 fix: `bee` uniquely resolves to `beta.md` via alias, but the
        // stem `beta` is ALSO carried by `archive/beta.md` — rewriting to `beta`
        // would trade a dangling link for an ambiguous one (link-target-ambiguous),
        // which is not an improvement. No hint is proposed; the link stays
        // dangling (skipped, no fuzzy match here either).
        let finding = finding_link_unresolved("src.md", "bee");
        let config = RepairConfig { rules: vec![] };
        let index = index_with_aliases(&[
            ("src.md", &[]),
            ("beta.md", &["bee"]),
            ("archive/beta.md", &[]),
        ]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert!(
            result.plan.operations.iter().all(|op| op
                .fields
                .get("repair_rule")
                .and_then(Value::as_str)
                != Some("built-in:alias-stem")),
            "no alias-stem op when the candidate stem is shared by another document"
        );
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0].skip_reason,
            SkipReason::LinkDecisionNeeded
        );
    }

    #[test]
    fn missing_document_hash_routes_to_skipped_with_missing_hash_reason() {
        let finding = finding_disallowed_value("task.md", "status", json!("someday"));
        let config = RepairConfig {
            rules: vec![make_rule(
                "fix-someday",
                "value-not-allowed",
                Some("status"),
                Some(json!("someday")),
                RepairAction::SetFrontmatter {
                    field: "status".into(),
                    value: json!("backlog"),
                },
            )],
        };
        // Empty index (no documents) → triggers MissingHash for the finding.
        let index = index_for(&[]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::MissingHash);
        // The reason text reflects the new clearer message.
        assert!(result.skipped[0].reason.contains("hash not present"));
        assert_eq!(
            result.summary.skipped.by_reason.get("missing-hash"),
            Some(&1)
        );
    }

    fn finding_required_missing(path: &str, field: &str, rule: Option<&str>) -> Finding {
        Finding::frontmatter_required_missing(path.into(), rule.map(Into::into), field.into())
    }

    #[test]
    fn add_frontmatter_rule_produces_planned_change_for_missing_field() {
        let finding = finding_required_missing("task.md", "kind", Some("typed-note"));
        let config = RepairConfig {
            rules: vec![make_rule(
                "ensure-kind",
                "frontmatter-required-field-missing",
                Some("kind"),
                None,
                RepairAction::AddFrontmatter {
                    field: "kind".into(),
                    value: json!("research"),
                },
            )],
        };
        let index = index_for(&["task.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 1);
        assert_eq!(result.skipped.len(), 0);
        assert_eq!(result.plan.operations[0].kind, "add_frontmatter");
        assert_eq!(op_str(&result, 0, "field"), Some("kind"));
        assert_eq!(op_field(&result, 0, "new_value"), Some(&json!("research")));
        assert_eq!(op_field(&result, 0, "expected_old_value"), None);
        assert_eq!(op_str(&result, 0, "document_hash"), Some("hash-task.md"));
    }

    #[test]
    fn required_missing_no_rule_routes_to_missing_default_skip() {
        let finding = finding_required_missing("task.md", "kind", Some("typed-note"));
        // No rules → the planner cannot find a deterministic default for this field.
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["task.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::MissingDefault);
        assert!(
            result.skipped[0]
                .reason
                .contains("missing field has no configured deterministic default"),
            "unexpected reason text: {}",
            result.skipped[0].reason
        );
        assert_eq!(
            result.summary.skipped.by_reason.get("missing-default"),
            Some(&1)
        );
    }

    #[test]
    fn summary_counts_match_skip_reason_partition() {
        let findings = vec![
            finding_disallowed_value("task1.md", "status", json!("someday")),
            finding_link_ambiguous("note.md", "Daily", vec!["a.md", "b.md"]),
            finding_link_unresolved("note.md", "missing"),
        ];
        let config = RepairConfig {
            rules: vec![make_rule(
                "fix-someday",
                "value-not-allowed",
                Some("status"),
                Some(json!("someday")),
                RepairAction::SetFrontmatter {
                    field: "status".into(),
                    value: json!("backlog"),
                },
            )],
        };
        let index = index_for(&["task1.md", "note.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            findings,
            &config,
            &index,
        );
        assert_eq!(result.summary.findings, 3);
        assert_eq!(result.summary.planned_changes, 1);
        assert_eq!(result.summary.skipped.total, 2);
        assert_eq!(
            result.summary.skipped.by_reason.get("link-decision-needed"),
            Some(&1)
        );
        assert_eq!(
            result.summary.skipped.by_reason.get("ambiguous-target"),
            Some(&1)
        );
        assert_eq!(result.summary.skipped.by_reason.get("missing-hash"), None);
    }

    #[test]
    fn closest_match_proposes_high_confidence_rewrite_on_target_missing() {
        // A doc links to [[Norn Brand]], but the resolution is target-missing.
        // The vault has norn-brand.md — slug-normalize identity → High.
        // source.md must also appear in the index so its document hash is found.
        let finding = finding_link_unresolved("source.md", "Norn Brand");
        let index =
            test_index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);

        let result = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(result.plan.operations.len(), 1, "expected exactly one op");
        assert_eq!(result.plan.operations[0].kind, "rewrite_link");
        assert_eq!(
            op_str(&result, 0, "finding_code"),
            Some("link-target-missing")
        );
        assert_eq!(
            op_field(&result, 0, "expected_old_value"),
            Some(&json!("Norn Brand"))
        );
        assert_eq!(
            op_field(&result, 0, "new_value"),
            Some(&json!("norn-brand"))
        );

        // The closest-match footnote now rides the op as its rendered string.
        assert_eq!(footnote_count(&result), 1, "expected exactly one footnote");
        assert_eq!(
            result.plan.operations[0].footnote.as_deref(),
            Some(
                "closest-match suggestion (confidence: high): \"Norn Brand\" → \"norn-brand\" (edit distance: 0)"
            )
        );
    }

    #[test]
    fn closest_match_proposes_medium_confidence_rewrite_on_target_missing() {
        // Broken target "norn-brnd" vs stem "norn-brand": 1-char edit on a
        // 10-char string → ratio 0.9 → Medium (above 0.7 threshold, below
        // post-normalize identity).
        let finding = finding_link_unresolved("source.md", "norn-brnd");
        let index =
            test_index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);

        let result = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(result.plan.operations.len(), 1, "expected exactly one op");
        assert_eq!(result.plan.operations[0].kind, "rewrite_link");
        assert_eq!(
            op_field(&result, 0, "expected_old_value"),
            Some(&json!("norn-brnd"))
        );
        assert_eq!(
            op_field(&result, 0, "new_value"),
            Some(&json!("norn-brand"))
        );

        // Medium-band footnote string (edit distance 1, not a slug-normalize identity).
        assert_eq!(footnote_count(&result), 1, "expected exactly one footnote");
        assert_eq!(
            result.plan.operations[0].footnote.as_deref(),
            Some(
                "closest-match suggestion (confidence: medium): \"norn-brnd\" → \"norn-brand\" (edit distance: 1)"
            )
        );
    }

    #[test]
    fn closest_match_skips_with_ambiguous_when_candidates_tied() {
        // Two stems normalize-identical to "norn-brand" → Tied → skipped.
        // source.md also needs a hash entry (even though it won't be used for
        // tied outcomes — the tied branch doesn't reach the hash lookup).
        let finding = finding_link_unresolved("source.md", "Norn Brand");
        let index = test_index_with_stems(&[
            ("source.md", "source"),
            ("notes/norn-brand.md", "norn-brand"),
            ("archive/Norn-Brand.md", "Norn-Brand"),
        ]);

        let result = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(footnote_count(&result), 0);
        assert_eq!(result.skipped.len(), 1);
        let skipped = &result.skipped[0];
        assert_eq!(skipped.skip_reason, SkipReason::AmbiguousTarget);
        assert_eq!(skipped.candidates.len(), 2);
        // Candidates should be the actual doc paths (subdirs preserved), not synthesized.
        assert!(skipped
            .candidates
            .iter()
            .any(|p| p.as_str() == "notes/norn-brand.md"));
        assert!(skipped
            .candidates
            .iter()
            .any(|p| p.as_str() == "archive/Norn-Brand.md"));
    }

    #[test]
    fn closest_match_unsupported_when_no_candidate_above_threshold() {
        let finding = finding_link_unresolved("source.md", "xyzzy-zzz-far");
        let index =
            test_index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);

        let result = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(footnote_count(&result), 0);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(
            result.skipped[0].skip_reason,
            SkipReason::LinkDecisionNeeded
        );
    }

    #[test]
    fn confidence_high_filter_drops_medium_proposals() {
        // Two findings: one obviously typo'd (medium-band), one normalize-identity (high).
        let high_finding = finding_link_unresolved("a.md", "Norn Brand");
        let medium_finding = finding_link_unresolved("b.md", "norn-brnd"); // 1-char edit
        let index = test_index_with_stems(&[
            ("a.md", "a"),
            ("b.md", "b"),
            ("norn-brand.md", "norn-brand"),
        ]);

        let filters = RepairPlanFilters {
            confidence: Some(ConfidenceFilter::High),
            ..Default::default()
        };
        let result = plan_repairs(
            "/tmp/v".into(),
            filters,
            vec![high_finding, medium_finding],
            &RepairConfig::default(),
            &index,
        );

        // Only the high-confidence proposal survives.
        assert_eq!(
            result.plan.operations.len(),
            1,
            "expected only high-confidence change"
        );
        assert_eq!(
            footnote_count(&result),
            1,
            "expected only high-confidence footnote"
        );
        // The surviving op's footnote is the high-confidence band.
        assert!(result.plan.operations[0]
            .footnote
            .as_deref()
            .unwrap()
            .contains("confidence: high"));
    }

    #[test]
    fn confidence_filter_default_keeps_both_bands() {
        let high_finding = finding_link_unresolved("a.md", "Norn Brand");
        let medium_finding = finding_link_unresolved("b.md", "norn-brnd");
        let index = test_index_with_stems(&[
            ("a.md", "a"),
            ("b.md", "b"),
            ("norn-brand.md", "norn-brand"),
        ]);

        // Default filters: no confidence filter set.
        let result = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![high_finding, medium_finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(result.plan.operations.len(), 2);
        assert_eq!(footnote_count(&result), 2);
    }

    #[test]
    fn closest_match_tied_candidates_deduped_by_path() {
        // Two docs share stem "context" → tie when target is "concept" (1 edit).
        // Algorithm returns candidate_stems = ["context", "context"] (one per
        // scored doc); the resolver must dedupe by path so the SkippedFinding
        // doesn't emit duplicate paths to the operator.
        let finding = finding_link_unresolved("source.md", "concept");
        let index = test_index_with_stems(&[
            ("a/context.md", "context"),
            ("b/context.md", "context"),
            ("source.md", "source"),
        ]);

        let result = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(result.skipped.len(), 1);
        let skipped = &result.skipped[0];
        assert_eq!(skipped.skip_reason, SkipReason::AmbiguousTarget);
        // Two distinct doc paths, not four.
        assert_eq!(
            skipped.candidates.len(),
            2,
            "candidates should be deduped by path; got {:?}",
            skipped.candidates
        );
        // Both unique paths present.
        assert!(skipped
            .candidates
            .iter()
            .any(|p| p.as_str() == "a/context.md"));
        assert!(skipped
            .candidates
            .iter()
            .any(|p| p.as_str() == "b/context.md"));
    }

    #[test]
    fn skip_reason_has_seven_variants_with_stable_codes() {
        use SkipReason::*;
        let all = [
            MissingDefault,
            LinkDecisionNeeded,
            NoRuleMatched,
            GraphDiagnostic,
            AmbiguousTarget,
            MissingHash,
            PreconditionFailed,
        ];
        assert_eq!(all.len(), 7);

        assert_eq!(MissingDefault.code(), "missing-default");
        assert_eq!(LinkDecisionNeeded.code(), "link-decision-needed");
        assert_eq!(NoRuleMatched.code(), "no-rule-matched");
        assert_eq!(GraphDiagnostic.code(), "graph-diagnostic");
        assert_eq!(AmbiguousTarget.code(), "ambiguous-target");
        assert_eq!(MissingHash.code(), "missing-hash");
        assert_eq!(PreconditionFailed.code(), "precondition-failed");
    }

    #[test]
    fn skip_reason_round_trips_through_serde_with_kebab_case_variants() {
        // NRN-190: the enum VALUE is kebab, identical to `SkipReason::code()`.
        let json = serde_json::to_string(&SkipReason::MissingDefault).unwrap();
        assert_eq!(json, r#""missing-default""#);
        let back: SkipReason = serde_json::from_str(r#""link-decision-needed""#).unwrap();
        assert!(matches!(back, SkipReason::LinkDecisionNeeded));
    }

    #[test]
    fn repair_plan_schema_version_is_nine() {
        assert_eq!(REPAIR_PLAN_SCHEMA_VERSION, 9);
    }

    #[test]
    fn skipped_summary_uses_code_keyed_map() {
        let mut by_reason = BTreeMap::new();
        by_reason.insert("missing-default".to_string(), 520);
        by_reason.insert("link-decision-needed".to_string(), 449);
        by_reason.insert("ambiguous-target".to_string(), 32);
        let summary = SkippedSummary {
            by_reason,
            total: 1001,
        };

        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["total"], 1001);
        assert_eq!(json["by_reason"]["missing-default"], 520);
        assert_eq!(json["by_reason"]["link-decision-needed"], 449);
        assert_eq!(json["by_reason"]["ambiguous-target"], 32);
        assert!(json["by_reason"].get("missing-hash").is_none()); // zero-count buckets omitted
    }

    #[test]
    fn from_skipped_aggregates_codes_and_omits_zero_buckets() {
        use camino::Utf8PathBuf;
        let findings = vec![
            SkippedFinding {
                path: Utf8PathBuf::from("notes/a.md"),
                code: "missing-default".to_string(),
                severity: Severity::Warning,
                message: "no default value".to_string(),
                skip_reason: SkipReason::MissingDefault,
                reason: "rule has no default".to_string(),
                rule: None,
                field: None,
                target: None,
                candidates: vec![],
                next_actions: vec![],
            },
            SkippedFinding {
                path: Utf8PathBuf::from("notes/b.md"),
                code: "missing-default".to_string(),
                severity: Severity::Warning,
                message: "no default value".to_string(),
                skip_reason: SkipReason::MissingDefault,
                reason: "rule has no default".to_string(),
                rule: None,
                field: None,
                target: None,
                candidates: vec![],
                next_actions: vec![],
            },
            SkippedFinding {
                path: Utf8PathBuf::from("notes/c.md"),
                code: "ambiguous-target".to_string(),
                severity: Severity::Warning,
                message: "multiple candidates".to_string(),
                skip_reason: SkipReason::AmbiguousTarget,
                reason: "ambiguous link target".to_string(),
                rule: None,
                field: None,
                target: None,
                candidates: vec![],
                next_actions: vec![],
            },
        ];

        let summary = SkippedSummary::from_skipped(&findings);

        assert_eq!(summary.total, findings.len());
        assert_eq!(summary.by_reason.get("missing-default"), Some(&2));
        assert_eq!(summary.by_reason.get("ambiguous-target"), Some(&1));
        assert!(!summary.by_reason.contains_key("missing-hash"));
        assert_eq!(summary.by_reason.len(), 2);

        // JSON serialization also has no zero-count keys
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(
            json["by_reason"].as_object().unwrap().len(),
            2,
            "zero-count buckets must not appear in serialized JSON"
        );
    }

    #[test]
    fn skipped_finding_skip_reason_serializes_kebab_matching_code() {
        // NRN-190: the redundant `reason_code` field was collapsed away; the
        // typed `skip_reason` field now serializes as the canonical kebab code
        // (identical to `SkipReason::code()`), and the human `reason` prose stays.
        let f = SkippedFinding {
            path: "foo.md".into(),
            code: "frontmatter-required-field-missing".into(),
            severity: crate::domain::Severity::Warning,
            message: "missing field".into(),
            skip_reason: SkipReason::MissingDefault,
            reason: "missing field has no configured deterministic default".into(),
            rule: None,
            field: None,
            target: None,
            candidates: vec![],
            next_actions: vec![],
        };
        let json = serde_json::to_value(&f).unwrap();
        assert_eq!(json["skip_reason"], "missing-default");
        assert_eq!(json["skip_reason"], SkipReason::MissingDefault.code());
        assert!(
            json.get("reason_code").is_none(),
            "redundant reason_code field must be gone"
        );
        assert_eq!(
            json["reason"],
            "missing field has no configured deterministic default"
        );
    }

    #[test]
    fn repair_plan_filters_has_skip_reason_field() {
        let filters = RepairPlanFilters {
            skip_reason: vec!["missing-default".into(), "ambiguous-*".into()],
            ..Default::default()
        };
        let json = serde_json::to_value(&filters).unwrap();
        assert_eq!(json["skip_reason"][0], "missing-default");
        assert_eq!(json["skip_reason"][1], "ambiguous-*");

        // Default = empty vec
        let default = RepairPlanFilters::default();
        let default_json = serde_json::to_value(&default).unwrap();
        assert_eq!(default_json["skip_reason"], serde_json::json!([]));
    }

    fn finding_bom_marker(path: &str) -> Finding {
        // Mirrors the graph-diagnostic-sourced finding `check_graph_diagnostics`
        // produces from the `bom-marker` `Diagnostic` `graph::build::parse_document`
        // pushes (NRN-385) — the built-in repair dispatches on `finding.code`,
        // not on the body shape, so this reconstruction is representative.
        let diagnostic = crate::domain::Diagnostic::warning(
            "bom-marker",
            "document begins with a UTF-8 byte-order mark (BOM)",
        );
        Finding::from_graph_diagnostic(path.into(), diagnostic)
    }

    #[test]
    fn bom_marker_finding_plans_a_built_in_strip_bom_change() {
        let finding = finding_bom_marker("bom.md");
        let index = index_for(&["bom.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );
        assert_eq!(result.plan.operations.len(), 1);
        assert_eq!(result.skipped.len(), 0);
        assert_eq!(result.plan.operations[0].kind, "strip_bom");
        assert_eq!(
            op_str(&result, 0, "repair_rule"),
            Some("built-in:strip-bom")
        );
        assert_eq!(op_str(&result, 0, "finding_code"), Some("bom-marker"));
        assert_eq!(op_str(&result, 0, "document_hash"), Some("hash-bom.md"));
        assert!(op_field(&result, 0, "field").is_none());
    }

    #[test]
    fn bom_marker_finding_without_a_document_hash_is_skipped_as_missing_hash() {
        let finding = finding_bom_marker("bom.md");
        let index = index_for(&[]); // bom.md absent from the index
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );
        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::MissingHash);
    }

    #[test]
    fn bom_marker_user_rule_overrides_the_built_in_strip() {
        // A configured repair rule matching `code: bom-marker` wins over the
        // built-in — same precedent as closest-match-stem being overridable.
        let finding = finding_bom_marker("bom.md");
        let config = RepairConfig {
            rules: vec![make_rule(
                "custom-bom-handling",
                "bom-marker",
                None,
                None,
                RepairAction::SetFrontmatter {
                    field: "needs-review".into(),
                    value: json!(true),
                },
            )],
        };
        let index = index_for(&["bom.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(result.plan.operations.len(), 1);
        assert_eq!(result.plan.operations[0].kind, "set_frontmatter");
        assert_eq!(
            op_str(&result, 0, "repair_rule"),
            Some("custom-bom-handling")
        );
    }

    fn finding_nonportable_filename(path: &str, issues: Vec<&str>) -> Finding {
        Finding::nonportable_filename(path.into(), issues.into_iter().map(String::from).collect())
    }

    #[test]
    fn nonportable_filename_finding_is_never_auto_repaired() {
        let finding = finding_nonportable_filename(
            "weird:name.md",
            vec!["segment 'weird:name.md' contains illegal character ':'"],
        );
        let index = index_for(&["weird:name.md"]);
        let result = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );
        assert_eq!(result.plan.operations.len(), 0);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].skip_reason, SkipReason::NoRuleMatched);
        assert_eq!(
            result.summary.skipped.by_reason.get("no-rule-matched"),
            Some(&1)
        );
        assert!(result.skipped[0]
            .reason
            .contains("diagnosed, not auto-repaired"));
    }

    #[test]
    fn native_op_fields_carry_the_expected_wire_shape() {
        // Wire-byte guard (ADR 0024): `op_fields_from_change` emits the `fields`
        // object the on-disk plan carries — `operation` is dropped (it becomes the
        // op `kind`), `move_document` remaps `path`/`destination` → `src`/`dst`,
        // linkage rides as present keys, and absent-`finding_rule` omits the key.
        // `serde_json::Value` compares by key set + value, so key order is
        // irrelevant.
        let move_link_risk = crate::standards::repair::link_risk::classify(
            camino::Utf8Path::new("notes/c.md"),
            camino::Utf8Path::new("archive/c.md"),
            &[],
            &[],
        );

        let set_op = ApplyOp {
            change_id: "id1".into(),
            path: "notes/a.md".into(),
            document_hash: "h1".into(),
            finding_code: Some("value-not-allowed".into()),
            finding_rule: Some("task-status".into()),
            repair_rule: Some("fix-status".into()),
            operation: "set_frontmatter".into(),
            field: Some("status".into()),
            expected_old_value: Some(json!("someday")),
            new_value: Some(json!("backlog")),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        assert_eq!(
            op_fields_from_change(&set_op),
            json!({
                "change_id": "id1",
                "path": "notes/a.md",
                "document_hash": "h1",
                "finding_code": "value-not-allowed",
                "finding_rule": "task-status",
                "repair_rule": "fix-status",
                "field": "status",
                "expected_old_value": "someday",
                "new_value": "backlog",
            }),
        );

        // `strip_bom` is the minimal shape: no field/value payload, no move keys.
        let bom_op = ApplyOp {
            change_id: "id4".into(),
            path: "notes/d.md".into(),
            document_hash: "h4".into(),
            finding_code: Some("bom-marker".into()),
            finding_rule: None,
            repair_rule: Some("strip-bom".into()),
            operation: "strip_bom".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        assert_eq!(
            op_fields_from_change(&bom_op),
            json!({
                "change_id": "id4",
                "path": "notes/d.md",
                "document_hash": "h4",
                "finding_code": "bom-marker",
                "repair_rule": "strip-bom",
            }),
        );

        // A `None` finding_rule omits the key entirely.
        let rewrite_op = ApplyOp {
            change_id: "id2".into(),
            path: "notes/b.md".into(),
            document_hash: "h2".into(),
            finding_code: Some("link-target-missing".into()),
            finding_rule: None,
            repair_rule: Some("built-in:closest-match-stem".into()),
            operation: "rewrite_link".into(),
            field: None,
            expected_old_value: Some(json!("Norn Brand")),
            new_value: Some(json!("norn-brand")),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        assert_eq!(
            op_fields_from_change(&rewrite_op),
            json!({
                "change_id": "id2",
                "path": "notes/b.md",
                "document_hash": "h2",
                "finding_code": "link-target-missing",
                "repair_rule": "built-in:closest-match-stem",
                "expected_old_value": "Norn Brand",
                "new_value": "norn-brand",
            }),
        );

        // move_document: `path` → `src`, `destination` → `dst`, and the planner
        // link_risk / force / parents ride along.
        let move_op = ApplyOp {
            change_id: "id3".into(),
            path: "notes/c.md".into(),
            document_hash: "h3".into(),
            finding_code: Some("document-misrouted".into()),
            finding_rule: Some("route".into()),
            repair_rule: Some("move-it".into()),
            operation: "move_document".into(),
            field: None,
            expected_old_value: None,
            new_value: None,
            destination: Some("archive/c.md".into()),
            link_risk: Some(move_link_risk.clone()),
            warnings: vec![],
            force: true,
            parents: true,
        };
        assert_eq!(
            op_fields_from_change(&move_op),
            json!({
                "change_id": "id3",
                "src": "notes/c.md",
                "document_hash": "h3",
                "finding_code": "document-misrouted",
                "finding_rule": "route",
                "repair_rule": "move-it",
                "dst": "archive/c.md",
                "link_risk": serde_json::to_value(&move_link_risk).unwrap(),
                "force": true,
                "parents": true,
            }),
        );
    }
}
