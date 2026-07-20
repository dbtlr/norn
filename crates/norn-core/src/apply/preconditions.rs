//! Owner-set preconditions (ADR 0015) — the pre-write identity barrier.
//!
//! A `MigrationPlan` carries first-class owner-set preconditions: each asserts
//! that an exact, sorted set of vault-relative owner paths — selected by a
//! canonical stem, a conjunctive frontmatter equality, or the resolved stem of a
//! named create operation — matches what the plan expected at planning time.
//! [`evaluate_owner_preconditions`] proves them once against a fresh
//! [`GraphIndex`]; the applier calls it as ONE barrier under the mutation lock
//! before any operation writes, and refuses the whole plan (writing nothing) on
//! a mismatch — a coded `owner-set-mismatch` refusal with expected and actual
//! paths, every operation `not_run`.
//!
//! ## Scope of this port
//!
//! The barrier's create-path RESOLUTION half — turning `{{seq}}` create ops into
//! concrete stems so `stem_from_operation` selectors can name them (the donor's
//! `resolve_create_paths`) — is fused with the typed-op → change expansion that
//! lands with the mutation verbs, so it ports with the executor. This module
//! ports the EVALUATION core: given already-resolved operation stems, it selects
//! the current owners and compares. Both consumers of that resolution
//! (`operation_stems`, `create_changes_by_stem`) arrive as value inputs.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;

use crate::apply::report::{
    ApplyError, ApplyOutcome, ApplyReport, ApplyReportOp, ApplyReportPrecondition, OpStatus,
    PreconditionStatus, APPLY_REPORT_SCHEMA_VERSION,
};
use crate::domain::{Document, GraphIndex};
use crate::plan::{MigrationOp, MigrationPlan, OwnerSelector, PlanPrecondition};

/// Evaluate every owner-set precondition in `plan` against `index`, returning one
/// [`ApplyReportPrecondition`] per plan precondition (in plan order).
///
/// `operation_stems` maps a create operation's id to its resolved filename stem
/// (empty when the plan has no `stem_from_operation` selectors), and
/// `create_changes_by_stem` maps a normalized (lowercased) stem to the set of
/// create-change indices producing it — used to refuse an internally
/// contradictory plan where two creates would claim the same fresh stem.
///
/// Returns `Err` for a plan-STRUCTURE fault (duplicate precondition id, empty
/// selector, a `stem_from_operation` naming a missing/non-create op) — the
/// applier turns that into a preflight refusal. A clean owner-set MISMATCH is not
/// an error: it is a `Failed` precondition carrying the `owner-set-mismatch`
/// code, and the applier maps a `Failed` precondition to a refusal report via
/// [`build_owner_precondition_refusal_report`].
pub fn evaluate_owner_preconditions(
    plan: &MigrationPlan,
    index: &GraphIndex,
    operation_stems: &HashMap<String, String>,
    create_changes_by_stem: &HashMap<String, BTreeSet<usize>>,
) -> Result<Vec<ApplyReportPrecondition>> {
    let mut precondition_ids = BTreeSet::new();
    for precondition in &plan.preconditions {
        if !precondition_ids.insert(precondition.id()) {
            anyhow::bail!(
                "duplicate owner-set precondition id in MigrationPlan: {}",
                precondition.id()
            );
        }
    }

    let mut results = plan
        .preconditions
        .iter()
        .map(|precondition| -> Result<ApplyReportPrecondition> {
            Ok(match precondition {
                PlanPrecondition::OwnerSet {
                    id,
                    selector,
                    expected_paths,
                } => {
                    let mut expected_paths = expected_paths.clone();
                    expected_paths.sort();
                    expected_paths.dedup();
                    let mut actual_paths = match selector {
                        OwnerSelector::Stem { stem } => {
                            if stem.is_empty() {
                                anyhow::bail!(
                                    "owner-set precondition '{}' has an empty stem selector",
                                    precondition.id()
                                );
                            }
                            scan_by_stem(index, stem)
                        }
                        OwnerSelector::StemFromOperation {
                            stem_from_operation,
                        } => {
                            let stem = operation_stems.get(stem_from_operation).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "owner-set precondition '{}' references missing or non-create operation id '{}'",
                                    precondition.id(),
                                    stem_from_operation
                                )
                            })?;
                            scan_by_stem(index, stem)
                        }
                        OwnerSelector::Eq { eq } => {
                            if eq.is_empty() {
                                anyhow::bail!(
                                    "owner-set precondition '{}' has an empty eq selector",
                                    precondition.id()
                                );
                            }
                            // Parse each `field:value` predicate ONCE, before the
                            // document scan, rather than re-parsing the strings per
                            // document.
                            let predicates = eq
                                .iter()
                                .map(|predicate| {
                                    crate::query::filter_args::parse_field_value(
                                        predicate,
                                        "owner_set.eq",
                                    )
                                })
                                .collect::<Result<Vec<_>>>()?;
                            index
                                .documents
                                .iter()
                                .filter(|document| document_matches_eq(document, &predicates))
                                .map(|document| document.path.to_string())
                                .collect::<Vec<_>>()
                        }
                    };
                    actual_paths.sort();
                    actual_paths.dedup();
                    let mismatch = owner_paths_mismatch(&expected_paths, &actual_paths);
                    ApplyReportPrecondition {
                        id: id.clone(),
                        status: if mismatch {
                            PreconditionStatus::Failed
                        } else {
                            PreconditionStatus::Passed
                        },
                        expected_paths,
                        actual_paths,
                        error: mismatch.then(|| ApplyError {
                            code: "owner-set-mismatch".to_string(),
                            message: format!(
                                "owner-set precondition '{}' did not match the current vault",
                                precondition.id()
                            ),
                            path: None,
                        }),
                    }
                }
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // A protected create and any other create producing the same derived stem
    // can both observe an empty on-disk owner set. Refuse that internally
    // contradictory plan at the same barrier instead of letting operation order
    // manufacture duplicates.
    for (position, precondition) in plan.preconditions.iter().enumerate() {
        let PlanPrecondition::OwnerSet {
            selector:
                OwnerSelector::StemFromOperation {
                    stem_from_operation,
                },
            ..
        } = precondition
        else {
            continue;
        };
        let Some(stem) = operation_stems.get(stem_from_operation) else {
            continue;
        };
        let normalized_stem = stem.to_ascii_lowercase();
        if create_changes_by_stem
            .get(&normalized_stem)
            .is_some_and(|changes| changes.len() > 1)
        {
            let result = &mut results[position];
            result.status = PreconditionStatus::Failed;
            result.error = Some(ApplyError {
                code: "owner-claim-conflict".to_string(),
                message: format!(
                    "owner-set precondition '{}' conflicts with another planned create for stem '{normalized_stem}'",
                    result.id
                ),
                path: None,
            });
        }
    }

    Ok(results)
}

/// Vault-relative paths of every document whose stem matches `stem`, folding
/// ASCII case (so `Foo` matches a `foo` selector). Shared by the `stem` and
/// `stem_from_operation` selectors — they differ only in how the `&str` stem is
/// obtained, so they resolve it and then run this one scan.
fn scan_by_stem(index: &GraphIndex, stem: &str) -> Vec<String> {
    index
        .documents
        .iter()
        .filter(|document| document.stem.eq_ignore_ascii_case(stem))
        .map(|document| document.path.to_string())
        .collect()
}

/// True when the expected and actual owner path-sets differ. Both inputs are
/// already individually sorted+deduped, so an exact element-wise comparison is
/// the identity check the barrier needs: any owner appearing, disappearing, or
/// moving registers as a mismatch.
///
/// Comparison is case-SENSITIVE by design; the `eq_ignore_ascii_case` stem
/// selection above is deliberately NOT mirrored here. ASCII-folding the
/// comparison would let a case-colliding owner (e.g. `Foo.md` alongside `foo.md`
/// on a case-sensitive filesystem) fold+dedup into an expected `foo.md` and
/// silently PASS — a false negative that defeats the whole identity barrier.
/// Exact comparison stays fail-safe instead: a mixed-case author-supplied
/// expected path spuriously refuses (recoverable) rather than letting a real
/// owner change slip through. A filesystem-case-aware policy is tracked as
/// NRN-266.
fn owner_paths_mismatch(expected: &[String], actual: &[String]) -> bool {
    expected != actual
}

/// True when `document`'s frontmatter satisfies every parsed `field:value`
/// predicate, matching `find --eq`'s scalar equality. STRINGS route through
/// `cache::canonical::canonicalize_scalar` — the SAME canonicalizer the query
/// path binds through — so wikilink brackets collapse exactly once and owner-set
/// eq can never drift from `find --eq`'s string handling. NUMBERS compare
/// numerically (`numbers_match`): `find --eq` binds through SQLite's `value = ?`,
/// where INTEGER and REAL compare by value, so `2` matches a stored `2.0`, while
/// an integer beyond f64 precision never rounds into a float. An array field
/// matches if ANY element matches, the same array-awareness the query's
/// `document_fields` rows give `find --eq`.
fn document_matches_eq(document: &Document, predicates: &[(String, serde_json::Value)]) -> bool {
    let Some(frontmatter) = document.frontmatter.as_ref() else {
        return false;
    };
    for (field, expected) in predicates {
        let Some(actual) = frontmatter.get(field) else {
            return false;
        };
        let matches = match actual {
            serde_json::Value::Array(values) => {
                values.iter().any(|value| eq_value_matches(value, expected))
            }
            value => eq_value_matches(value, expected),
        };
        if !matches {
            return false;
        }
    }
    true
}

/// One frontmatter value against one eq predicate value, matching `find --eq`:
/// two numbers compare numerically (see `numbers_match`); everything else
/// compares through `canonicalize_scalar` (wikilink-stripped strings, bools,
/// null, JSON-encoded objects).
fn eq_value_matches(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    if let (serde_json::Value::Number(actual), serde_json::Value::Number(expected)) =
        (actual, expected)
    {
        return numbers_match(actual, expected);
    }
    crate::cache::canonical::canonicalize_scalar(actual)
        == crate::cache::canonical::canonicalize_scalar(expected)
}

/// Numeric equality matching SQLite's INTEGER/REAL comparison, which `find --eq`
/// binds through: equal integers match; an integer matches a float only when the
/// float is finite, integral, and exactly equals the integer (so `2` == `2.0`,
/// but an integer beyond f64 precision never rounds into a float); two floats
/// compare by value.
fn numbers_match(actual: &serde_json::Number, expected: &serde_json::Number) -> bool {
    let integer = |number: &serde_json::Number| {
        number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
    };
    match (integer(actual), integer(expected)) {
        (Some(actual), Some(expected)) => actual == expected,
        (Some(integer), None) => expected.as_f64().is_some_and(|float| {
            float.is_finite() && float.fract() == 0.0 && float as i128 == integer
        }),
        (None, Some(integer)) => actual.as_f64().is_some_and(|float| {
            float.is_finite() && float.fract() == 0.0 && float as i128 == integer
        }),
        (None, None) => actual.as_f64() == expected.as_f64(),
    }
}

/// Build the byte-identical-vault refusal report for a plan whose owner-set
/// barrier failed: every operation is `not_run`, the failed preconditions carry
/// their coded errors, and `outcome = refused` (exit 2). No operation ran, so no
/// path was touched.
///
/// This is the single canonical constructor for the owner-precondition refusal —
/// the donor open-coded the same not-run-ops + refused-outcome shape at the
/// applier's precondition-refusal site; here it is one function the applier
/// calls, so the shape cannot drift.
pub fn build_owner_precondition_refusal_report(
    plan: &MigrationPlan,
    dry_run: bool,
    preconditions: Vec<ApplyReportPrecondition>,
) -> ApplyReport {
    let operations = plan
        .operations
        .iter()
        .enumerate()
        .map(|(index, operation)| ApplyReportOp {
            op_id: index.to_string(),
            kind: operation.kind.clone(),
            status: OpStatus::NotRun,
            from: None,
            path: None,
            stem: None,
            summary: format!("would {} {}", operation.kind, op_display_path(operation)),
            error: None,
            footnote: operation.footnote.clone(),
            cascade: None,
            link_impact: None,
        })
        .collect::<Vec<_>>();
    ApplyReport {
        schema_version: APPLY_REPORT_SCHEMA_VERSION,
        trace_id: String::new(),
        plan_hash: plan.canonical_hash(),
        vault_root: plan.vault_root.clone(),
        dry_run,
        applied: 0,
        skipped: 0,
        failed: 0,
        remaining: operations.len(),
        preconditions,
        operations,
        warnings: Vec::new(),
        outcome: ApplyOutcome::Refused,
        touched_paths: Vec::new(),
    }
}

/// A display path for an operation's summary line: its `path`, else its `src`,
/// else `<unknown>`.
fn op_display_path(op: &MigrationOp) -> String {
    op.fields
        .get("path")
        .or_else(|| op.fields.get("src"))
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use serde_json::json;

    fn doc(path: &str, stem: &str, frontmatter: Option<serde_json::Value>) -> Document {
        Document {
            path: Utf8PathBuf::from(path),
            stem: stem.to_string(),
            hash: "hash".to_string(),
            frontmatter,
            body_text: String::new(),
            headings: vec![],
            block_ids: vec![],
            links: vec![],
            diagnostics: vec![],
            aliases: vec![],
            alias_malformed: vec![],
        }
    }

    fn index(documents: Vec<Document>) -> GraphIndex {
        GraphIndex {
            root: Utf8PathBuf::from("/vault"),
            files: vec![],
            ignored_files: vec![],
            documents,
        }
    }

    fn owner_set_plan(id: &str, selector: serde_json::Value, expected: &[&str]) -> MigrationPlan {
        let plan = json!({
            "schema_version": 2,
            "vault_root": "/vault",
            "preconditions": [{
                "id": id,
                "kind": "owner_set",
                "selector": selector,
                "expected_paths": expected,
            }],
            "operations": [],
        });
        serde_json::from_value(plan).unwrap()
    }

    #[test]
    fn stem_selector_passes_on_exact_owner_set() {
        let idx = index(vec![doc("projects/mimir.md", "mimir", None)]);
        let plan = owner_set_plan("p", json!({"stem": "mimir"}), &["projects/mimir.md"]);
        let out = evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new())
            .expect("structurally valid");
        assert_eq!(out[0].status, PreconditionStatus::Passed);
        assert!(out[0].error.is_none());
    }

    #[test]
    fn stem_selector_folds_ascii_case_in_selection() {
        // A `Mimir` document is selected by a `mimir` stem selector.
        let idx = index(vec![doc("projects/Mimir.md", "Mimir", None)]);
        let plan = owner_set_plan("p", json!({"stem": "mimir"}), &["projects/Mimir.md"]);
        let out =
            evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Passed);
    }

    #[test]
    fn stem_selector_fails_when_owner_moved() {
        let idx = index(vec![doc("archive/mimir.md", "mimir", None)]);
        let plan = owner_set_plan("p", json!({"stem": "mimir"}), &["projects/mimir.md"]);
        let out =
            evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Failed);
        assert_eq!(out[0].error.as_ref().unwrap().code, "owner-set-mismatch");
        assert_eq!(out[0].actual_paths, vec!["archive/mimir.md".to_string()]);
    }

    #[test]
    fn owner_path_comparison_is_case_sensitive() {
        // Selection folds case, but the expected/actual comparison does not: a
        // mixed-case expected path spuriously refuses (fail-safe) rather than
        // folding into a match.
        let idx = index(vec![doc("projects/mimir.md", "mimir", None)]);
        let plan = owner_set_plan("p", json!({"stem": "mimir"}), &["projects/Mimir.md"]);
        let out =
            evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Failed);
    }

    #[test]
    fn eq_selector_matches_conjunctive_frontmatter() {
        let idx = index(vec![
            doc(
                "projects/mimir.md",
                "mimir",
                Some(json!({"type": "project", "key": "MMR"})),
            ),
            doc(
                "projects/other.md",
                "other",
                Some(json!({"type": "project", "key": "OTH"})),
            ),
        ]);
        let plan = owner_set_plan(
            "p",
            json!({"eq": ["type:project", "key:MMR"]}),
            &["projects/mimir.md"],
        );
        let out =
            evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Passed);
    }

    #[test]
    fn eq_selector_matches_number_stored_as_float() {
        let idx = index(vec![doc("tasks/t.md", "t", Some(json!({"priority": 2.0})))]);
        let plan = owner_set_plan("p", json!({"eq": ["priority:2"]}), &["tasks/t.md"]);
        let out =
            evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Passed);
    }

    #[test]
    fn eq_selector_matches_array_field_by_any_element() {
        let idx = index(vec![doc(
            "notes/n.md",
            "n",
            Some(json!({"tags": ["a", "b", "c"]})),
        )]);
        let plan = owner_set_plan("p", json!({"eq": ["tags:b"]}), &["notes/n.md"]);
        let out =
            evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Passed);
    }

    #[test]
    fn stem_from_operation_resolves_named_create_stem() {
        let idx = index(vec![]); // absence: creating a fresh owner
        let plan: MigrationPlan = serde_json::from_value(json!({
            "schema_version": 2,
            "vault_root": "/vault",
            "preconditions": [{
                "id": "absent",
                "kind": "owner_set",
                "selector": {"stem_from_operation": "op1"},
                "expected_paths": [],
            }],
            "operations": [{
                "kind": "create_document",
                "id": "op1",
                "fields": {"path": "tasks/NRN-9.md"},
            }],
        }))
        .unwrap();
        let mut stems = HashMap::new();
        stems.insert("op1".to_string(), "NRN-9".to_string());
        let out = evaluate_owner_preconditions(&plan, &idx, &stems, &HashMap::new()).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Passed);
    }

    #[test]
    fn stem_from_operation_missing_op_is_a_structure_error() {
        let idx = index(vec![]);
        let plan: MigrationPlan = serde_json::from_value(json!({
            "schema_version": 2,
            "vault_root": "/vault",
            "preconditions": [{
                "id": "absent",
                "kind": "owner_set",
                "selector": {"stem_from_operation": "missing"},
                "expected_paths": [],
            }],
            "operations": [],
        }))
        .unwrap();
        let err = evaluate_owner_preconditions(&plan, &idx, &HashMap::new(), &HashMap::new())
            .expect_err("missing operation is a structure fault");
        assert!(err.to_string().contains("missing or non-create operation"));
    }

    #[test]
    fn two_creates_claiming_one_stem_conflict() {
        let idx = index(vec![]);
        let plan: MigrationPlan = serde_json::from_value(json!({
            "schema_version": 2,
            "vault_root": "/vault",
            "preconditions": [{
                "id": "absent",
                "kind": "owner_set",
                "selector": {"stem_from_operation": "op1"},
                "expected_paths": [],
            }],
            "operations": [{"kind": "create_document", "id": "op1", "fields": {}}],
        }))
        .unwrap();
        let mut stems = HashMap::new();
        stems.insert("op1".to_string(), "dupe".to_string());
        let mut by_stem: HashMap<String, BTreeSet<usize>> = HashMap::new();
        by_stem.insert("dupe".to_string(), BTreeSet::from([0usize, 1usize]));
        let out = evaluate_owner_preconditions(&plan, &idx, &stems, &by_stem).unwrap();
        assert_eq!(out[0].status, PreconditionStatus::Failed);
        assert_eq!(out[0].error.as_ref().unwrap().code, "owner-claim-conflict");
    }

    #[test]
    fn duplicate_precondition_id_is_a_structure_error() {
        let plan: MigrationPlan = serde_json::from_value(json!({
            "schema_version": 2,
            "vault_root": "/vault",
            "preconditions": [
                {"id": "dup", "kind": "owner_set", "selector": {"stem": "a"}, "expected_paths": []},
                {"id": "dup", "kind": "owner_set", "selector": {"stem": "b"}, "expected_paths": []},
            ],
            "operations": [],
        }))
        .unwrap();
        let err =
            evaluate_owner_preconditions(&plan, &index(vec![]), &HashMap::new(), &HashMap::new())
                .expect_err("duplicate id");
        assert!(err
            .to_string()
            .contains("duplicate owner-set precondition id"));
    }

    #[test]
    fn empty_stem_selector_is_a_structure_error() {
        let plan = owner_set_plan("p", json!({"stem": ""}), &[]);
        let err =
            evaluate_owner_preconditions(&plan, &index(vec![]), &HashMap::new(), &HashMap::new())
                .expect_err("empty stem");
        assert!(err.to_string().contains("empty stem selector"));
    }

    #[test]
    fn refusal_report_marks_every_op_not_run() {
        let plan: MigrationPlan = serde_json::from_value(json!({
            "schema_version": 2,
            "vault_root": "/vault",
            "operations": [
                {"kind": "move_document", "fields": {"src": "a.md", "dst": "b.md"}},
                {"kind": "create_document", "fields": {"path": "c.md"}},
            ],
        }))
        .unwrap();
        let preconditions = vec![ApplyReportPrecondition {
            id: "p".into(),
            status: PreconditionStatus::Failed,
            expected_paths: vec!["projects/mimir.md".into()],
            actual_paths: vec!["archive/mimir.md".into()],
            error: Some(ApplyError {
                code: "owner-set-mismatch".into(),
                message: "changed".into(),
                path: None,
            }),
        }];
        let report = build_owner_precondition_refusal_report(&plan, false, preconditions);
        assert_eq!(report.outcome, ApplyOutcome::Refused);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.applied, 0);
        assert_eq!(report.remaining, 2);
        assert!(report
            .operations
            .iter()
            .all(|o| o.status == OpStatus::NotRun));
        assert_eq!(report.operations[0].summary, "would move_document a.md");
        assert_eq!(report.operations[1].summary, "would create_document c.md");
        assert_eq!(report.plan_hash, plan.canonical_hash());
    }

    #[test]
    fn numbers_match_aligns_with_find_eq() {
        let num = |v: serde_json::Value| match v {
            serde_json::Value::Number(n) => n,
            _ => unreachable!("test passes only numbers"),
        };
        // `2` matches a stored `2.0` — SQLite INTEGER/REAL numeric equality, the
        // behavior `find --eq` binds through.
        assert!(numbers_match(
            &num(serde_json::json!(2)),
            &num(serde_json::json!(2.0))
        ));
        assert!(numbers_match(
            &num(serde_json::json!(2.0)),
            &num(serde_json::json!(2))
        ));
        assert!(numbers_match(
            &num(serde_json::json!(2)),
            &num(serde_json::json!(2))
        ));
        // Different values never match.
        assert!(!numbers_match(
            &num(serde_json::json!(2)),
            &num(serde_json::json!(3.0))
        ));
        // An integer beyond f64 precision never rounds into a float.
        assert!(!numbers_match(
            &num(serde_json::json!(9007199254740993i64)),
            &num(serde_json::json!(9007199254740992.0))
        ));
        // Two floats compare by value.
        assert!(numbers_match(
            &num(serde_json::json!(1.5)),
            &num(serde_json::json!(1.5))
        ));
        assert!(!numbers_match(
            &num(serde_json::json!(1.5)),
            &num(serde_json::json!(1.6))
        ));
    }
}
