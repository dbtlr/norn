//! `vault.repair_plan` — produce a deterministic `MigrationPlan` without applying it.
//!
//! The pure handler drives the same pipeline as `norn repair --plan`:
//!
//! 1. Load the `GraphIndex` via `cache_cmd::load_graph_index` (respects
//!    `files.ignore`, incremental refresh) — the same entry point as the CLI.
//! 2. Run `validate_with_compiled` to collect findings.
//! 3. Apply triage filters via `filter_findings`.
//! 4. Call `plan_from_findings` to build the in-memory `MigrationPlan`.
//! 5. Apply `--skip-reason` narrowing to the skipped set.
//! 6. Serialize the plan as `serde_json::Value` into `RepairPlanOutput`.
//!
//! **Read-only guarantee:** this tool calls `repair::run_plan` logic up to the
//! point where it returns the `MigrationPlan` in memory.  It never calls
//! `fs::write`, `repair_apply`, or `run_apply`.  The plan is the output.
//!
//! **Envelope shape:** `MigrationPlan` carries `String`-typed path fields, so
//! it COULD derive `schemars::JsonSchema` — but the `serde_json::Value`
//! wrapper is still correct and future-proof (the schema can evolve without
//! breaking the MCP wire format).  We follow the same pattern as `validate.rs`
//! for consistency: one typed envelope with a `Value` payload.
//!
//! The returned `plan` JSON is byte-for-byte identical to what
//! `norn repair --plan --format json` emits: `serde_json::to_value(&plan)`.
//! `vault.apply_plan` (Task 12) can consume it directly.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cache_cmd::load_graph_index;
use crate::cli::{ConfidenceArg, RepairArgs, ValidateTriageArgs};
use crate::mcp::context::VaultContext;
use crate::planner::findings::plan_from_findings;
use crate::repair::skip_reasons::code_matches_any;
use crate::standards::{validate_with_compiled, ConfidenceFilter, RepairPlanFilters};
use crate::validate_filter::{filter_findings, ValidateFilterOptions};

/// Parameters for `vault.repair_plan`.
///
/// Mirrors the agent-relevant knobs from `norn repair --plan`:
/// triage filters narrow which findings are fed to the planner, and
/// `confidence` lets agents request only high-confidence rewrites.
///
/// All fields are optional; omitting them matches `norn repair --plan`
/// bare defaults (all bands, no triage filters, no skip-reason filter).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct RepairPlanParams {
    /// Filter findings by code before planning. Comma-separated.
    #[serde(default)]
    pub code: Vec<String>,

    /// Filter findings by severity (`warning` or `error`). Comma-separated.
    #[serde(default)]
    pub severity: Vec<String>,

    /// Filter findings by frontmatter field name. Comma-separated.
    #[serde(default)]
    pub field: Vec<String>,

    /// Filter findings by validate rule name. Comma-separated.
    #[serde(default)]
    pub rule: Vec<String>,

    /// Filter findings by vault-relative path glob. Comma-separated.
    #[serde(default)]
    pub path: Vec<String>,

    /// Filter link findings by link target. Comma-separated.
    #[serde(default)]
    pub target: Vec<String>,

    /// Filter link findings by unresolved reason. Comma-separated.
    #[serde(default)]
    pub reason: Vec<String>,

    /// Confidence band filter for closest-match proposals.
    /// `high` keeps only High-confidence rewrites (drops Medium proposals).
    /// Omit to receive all bands (default).
    #[serde(default)]
    pub confidence: Option<ConfidenceBand>,

    /// Filter the skipped-findings list by reason code.
    /// Glob patterns accepted. Comma-separated, repeatable.
    /// Omit to receive all skipped findings.
    #[serde(default)]
    pub skip_reason: Vec<String>,
}

/// Confidence band selector for closest-match rewrite proposals.
#[derive(Debug, Deserialize, schemars::JsonSchema, Clone, Copy)]
pub enum ConfidenceBand {
    /// Include only High-confidence proposals (drop Medium).
    #[serde(rename = "high")]
    High,
}

/// Structured output for `vault.repair_plan`.
///
/// `plan` is the `MigrationPlan` serialized to `serde_json::Value`.  It is
/// structurally identical to the JSON written by `norn repair --plan --out plan.json`
/// and can be fed to `vault.apply_plan` (Task 12) unchanged.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RepairPlanOutput {
    /// The full `MigrationPlan` as a JSON object.  Fields:
    /// `schema_version` (u32), `vault_root` (string), `operations` (array),
    /// `skipped` (array, omitted when empty).
    /// Feed this value to `vault.apply_plan` to execute the rewrites.
    pub plan: serde_json::Value,
}

/// Pure handler for `vault.repair_plan`.
///
/// Mirrors `repair::run_plan` exactly up to the `MigrationPlan` in-memory
/// construction — with NO filesystem writes (no `fs::write`, no apply).
///
/// Uses `cache_cmd::load_graph_index` (not the cache reader's version) so
/// that `files.ignore` patterns are honoured at index-load time, matching the
/// CLI's `norn repair` behaviour.
pub fn handle(ctx: &VaultContext, p: RepairPlanParams) -> Result<RepairPlanOutput> {
    // Load the graph index via the same entry point the CLI uses.
    // `false` means "allow cache refresh if stale" (mirrors `no_cache_refresh = false`).
    // Use the context's current config (`ctx.config()`; hot-swapped in warm mode) —
    // consistent with `validate.rs` and matching `norn repair`'s approach where
    // config is loaded once per invocation.
    let config = ctx.config();
    let index = load_graph_index(&ctx.vault_root, &config.index_options, false)?;

    // Collect all validation findings using the context's current config.
    let findings = validate_with_compiled(
        &index,
        &config.validate,
        &config.compiled,
        config.index_options.alias_field.as_deref(),
    );

    // Build a RepairArgs equivalent from the MCP params for the shared filter helpers.
    // We construct it inline — only the triage and confidence fields are used by
    // `repair_plan_filters` and `filter_findings`; `plan`, `format`, `out` are
    // irrelevant to the in-memory plan path.
    let fake_args = RepairArgs {
        plan: true,
        format: None,
        out: None,
        confidence: p.confidence.map(|c| match c {
            ConfidenceBand::High => ConfidenceArg::High,
        }),
        skip_reason: p.skip_reason.clone(),
        triage: ValidateTriageArgs {
            code: p.code.clone(),
            severity: p.severity.clone(),
            field: p.field.clone(),
            rule: p.rule.clone(),
            path: p.path.clone(),
            target: p.target.clone(),
            reason: p.reason.clone(),
        },
    };

    // Apply triage filters (same as `repair::gather_findings` → `filter_findings`).
    let filter_opts = ValidateFilterOptions {
        codes: &fake_args.triage.code,
        severities: &fake_args.triage.severity,
        fields: &fake_args.triage.field,
        rules: &fake_args.triage.rule,
        paths: &fake_args.triage.path,
        targets: &fake_args.triage.target,
        reasons: &fake_args.triage.reason,
    };
    let filtered_findings = filter_findings(findings, &filter_opts)?;

    // Build the RepairPlanFilters from the triage args (mirrors `repair::repair_plan_filters`).
    let plan_filters = RepairPlanFilters {
        code: normalized_filter_values(&fake_args.triage.code),
        severity: normalized_filter_values(&fake_args.triage.severity),
        field: normalized_filter_values(&fake_args.triage.field),
        rule: normalized_filter_values(&fake_args.triage.rule),
        path: normalized_filter_values(&fake_args.triage.path),
        target: normalized_filter_values(&fake_args.triage.target),
        reason: normalized_filter_values(&fake_args.triage.reason),
        skip_reason: normalized_filter_values(&fake_args.skip_reason),
        confidence: fake_args.confidence.map(|c| match c {
            ConfidenceArg::High => ConfidenceFilter::High,
        }),
    };

    // Build the in-memory MigrationPlan — identical to `repair::run_plan`'s call.
    // `plan_from_findings` is pure: no filesystem side effects.
    let mut plan = plan_from_findings(
        ctx.vault_root.clone(),
        plan_filters,
        filtered_findings,
        &config.repair,
        &index,
    );

    // Apply `skip_reason` narrowing to the skipped set (same as `repair::run_plan`).
    let skip_patterns = normalized_filter_values(&fake_args.skip_reason);
    if !skip_patterns.is_empty() {
        plan.skipped
            .retain(|sf| code_matches_any(&sf.reason, &skip_patterns));
    }

    // Serialize the MigrationPlan to a serde_json::Value.
    // This is byte-equivalent to `serde_json::to_string_pretty(&plan)` — the
    // same bytes `norn repair --plan --format json` writes.
    let plan_value = serde_json::to_value(&plan)?;

    Ok(RepairPlanOutput { plan: plan_value })
}

fn normalized_filter_values(values: &[String]) -> Vec<String> {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use tempfile::TempDir;

    /// Vault with a FIXABLE broken wikilink:
    /// - `target-note.md` exists (stem: `target-note`)
    /// - `source.md` links to `[[target-not]]` — one-char edit on a 10-char string
    ///   → normalized edit distance ratio ≈ 0.9 → closest-match High/Medium proposal.
    ///
    /// The repair planner should produce exactly one `rewrite_link` operation.
    fn vault_with_fixable_link() -> (TempDir, Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-repair-plan-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

        // The target document that exists.
        std::fs::write(
            root.join("target-note.md"),
            "---\ntype: note\ntitle: Target Note\n---\n\nI am the target.\n",
        )
        .unwrap();

        // Source with a near-miss wikilink (missing the 'e' at the end of 'note').
        std::fs::write(
            root.join("source.md"),
            "---\ntype: note\ntitle: Source\n---\n\nSee [[target-not]] for details.\n",
        )
        .unwrap();

        (tmp, root)
    }

    #[test]
    fn handle_fixable_link_returns_plan_with_at_least_one_operation() {
        let (_tmp, root) = vault_with_fixable_link();
        let ctx = VaultContext::open(&root, None).expect("open ctx");

        let out = handle(&ctx, RepairPlanParams::default()).expect("handle should succeed");

        // The plan must be a JSON object (MigrationPlan shape).
        assert!(
            out.plan.is_object(),
            "plan must be a JSON object (MigrationPlan), got: {:?}",
            out.plan
        );

        // Must carry `schema_version`.
        assert_eq!(
            out.plan["schema_version"], 1,
            "plan must have schema_version=1, got: {:?}",
            out.plan["schema_version"]
        );

        // Must carry `vault_root`.
        assert!(
            out.plan["vault_root"].is_string(),
            "plan must carry vault_root string, got: {:?}",
            out.plan["vault_root"]
        );

        // Must carry `operations` array with ≥1 entry (the rewrite_link op).
        let ops = out.plan["operations"]
            .as_array()
            .unwrap_or_else(|| panic!("plan.operations must be an array, got: {:?}", out.plan));

        assert!(
            !ops.is_empty(),
            "expected ≥1 operation for a fixable broken wikilink, got 0;\nplan: {:?}",
            out.plan
        );

        // The operation must be a `rewrite_link` kind.
        let rewrite_op = ops
            .iter()
            .find(|op| op["kind"] == "rewrite_link")
            .unwrap_or_else(|| {
                panic!(
                    "expected a rewrite_link operation, got: {:?}",
                    ops.iter()
                        .map(|op| op["kind"].as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                )
            });

        // The operation's fields must carry expected_old_value (broken target) and new_value (correct stem).
        let fields = &rewrite_op["fields"];
        assert!(
            fields.get("expected_old_value").is_some(),
            "rewrite_link op must carry expected_old_value in fields, got: {fields:?}"
        );
        assert!(
            fields.get("new_value").is_some(),
            "rewrite_link op must carry new_value in fields, got: {fields:?}"
        );

        // Verify the rewrite corrects the broken link.
        assert_eq!(
            fields["expected_old_value"], "target-not",
            "expected_old_value must be the broken link target 'target-not', got: {:?}",
            fields["expected_old_value"]
        );
        assert_eq!(
            fields["new_value"], "target-note",
            "new_value must be the correct stem 'target-note', got: {:?}",
            fields["new_value"]
        );
    }

    #[test]
    fn handle_clean_vault_returns_empty_operations() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-repair-plan-clean-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("clean.md"),
            "---\ntype: note\ntitle: Clean\n---\n\nNo broken links here.\n",
        )
        .unwrap();

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        let out = handle(&ctx, RepairPlanParams::default()).expect("handle should succeed");

        assert!(
            out.plan.is_object(),
            "plan must be a JSON object even when empty"
        );
        let ops = out.plan["operations"]
            .as_array()
            .unwrap_or_else(|| panic!("plan.operations must be an array"));
        assert_eq!(
            ops.len(),
            0,
            "clean vault must produce 0 operations, got: {ops:?}"
        );
    }

    #[test]
    fn handle_returns_read_only_plan_not_applied() {
        // Verify the tool is read-only: the source file must be unchanged after
        // handle() runs.  If anything were applied the wikilink text would change.
        let (_tmp, root) = vault_with_fixable_link();
        let source_before =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md before");

        let ctx = VaultContext::open(&root, None).expect("open ctx");
        handle(&ctx, RepairPlanParams::default()).expect("handle should succeed");

        let source_after =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md after");
        assert_eq!(
            source_before, source_after,
            "source.md must be unmodified after repair_plan (read-only tool)"
        );
    }
}
