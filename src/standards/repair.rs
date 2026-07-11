pub mod closest_match;
pub mod destination;
pub mod link_risk;
pub mod warnings;

use std::collections::{BTreeMap, BTreeSet};

use crate::core::Severity;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::standards::config::{RepairAction, RepairConfig, RepairRule, RepairRuleMatch};
use crate::standards::findings::{Finding, FindingBody};

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
    /// Alias collides with an existing doc stem and cannot be safely rewritten.
    AliasShadowed,
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
            SkipReason::AliasShadowed => "alias-shadowed",
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
pub struct RepairPlan {
    pub schema_version: u32,
    pub vault_root: Utf8PathBuf,
    pub source_filters: RepairPlanFilters,
    pub summary: RepairPlanSummary,
    pub changes: Vec<PlannedChange>,
    pub(crate) skipped_findings: Vec<SkippedFinding>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub footnotes: Vec<PlanFootnote>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepairPlanSummary {
    pub findings: usize,
    pub planned_changes: usize,
    pub skipped: SkippedSummary,
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
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FootnoteDetails {
    ClosestMatch(ClosestMatchDetails),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClosestMatchDetails {
    pub original_target: String,
    pub normalized_target: String,
    pub candidate_stem: String,
    pub normalized_distance: usize,
    pub slug_normalized_identity: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlanFootnote {
    pub change_id: String,
    pub kind: FootnoteKind,
    pub confidence: Confidence,
    pub details: FootnoteDetails,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PlannedChange {
    pub change_id: String,
    pub path: Utf8PathBuf,
    pub document_hash: String,
    pub finding_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finding_rule: Option<String>,
    pub repair_rule: String,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_old_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<Utf8PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) link_risk: Option<crate::standards::repair::link_risk::LinkRisk>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<crate::standards::repair::warnings::PlanWarning>,
    /// When true, `apply_move` will remove an existing destination before
    /// renaming. Defaults to false; skips serialization when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// When true, intermediate destination subdirectories are created at apply
    /// time (analogous to `mkdir -p`). Propagated from `move_folder` ops.
    /// Defaults to false; skips serialization when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parents: bool,
}

fn derive_change_id(
    path: &Utf8PathBuf,
    finding_code: &str,
    expected_old_value: Option<&Value>,
    occurrence_index: u32,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(finding_code.as_bytes());
    hasher.update(b"\0");
    if let Some(v) = expected_old_value {
        hasher.update(v.to_string().as_bytes());
    }
    hasher.update(b"\0");
    hasher.update(occurrence_index.to_le_bytes());
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

const DEFAULT_MEDIUM_THRESHOLD: f64 = 0.7;

enum ClosestMatchOutcome {
    Change {
        change: Box<PlannedChange>,
        footnote: Box<PlanFootnote>,
    },
    TiedSkip {
        skipped: Box<SkippedFinding>,
    },
    NoMatch,
}

fn handle_closest_match(
    finding: &Finding,
    stem_corpus: &[&str],
    documents: &[crate::core::Document],
    document_hashes: &BTreeMap<Utf8PathBuf, String>,
    occurrence_counts: &mut BTreeMap<(Utf8PathBuf, String, String), u32>,
    medium_threshold: f64,
) -> ClosestMatchOutcome {
    let FindingBody::LinkIssue { link } = &finding.body else {
        return ClosestMatchOutcome::NoMatch;
    };
    let broken_target = link.target.as_str();

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

            let change = PlannedChange {
                change_id: change_id.clone(),
                path: finding.path.clone(),
                document_hash,
                finding_code: finding.code.clone(),
                finding_rule: None,
                repair_rule: "built-in:closest-match-stem".to_string(),
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
    index: &crate::core::GraphIndex,
) -> RepairPlan {
    let document_hashes: BTreeMap<Utf8PathBuf, String> = index
        .documents
        .iter()
        .map(|d| (d.path.clone(), d.hash.clone()))
        .collect();
    let stem_corpus: Vec<&str> = index.documents.iter().map(|d| d.stem.as_str()).collect();
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
        if matches!(finding.body, FindingBody::ReferenceType { .. }) {
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
                } else {
                    let skip = skip_reason_for_body(&finding.body);
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

    RepairPlan {
        schema_version: REPAIR_PLAN_SCHEMA_VERSION,
        vault_root,
        source_filters: filters,
        summary: RepairPlanSummary {
            findings: findings.len(),
            planned_changes: changes.len(),
            skipped: skipped_summary,
        },
        changes,
        skipped_findings: skipped,
        footnotes,
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
    documents: &[crate::core::Document],
    occurrence_index: u32,
) -> Result<PlannedChange, (SkipReason, Option<String>)> {
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
        RepairAction::SetFrontmatter { field, value } => PlannedChange {
            change_id: change_id.clone(),
            path: finding.path.clone(),
            document_hash,
            finding_code: finding.code.clone(),
            finding_rule: finding_rule(finding),
            repair_rule,
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
        RepairAction::RemoveFrontmatter { field } => PlannedChange {
            change_id: change_id.clone(),
            path: finding.path.clone(),
            document_hash,
            finding_code: finding.code.clone(),
            finding_rule: finding_rule(finding),
            repair_rule,
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
        RepairAction::AddFrontmatter { field, value } => PlannedChange {
            change_id: change_id.clone(),
            path: finding.path.clone(),
            document_hash,
            finding_code: finding.code.clone(),
            finding_rule: finding_rule(finding),
            repair_rule,
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

            PlannedChange {
                change_id,
                path: finding.path.clone(),
                document_hash,
                finding_code: finding.code.clone(),
                finding_rule: finding_rule(finding),
                repair_rule,
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

/// Derive the fine-grained `SkipReason` variant from a finding's body.
/// Used at emit sites that previously emitted the coarse `Unsupported` or `Ambiguous` variants.
fn skip_reason_for_body(body: &FindingBody) -> SkipReason {
    match body {
        FindingBody::LinkIssue { link } if link.status == crate::core::LinkStatus::Ambiguous => {
            SkipReason::AmbiguousTarget
        }
        FindingBody::LinkIssue { .. } => SkipReason::LinkDecisionNeeded,
        FindingBody::RequiredFrontmatterMissing { .. } => SkipReason::MissingDefault,
        FindingBody::DisallowedValue { .. }
        | FindingBody::InvalidFieldType { .. }
        | FindingBody::ExceedsMaxLength { .. }
        | FindingBody::ForbiddenField { .. }
        | FindingBody::DocumentMisrouted { .. }
        | FindingBody::ReferenceType { .. }
        | FindingBody::AliasMalformed { .. }
        | FindingBody::AliasDuplicateAcrossDocs { .. } => SkipReason::NoRuleMatched,
        FindingBody::AliasShadowedByStem { .. } => SkipReason::AliasShadowed,
        FindingBody::GraphDiagnostic { .. } => SkipReason::GraphDiagnostic,
    }
}

fn skipped_finding(
    finding: &Finding,
    skip_reason: SkipReason,
    reason_override: Option<String>,
) -> SkippedFinding {
    let (reason, next_actions) = match &finding.body {
        FindingBody::LinkIssue { link } if link.status == crate::core::LinkStatus::Ambiguous => (
            "ambiguous link target".to_string(),
            vec![
                "change the link to an explicit path".to_string(),
                "rename one duplicate candidate".to_string(),
                "rerun repair plan after disambiguation".to_string(),
            ],
        ),
        FindingBody::LinkIssue { .. } => (
            "link repair requires an explicit path/link decision".to_string(),
            vec![
                "create the missing target or target anchor".to_string(),
                "rewrite the link manually".to_string(),
                "rerun validate after resolving the link".to_string(),
            ],
        ),
        FindingBody::RequiredFrontmatterMissing { field, .. } => (
            "missing field has no configured deterministic default".to_string(),
            vec![
                format!("add a repair rule that sets {field} when safe"),
                "fill the field manually and rerun validate".to_string(),
            ],
        ),
        FindingBody::DisallowedValue { field, .. }
        | FindingBody::InvalidFieldType { field, .. }
        | FindingBody::ExceedsMaxLength { field, .. }
        | FindingBody::ForbiddenField { field, .. } => (
            "no configured deterministic repair rule matched".to_string(),
            vec![
                format!("add a repair rule for field {field}"),
                "rerun repair plan after updating config".to_string(),
            ],
        ),
        FindingBody::ReferenceType {
            field,
            allowed_types,
            ..
        } => (
            "typed-reference violation cannot be repaired deterministically".to_string(),
            vec![
                format!(
                    "repoint '{field}' at a document whose type is one of: {}",
                    allowed_types.join(", ")
                ),
                "or change the target document's type if the reference is right".to_string(),
                "rerun validate after fixing the reference".to_string(),
            ],
        ),
        FindingBody::DocumentMisrouted { .. } => (
            "no configured move_document repair rule matched this misrouted document".to_string(),
            vec![
                "review allowed_paths and current document location".to_string(),
                "add a move_document repair rule matching this finding's code".to_string(),
            ],
        ),
        FindingBody::GraphDiagnostic { .. } => (
            "graph diagnostic cannot be repaired deterministically".to_string(),
            vec![
                "inspect the diagnostic detail".to_string(),
                "fix the document manually and rerun validate".to_string(),
            ],
        ),
        FindingBody::AliasMalformed { field, .. } => (
            "malformed alias entries cannot be repaired deterministically".to_string(),
            vec![
                format!("edit the '{field}' frontmatter list to contain only scalar strings"),
                "rerun validate after fixing the entries".to_string(),
            ],
        ),
        FindingBody::AliasShadowedByStem {
            alias_value,
            shadowing_doc_path,
        } => (
            "alias shadowed by a doc stem cannot be repaired deterministically".to_string(),
            vec![
                format!(
                    "remove or rename alias '{alias_value}' on this doc, or rename {shadowing_doc_path} to free the stem"
                ),
                "rerun validate after fixing the conflict".to_string(),
            ],
        ),
        FindingBody::AliasDuplicateAcrossDocs { alias_value, .. } => (
            "alias duplicated across docs cannot be repaired deterministically".to_string(),
            vec![
                format!(
                    "pick a canonical doc for alias '{alias_value}', remove the alias from the others"
                ),
                "rerun validate after fixing the conflict".to_string(),
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
        severity: finding.severity.clone(),
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
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { rule, .. }
        | FindingBody::DisallowedValue { rule, .. }
        | FindingBody::InvalidFieldType { rule, .. }
        | FindingBody::ExceedsMaxLength { rule, .. }
        | FindingBody::ForbiddenField { rule, .. }
        | FindingBody::DocumentMisrouted { rule, .. }
        | FindingBody::ReferenceType { rule, .. } => rule.clone(),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::AliasMalformed { .. }
        | FindingBody::AliasShadowedByStem { .. }
        | FindingBody::AliasDuplicateAcrossDocs { .. } => None,
    }
}

fn finding_field(finding: &Finding) -> Option<String> {
    match &finding.body {
        FindingBody::RequiredFrontmatterMissing { field, .. }
        | FindingBody::DisallowedValue { field, .. }
        | FindingBody::InvalidFieldType { field, .. }
        | FindingBody::ExceedsMaxLength { field, .. }
        | FindingBody::ForbiddenField { field, .. }
        | FindingBody::AliasMalformed { field, .. }
        | FindingBody::ReferenceType { field, .. } => Some(field.clone()),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::DocumentMisrouted { .. }
        | FindingBody::AliasShadowedByStem { .. }
        | FindingBody::AliasDuplicateAcrossDocs { .. } => None,
    }
}

fn finding_actual_value(finding: &Finding) -> Option<&Value> {
    match &finding.body {
        FindingBody::DisallowedValue { actual_value, .. }
        | FindingBody::InvalidFieldType { actual_value, .. }
        | FindingBody::ExceedsMaxLength { actual_value, .. }
        | FindingBody::ForbiddenField { actual_value, .. } => Some(actual_value),
        FindingBody::GraphDiagnostic { .. }
        | FindingBody::LinkIssue { .. }
        | FindingBody::RequiredFrontmatterMissing { .. }
        | FindingBody::DocumentMisrouted { .. }
        | FindingBody::ReferenceType { .. }
        | FindingBody::AliasMalformed { .. }
        | FindingBody::AliasShadowedByStem { .. }
        | FindingBody::AliasDuplicateAcrossDocs { .. } => None,
    }
}

fn finding_target(finding: &Finding) -> Option<String> {
    match &finding.body {
        FindingBody::LinkIssue { link } => Some(link.target.clone()),
        _ => None,
    }
}

fn finding_candidates(finding: &Finding) -> Vec<Utf8PathBuf> {
    match &finding.body {
        FindingBody::LinkIssue { link } => link.candidates.clone(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Link, LinkKind, LinkStatus, Severity, UnresolvedReason};
    use crate::standards::config::{RepairAction, RepairRule, RepairRuleMatch};
    use crate::standards::findings::{Finding, FindingBody};
    use serde_json::json;

    fn vault_root() -> Utf8PathBuf {
        "/vault".into()
    }

    fn finding_disallowed_value(path: &str, field: &str, value: serde_json::Value) -> Finding {
        Finding {
            code: "value-not-allowed".into(),
            severity: Severity::Warning,
            path: path.into(),
            message: format!("frontmatter field has a disallowed value: {field}"),
            body: FindingBody::DisallowedValue {
                rule: Some("task-status".into()),
                field: field.into(),
                actual_value: value,
                allowed_values: vec![json!("backlog"), json!("completed")],
            },
        }
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

    fn doc(path: &str, hash: &str) -> crate::core::Document {
        crate::core::Document {
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

    fn index_for(paths: &[&str]) -> crate::core::GraphIndex {
        let documents = paths.iter().map(|p| doc(p, &format!("hash-{p}"))).collect();
        crate::core::GraphIndex {
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
    fn test_index_with_stems(pairs: &[(&str, &str)]) -> crate::core::GraphIndex {
        let documents = pairs
            .iter()
            .map(|(path, stem)| {
                let mut d = doc(path, &format!("hash-{path}"));
                d.stem = stem.to_string();
                d
            })
            .collect();
        crate::core::GraphIndex {
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
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.skipped_findings.len(), 0);
        assert_eq!(plan.changes[0].operation, "set_frontmatter");
        assert_eq!(plan.changes[0].field.as_deref(), Some("status"));
        assert_eq!(plan.changes[0].new_value, Some(json!("backlog")));
        assert_eq!(plan.changes[0].expected_old_value, Some(json!("someday")));
        assert_eq!(plan.changes[0].document_hash, "hash-task.md");
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
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(
            plan.changes.len(),
            0,
            "reference-type findings must never plan changes: {:?}",
            plan.changes
        );
        assert_eq!(plan.skipped_findings.len(), 1);
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
            SkipReason::NoRuleMatched
        );
        assert_eq!(plan.skipped_findings[0].field.as_deref(), Some("parent"));
    }

    #[test]
    fn unmatched_finding_routes_to_skipped_with_no_rule_matched_reason() {
        let finding = finding_disallowed_value("task.md", "status", json!("someday"));
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["task.md"]);
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(plan.changes.len(), 0);
        assert_eq!(plan.skipped_findings.len(), 1);
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
            SkipReason::NoRuleMatched
        );
        assert_eq!(
            plan.summary.skipped.by_reason.get("no-rule-matched"),
            Some(&1)
        );
        assert_eq!(plan.summary.skipped.by_reason.get("ambiguous-target"), None);
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
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(plan.skipped_findings.len(), 1);
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
            SkipReason::AmbiguousTarget
        );
        assert_eq!(plan.skipped_findings[0].candidates.len(), 2);
        assert_eq!(
            plan.summary.skipped.by_reason.get("ambiguous-target"),
            Some(&1)
        );
        assert_eq!(plan.summary.skipped.by_reason.get("no-rule-matched"), None);
    }

    #[test]
    fn unresolved_link_finding_routes_to_skipped_with_link_decision_needed_reason() {
        let finding = finding_link_unresolved("note.md", "missing");
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["note.md"]);
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
            SkipReason::LinkDecisionNeeded
        );
        assert_eq!(
            plan.summary.skipped.by_reason.get("link-decision-needed"),
            Some(&1)
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
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(plan.changes.len(), 0);
        assert_eq!(plan.skipped_findings.len(), 1);
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
            SkipReason::MissingHash
        );
        // The reason text reflects the new clearer message.
        assert!(plan.skipped_findings[0].reason.contains("hash not present"));
        assert_eq!(plan.summary.skipped.by_reason.get("missing-hash"), Some(&1));
    }

    fn finding_required_missing(path: &str, field: &str, rule: Option<&str>) -> Finding {
        Finding {
            code: "frontmatter-required-field-missing".into(),
            severity: Severity::Warning,
            path: path.into(),
            message: format!("required frontmatter field is missing: {field}"),
            body: FindingBody::RequiredFrontmatterMissing {
                rule: rule.map(Into::into),
                field: field.into(),
            },
        }
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
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.skipped_findings.len(), 0);
        let change = &plan.changes[0];
        assert_eq!(change.operation, "add_frontmatter");
        assert_eq!(change.field.as_deref(), Some("kind"));
        assert_eq!(change.new_value, Some(json!("research")));
        assert_eq!(change.expected_old_value, None);
        assert_eq!(change.document_hash, "hash-task.md");
    }

    #[test]
    fn required_missing_no_rule_routes_to_missing_default_skip() {
        let finding = finding_required_missing("task.md", "kind", Some("typed-note"));
        // No rules → the planner cannot find a deterministic default for this field.
        let config = RepairConfig { rules: vec![] };
        let index = index_for(&["task.md"]);
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            vec![finding],
            &config,
            &index,
        );
        assert_eq!(plan.changes.len(), 0);
        assert_eq!(plan.skipped_findings.len(), 1);
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
            SkipReason::MissingDefault
        );
        assert!(
            plan.skipped_findings[0]
                .reason
                .contains("missing field has no configured deterministic default"),
            "unexpected reason text: {}",
            plan.skipped_findings[0].reason
        );
        assert_eq!(
            plan.summary.skipped.by_reason.get("missing-default"),
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
        let plan = plan_repairs(
            vault_root(),
            RepairPlanFilters::default(),
            findings,
            &config,
            &index,
        );
        assert_eq!(plan.summary.findings, 3);
        assert_eq!(plan.summary.planned_changes, 1);
        assert_eq!(plan.summary.skipped.total, 2);
        assert_eq!(
            plan.summary.skipped.by_reason.get("link-decision-needed"),
            Some(&1)
        );
        assert_eq!(
            plan.summary.skipped.by_reason.get("ambiguous-target"),
            Some(&1)
        );
        assert_eq!(plan.summary.skipped.by_reason.get("missing-hash"), None);
    }

    #[test]
    fn plan_v5_serde_round_trip_with_footnote() {
        let plan = RepairPlan {
            schema_version: 5,
            vault_root: "/tmp/v".into(),
            source_filters: RepairPlanFilters::default(),
            summary: RepairPlanSummary {
                findings: 1,
                planned_changes: 1,
                skipped: SkippedSummary::default(),
            },
            changes: vec![PlannedChange {
                change_id: "abc12345".into(),
                path: "doc.md".into(),
                document_hash: "h".into(),
                finding_code: "link-target-missing".into(),
                finding_rule: None,
                repair_rule: "built-in:closest-match-stem".into(),
                operation: "rewrite_link".into(),
                field: None,
                expected_old_value: Some(Value::String("Norn Brand".into())),
                new_value: Some(Value::String("norn-brand".into())),
                destination: None,
                link_risk: None,
                warnings: vec![],
                force: false,
                parents: false,
            }],
            skipped_findings: vec![],
            footnotes: vec![PlanFootnote {
                change_id: "abc12345".into(),
                kind: FootnoteKind::ClosestMatchSuggestion,
                confidence: Confidence::High,
                details: FootnoteDetails::ClosestMatch(ClosestMatchDetails {
                    original_target: "Norn Brand".into(),
                    normalized_target: "norn-brand".into(),
                    candidate_stem: "norn-brand".into(),
                    normalized_distance: 0,
                    slug_normalized_identity: true,
                }),
            }],
        };

        let json = serde_json::to_string(&plan).unwrap();
        let round_tripped: RepairPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped.schema_version, 5);
        assert_eq!(round_tripped.changes.len(), 1);
        assert_eq!(round_tripped.changes[0].change_id, "abc12345");
        assert_eq!(round_tripped.footnotes.len(), 1);
        assert!(matches!(
            round_tripped.footnotes[0].confidence,
            Confidence::High
        ));
    }

    #[test]
    fn closest_match_proposes_high_confidence_rewrite_on_target_missing() {
        // A doc links to [[Norn Brand]], but the resolution is target-missing.
        // The vault has norn-brand.md — slug-normalize identity → High.
        // source.md must also appear in the index so its document hash is found.
        let finding = finding_link_unresolved("source.md", "Norn Brand");
        let index =
            test_index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);

        let plan = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(plan.changes.len(), 1, "expected exactly one PlannedChange");
        let change = &plan.changes[0];
        assert_eq!(change.operation, "rewrite_link");
        assert_eq!(change.finding_code, "link-target-missing");
        assert_eq!(
            change.expected_old_value,
            Some(Value::String("Norn Brand".into()))
        );
        assert_eq!(change.new_value, Some(Value::String("norn-brand".into())));

        assert_eq!(plan.footnotes.len(), 1, "expected exactly one PlanFootnote");
        let footnote = &plan.footnotes[0];
        assert_eq!(footnote.change_id, change.change_id);
        assert!(matches!(
            footnote.kind,
            FootnoteKind::ClosestMatchSuggestion
        ));
        assert!(matches!(footnote.confidence, Confidence::High));
        match &footnote.details {
            FootnoteDetails::ClosestMatch(d) => {
                assert_eq!(d.original_target, "Norn Brand");
                assert_eq!(d.candidate_stem, "norn-brand");
                assert!(d.slug_normalized_identity);
                assert_eq!(d.normalized_distance, 0);
            }
        }
    }

    #[test]
    fn closest_match_proposes_medium_confidence_rewrite_on_target_missing() {
        // Broken target "norn-brnd" vs stem "norn-brand": 1-char edit on a
        // 10-char string → ratio 0.9 → Medium (above 0.7 threshold, below
        // post-normalize identity).
        let finding = finding_link_unresolved("source.md", "norn-brnd");
        let index =
            test_index_with_stems(&[("source.md", "source"), ("norn-brand.md", "norn-brand")]);

        let plan = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(plan.changes.len(), 1, "expected exactly one PlannedChange");
        let change = &plan.changes[0];
        assert_eq!(change.operation, "rewrite_link");
        assert_eq!(
            change.expected_old_value,
            Some(Value::String("norn-brnd".into()))
        );
        assert_eq!(change.new_value, Some(Value::String("norn-brand".into())));

        assert_eq!(plan.footnotes.len(), 1, "expected exactly one PlanFootnote");
        let footnote = &plan.footnotes[0];
        assert_eq!(footnote.change_id, change.change_id);
        assert!(matches!(
            footnote.kind,
            FootnoteKind::ClosestMatchSuggestion
        ));
        assert!(matches!(footnote.confidence, Confidence::Medium));
        match &footnote.details {
            FootnoteDetails::ClosestMatch(d) => {
                assert_eq!(d.original_target, "norn-brnd");
                assert_eq!(d.candidate_stem, "norn-brand");
                assert!(!d.slug_normalized_identity);
                assert_eq!(d.normalized_distance, 1);
            }
        }
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

        let plan = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(plan.changes.len(), 0);
        assert_eq!(plan.footnotes.len(), 0);
        assert_eq!(plan.skipped_findings.len(), 1);
        let skipped = &plan.skipped_findings[0];
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

        let plan = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(plan.changes.len(), 0);
        assert_eq!(plan.footnotes.len(), 0);
        assert_eq!(plan.skipped_findings.len(), 1);
        assert_eq!(
            plan.skipped_findings[0].skip_reason,
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
        let plan = plan_repairs(
            "/tmp/v".into(),
            filters,
            vec![high_finding, medium_finding],
            &RepairConfig::default(),
            &index,
        );

        // Only the high-confidence proposal survives.
        assert_eq!(
            plan.changes.len(),
            1,
            "expected only high-confidence change"
        );
        assert_eq!(
            plan.footnotes.len(),
            1,
            "expected only high-confidence footnote"
        );
        assert!(matches!(plan.footnotes[0].confidence, Confidence::High));
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
        let plan = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![high_finding, medium_finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(plan.changes.len(), 2);
        assert_eq!(plan.footnotes.len(), 2);
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

        let plan = plan_repairs(
            "/tmp/v".into(),
            RepairPlanFilters::default(),
            vec![finding],
            &RepairConfig::default(),
            &index,
        );

        assert_eq!(plan.skipped_findings.len(), 1);
        let skipped = &plan.skipped_findings[0];
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
    fn skip_reason_has_eight_variants_with_stable_codes() {
        use SkipReason::*;
        let all = [
            MissingDefault,
            LinkDecisionNeeded,
            NoRuleMatched,
            AliasShadowed,
            GraphDiagnostic,
            AmbiguousTarget,
            MissingHash,
            PreconditionFailed,
        ];
        assert_eq!(all.len(), 8);

        assert_eq!(MissingDefault.code(), "missing-default");
        assert_eq!(LinkDecisionNeeded.code(), "link-decision-needed");
        assert_eq!(NoRuleMatched.code(), "no-rule-matched");
        assert_eq!(AliasShadowed.code(), "alias-shadowed");
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
    fn create_document_planned_change_serializes_round_trip() {
        let pc = PlannedChange {
            change_id: "abc12345".into(),
            path: "Workspaces/foo/tasks/bar.md".into(),
            document_hash: "0".repeat(64),
            finding_code: "imperative-create".into(),
            finding_rule: None,
            repair_rule: "vault-new".into(),
            operation: "create_document".into(),
            field: None,
            expected_old_value: None,
            new_value: Some(serde_json::json!({
                "frontmatter": {"type": "task", "status": "backlog"},
                "body": ""
            })),
            destination: None,
            link_risk: None,
            warnings: vec![],
            force: false,
            parents: false,
        };
        let json = serde_json::to_string(&pc).unwrap();
        let round: PlannedChange = serde_json::from_str(&json).unwrap();
        assert_eq!(round.operation, "create_document");
        assert_eq!(round.path.as_str(), "Workspaces/foo/tasks/bar.md");
        assert!(round.new_value.is_some());
    }

    #[test]
    fn replace_body_op_is_a_valid_operation() {
        let plan_json = r#"{
            "schema_version": 8,
            "vault_root": "/tmp/vault",
            "source_filters": {
                "code": [],
                "severity": [],
                "field": [],
                "rule": [],
                "path": [],
                "target": [],
                "reason": [],
                "skip_reason": []
            },
            "summary": {
                "findings": 1,
                "planned_changes": 1,
                "skipped": {
                    "by_reason": {},
                    "total": 0
                }
            },
            "changes": [{
                "change_id": "abcd1234",
                "path": "notes/foo.md",
                "document_hash": "deadbeef",
                "finding_code": "operator-mutation",
                "repair_rule": "vault-set",
                "operation": "replace_body",
                "new_value": "fresh body content"
            }],
            "skipped_findings": [],
            "footnotes": []
        }"#;
        let plan: RepairPlan = serde_json::from_str(plan_json).expect("plan should deserialize");
        assert_eq!(plan.changes[0].operation, "replace_body");
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
            severity: crate::core::Severity::Warning,
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
}
