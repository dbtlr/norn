//! The `repair` verb's execute seam (the 0016 Params/execute/Report vocabulary).
//!
//! Ported from the donor `repair::{filtered_findings, build_plan}` +
//! `mcp/tools/repair.rs` (ADR 0018). Repair is READ-ONLY: it loads the warm
//! graph, runs the standards engine, triage-filters the findings, and turns them
//! into a deterministic [`MigrationPlan`](norn_wire::MigrationPlan) via
//! [`plan_from_findings`](crate::planner::findings::plan_from_findings) — it never
//! writes. Emitting the plan is the whole job; `apply` executes it (the donor's
//! `norn repair --plan | norn apply -` two-step).
//!
//! # Error model
//!
//! Like `validate`: a request-rejecting user error (a bad `--code` glob, or a
//! config whose path patterns fail to compile) becomes the inner `Err(String)`
//! arm; only a cache read failure is the outer `Err(_)` (exit-to-heal, ADR 0017).
//!
//! # Exit-code signal
//!
//! `has_diagnostic_errors` is computed over the WHOLE index before triage
//! filtering, so a `--code`/`--severity` narrow never changes the process exit
//! code — it mirrors the donor's `exit_code_for(&index)` / `has_errors(&index)`,
//! which reads the unfiltered graph.

use std::collections::BTreeMap;

use anyhow::Result;

use norn_wire::{RepairParams, RepairReport, RepairSkipDetail};

use crate::cache::Cache;
use crate::graph::{concise_diagnostics, has_errors};
use crate::planner::findings::plan_from_findings;
use crate::standards::{
    compile_config, filter_findings, validate_with_compiled, ConfidenceFilter, RepairPlanFilters,
    ValidateFilterOptions, VaultConfig,
};

/// Run a `repair` request against the warm cache + retained config: build the
/// findings-derived `MigrationPlan` (never applying it) and the bare-summary
/// tally. See the module docs for the inner-`Err` rejection model and the
/// pre-filter exit signal.
pub fn execute(
    cache: &Cache,
    config: Option<&VaultConfig>,
    params: &RepairParams,
    _today: &str,
) -> Result<Result<RepairReport, String>> {
    let default_config = VaultConfig::default();
    let config = config.unwrap_or(&default_config);

    // Compile the config's path patterns once per request (shared with
    // `validate`'s execute seam) — the accidental-quadratic guard.
    let source_path = cache.vault_root().join(".norn/config.yaml");
    let compiled = match compile_config(config, &source_path) {
        Ok(c) => c,
        Err(e) => return Ok(Err(e.to_string())),
    };

    let mut index = cache.load_graph_index()?;

    // Non-verbose (the default) strips graph-diagnostic `detail` to the concise
    // coded form (the donor `trim_diagnostics`). `has_errors` reads severity,
    // which concise preserves, so the exit signal is unaffected by the trim.
    if !params.verbose {
        for document in &mut index.documents {
            document.diagnostics = concise_diagnostics(document);
        }
    }
    let has_diagnostic_errors = has_errors(&index);

    let alias_field = cache.alias_field();
    let findings = validate_with_compiled(&index, &config.validate, &compiled, alias_field);

    let options = ValidateFilterOptions {
        codes: &params.codes,
        severities: &params.severities,
        fields: &params.fields,
        rules: &params.rules,
        paths: &params.paths,
        targets: &params.targets,
        reasons: &params.reasons,
    };
    let findings = match filter_findings(findings, &options) {
        Ok(f) => f,
        Err(e) => return Ok(Err(e.to_string())),
    };

    // Bare-summary tally: total findings + per-code counts (sorted, a BTreeMap in
    // code order — the donor `by_code`). Owned keys so `findings` can move into
    // the planner below.
    let findings_total = findings.len();
    let mut by_code: BTreeMap<String, usize> = BTreeMap::new();
    for finding in &findings {
        *by_code.entry(finding.code.clone()).or_insert(0) += 1;
    }
    let findings_by_code: Vec<(String, usize)> = by_code.into_iter().collect();
    let total_docs = index.documents.len();

    // Build the plan. Only `--confidence high` reaches the plan generator (it
    // drops Medium closest-match proposals); the triage narrowing already ran on
    // the findings above (the donor keeps confidence on the plan filters and the
    // triage on the finding filter).
    let filters = RepairPlanFilters {
        confidence: if params.confidence_high {
            Some(ConfidenceFilter::High)
        } else {
            None
        },
        ..Default::default()
    };
    let result = plan_from_findings(
        cache.vault_root().to_path_buf(),
        filters,
        findings,
        &config.repair,
        &index,
    );
    let mut plan = result.plan;

    // The rich skip detail (candidates / next actions / reason code) rides the
    // REPORT, sibling to the lean plan `skipped` (ADR 0024). It mirrors
    // `plan.skipped` one-for-one, so the same `--skip-reason` narrowing applies to
    // both, keeping them in lockstep.
    let mut skipped_detail: Vec<RepairSkipDetail> = result
        .skipped
        .iter()
        .map(|sf| RepairSkipDetail {
            finding_code: sf.code.clone(),
            path: sf.path.to_string(),
            reason_code: sf.skip_reason.code().to_string(),
            candidates: sf.candidates.iter().map(|p| p.to_string()).collect(),
            next_actions: sf.next_actions.clone(),
        })
        .collect();

    // `--skip-reason` narrows the plan's skipped list only (the planner does not
    // apply it) — the `MigrationPlan::skipped` `reason` carries the kebab-case
    // reason code (donor `build_plan`).
    if !params.skip_reasons.is_empty() {
        plan.skipped
            .retain(|sf| skip_reason_matches(&sf.reason, &params.skip_reasons));
        skipped_detail.retain(|d| skip_reason_matches(&d.reason_code, &params.skip_reasons));
    }

    Ok(Ok(RepairReport {
        plan,
        skipped_detail,
        findings_by_code,
        findings_total,
        total_docs,
        has_diagnostic_errors,
    }))
}

/// True if `code` matches any of the supplied globs (donor
/// `repair::skip_reasons::code_matches_any`). A malformed glob falls back to an
/// exact-string compare. Empty pattern list is handled by the caller (no filter).
fn skip_reason_matches(code: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| match globset::Glob::new(pattern) {
            Ok(g) => g.compile_matcher().is_match(code),
            Err(_) => code == pattern,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-20";

    fn synth(config: Option<&str>) -> (TempDir, Utf8PathBuf) {
        let tmp = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .unwrap()
            .join("vault");
        std::fs::create_dir(root.as_std_path()).unwrap();
        if let Some(cfg) = config {
            std::fs::create_dir(root.join(".norn").as_std_path()).unwrap();
            std::fs::write(root.join(".norn/config.yaml").as_std_path(), cfg).unwrap();
        }
        (tmp, root)
    }

    fn built(root: &Utf8PathBuf) -> Cache {
        let mut cache = Cache::open(root).unwrap();
        cache.full_build(root).unwrap();
        cache
    }

    #[test]
    fn clean_vault_yields_empty_plan_and_no_findings() {
        let (_t, root) = synth(None);
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n# A\n").unwrap();
        let cache = built(&root);
        let report = execute(&cache, None, &RepairParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report.findings_total, 0);
        assert!(report.findings_by_code.is_empty());
        assert!(!report.has_diagnostic_errors);
        assert_eq!(report.plan.operations.len(), 0);
    }

    #[test]
    fn required_frontmatter_finding_lands_in_the_tally() {
        let cfg = "validate:\n  required_frontmatter:\n    - title\n";
        let (_t, root) = synth(Some(cfg));
        std::fs::write(root.join("a.md").as_std_path(), "no frontmatter here\n").unwrap();
        let config = crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap();
        let cache = built(&root);
        let report = execute(&cache, Some(&config), &RepairParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report.findings_total, 1);
        assert_eq!(
            report.findings_by_code,
            vec![("frontmatter-required-field-missing".to_string(), 1)]
        );
        assert_eq!(report.total_docs, 1);
    }

    #[test]
    fn code_filter_narrows_findings_but_not_exit_signal() {
        let cfg = "validate:\n  required_frontmatter:\n    - title\n";
        let (_t, root) = synth(Some(cfg));
        std::fs::write(root.join("a.md").as_std_path(), "body\n").unwrap();
        let config = crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap();
        let cache = built(&root);
        let params = RepairParams {
            codes: vec!["link-target-missing".to_string()],
            ..Default::default()
        };
        let report = execute(&cache, Some(&config), &params, TODAY)
            .unwrap()
            .unwrap();
        // The one required-field-missing finding is filtered out by --code.
        assert_eq!(report.findings_total, 0);
    }

    #[test]
    fn bad_code_glob_is_a_user_rejection() {
        let (_t, root) = synth(None);
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let cache = built(&root);
        let params = RepairParams {
            codes: vec!["link-[invalid".to_string()],
            ..Default::default()
        };
        let outcome = execute(&cache, None, &params, TODAY).unwrap();
        assert!(outcome.is_err(), "a bad --code glob rejects");
    }

    #[test]
    fn plan_is_typed_and_carries_vault_root() {
        let (_t, root) = synth(None);
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let cache = built(&root);
        let report = execute(&cache, None, &RepairParams::default(), TODAY)
            .unwrap()
            .unwrap();
        // The typed plan carries the vault root, and still pretty-prints multi-line.
        assert_eq!(report.plan.vault_root, root.as_str());
        let plan_json = serde_json::to_string_pretty(&report.plan).unwrap();
        assert!(plan_json.contains('\n'));
    }
}
