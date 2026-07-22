//! The `validate` verb's execute seam (the 0016 Params/execute/Report vocabulary).
//!
//! Ported from the donor `Command::Validate` arm (ADR 0018). Validate is
//! READ-ONLY: it loads the warm graph, runs the standards [`engine`] over it,
//! triage-filters the findings, and returns them plus the run counts and the
//! pre-rendered `--summary` JSON body — no repair, no mutation. Those live in a
//! separate task on the plan/apply engine.
//!
//! # Error model
//!
//! Like `find`/`count`: a request-rejecting user error (a bad `--code` glob, or
//! a config whose path patterns fail to compile) becomes the inner `Err(String)`
//! arm; only a cache read failure is the outer `Err(_)` (exit-to-heal, ADR 0017).
//!
//! # Exit-code signal
//!
//! `has_errors` is computed over the WHOLE index before triage filtering, so a
//! `--code`/`--severity` narrow never changes the process exit code — it mirrors
//! the donor's `exit_code_for(&index)`, which reads the unfiltered graph.

use anyhow::Result;

use norn_wire::{ValidateParams, ValidateReport};

use crate::cache::Cache;
use crate::graph::{concise_diagnostics, has_errors};
use crate::standards::{
    compile_config, filter_findings, summarize, validate_with_compiled, ValidateFilterOptions,
    VaultConfig,
};

/// Run a `validate` request against the warm cache + retained config. See the
/// module docs for the inner-Err rejection model and the pre-filter exit signal.
pub fn execute(
    cache: &Cache,
    config: Option<&VaultConfig>,
    params: &ValidateParams,
    _today: &str,
) -> Result<Result<ValidateReport, String>> {
    let default_config = VaultConfig::default();
    let config = config.unwrap_or(&default_config);

    // Compile the config's path patterns ONCE per request (the donor's
    // uncompiled fallback re-parsed every rule glob per document — the accidental
    // quadratic this pre-compile avoids). Routes through the single
    // `compile_config` path shared with `parse_config_compiled`.
    let source_path = cache.vault_root().join(".norn/config.yaml");
    let compiled = match compile_config(config, &source_path) {
        Ok(c) => c,
        Err(e) => return Ok(Err(e.to_string())),
    };

    let mut index = cache.load_graph_index()?;

    // Non-verbose (the default) strips graph-diagnostic `detail` to the concise
    // coded form — the donor `trim_diagnostics`. `has_errors` reads severity,
    // which concise preserves, so the exit signal is unaffected by the trim.
    if !params.verbose {
        for document in &mut index.documents {
            document.diagnostics = concise_diagnostics(document);
        }
    }
    let has_errors = has_errors(&index);

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

    // The summary is computed core-side (it folds the engine's richer internal
    // finding, which does not cross the wire) and carried as its final
    // pretty-JSON string so `--format json --summary` is a verbatim passthrough.
    // It is folded ONLY when the request set `--summary`. The findings cross the
    // wire as the typed flat [`norn_wire::Finding`] contract (ADR 0022) — the
    // internal `Finding` projects onto it here, at the producer edge.
    let summary_json = if params.summary {
        Some(serde_json::to_string_pretty(&summarize(&findings))?)
    } else {
        None
    };
    let wire_findings: Vec<norn_wire::Finding> = findings.iter().map(|f| f.to_wire()).collect();

    let rules_count = config.validate.rules.len() + config.validate.required_frontmatter.len();
    let total_docs = index.documents.len();

    Ok(Ok(ValidateReport {
        findings: wire_findings,
        summary_json,
        total_docs,
        rules_count,
        has_errors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use serde_json::Value;
    use tempfile::TempDir;

    const TODAY: &str = "2026-07-19";

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
    fn clean_vault_yields_no_findings_and_exit_ok() {
        let (_t, root) = synth(None);
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n# A\n").unwrap();
        let cache = built(&root);
        let report = execute(&cache, None, &ValidateParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert!(report.findings.is_empty());
        assert!(!report.has_errors);
        assert_eq!(report.total_docs, 1);
    }

    #[test]
    fn required_frontmatter_rule_surfaces_a_finding() {
        let cfg = "validate:\n  required_frontmatter:\n    - title\n";
        let (_t, root) = synth(Some(cfg));
        std::fs::write(root.join("a.md").as_std_path(), "no frontmatter here\n").unwrap();
        let config = crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap();
        let cache = built(&root);
        let report = execute(&cache, Some(&config), &ValidateParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert_eq!(report.findings.len(), 1);
        assert_eq!(
            report.findings[0].code,
            "frontmatter-required-field-missing"
        );
        // rules_count counts required_frontmatter entries + rules.
        assert_eq!(report.rules_count, 1);
    }

    #[test]
    fn code_filter_narrows_findings_but_not_exit() {
        let cfg = "validate:\n  required_frontmatter:\n    - title\n";
        let (_t, root) = synth(Some(cfg));
        std::fs::write(root.join("a.md").as_std_path(), "body\n").unwrap();
        let config = crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap();
        let cache = built(&root);
        let params = ValidateParams {
            codes: vec!["link-target-missing".to_string()],
            ..Default::default()
        };
        let report = execute(&cache, Some(&config), &params, TODAY)
            .unwrap()
            .unwrap();
        // The one finding is a required-field-missing, filtered out by --code.
        assert!(report.findings.is_empty());
    }

    #[test]
    fn bad_code_glob_is_a_user_rejection() {
        let (_t, root) = synth(None);
        std::fs::write(root.join("a.md").as_std_path(), "---\ntitle: A\n---\n").unwrap();
        let cache = built(&root);
        let params = ValidateParams {
            codes: vec!["link-[invalid".to_string()],
            ..Default::default()
        };
        let outcome = execute(&cache, None, &params, TODAY).unwrap();
        assert!(outcome.is_err(), "a bad --code glob rejects");
    }

    #[test]
    fn summary_json_carries_the_grouped_shape_only_when_requested() {
        let cfg = "validate:\n  required_frontmatter:\n    - title\n";
        let (_t, root) = synth(Some(cfg));
        std::fs::write(root.join("a.md").as_std_path(), "body\n").unwrap();
        let config = crate::standards::parse_config(cfg, camino::Utf8Path::new("c.yaml")).unwrap();
        let cache = built(&root);

        // Not requested → the summary fold is skipped entirely.
        let plain = execute(&cache, Some(&config), &ValidateParams::default(), TODAY)
            .unwrap()
            .unwrap();
        assert!(plain.summary_json.is_none());

        // Requested → the grouped shape is present.
        let params = ValidateParams {
            summary: true,
            ..Default::default()
        };
        let report = execute(&cache, Some(&config), &params, TODAY)
            .unwrap()
            .unwrap();
        let summary: Value = serde_json::from_str(report.summary_json.as_ref().unwrap()).unwrap();
        assert_eq!(summary["findings"], 1);
        assert_eq!(summary["codes"]["frontmatter-required-field-missing"], 1);
    }
}
