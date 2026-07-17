//! `vault.repair` — produce a deterministic `MigrationPlan` without applying it.
//!
//! `RepairParams` is the surface-neutral request vocabulary and
//! `<RepairParams as Request>::execute` is the SINGLE implementation (NRN-291):
//! the `#[tool]` wrapper, the warm daemon, and the cold CLI dispatch path all
//! run this one body. It drives the same pipeline as `norn repair --plan`:
//!
//! 1. Load the `GraphIndex` via `VaultEnv::load_graph_index` (warm-connection
//!    reuse under the daemon, fresh open in cold mode; incremental refresh either
//!    way). `files.ignore` is enforced at cache-build time, so the index arrives
//!    already filtered — same result as the CLI's load path.
//! 2. Filter findings via the shared `repair::filtered_findings`
//!    (`validate_with_compiled` → `filter_findings`).
//! 3. Build the in-memory `MigrationPlan` via the shared `repair::build_plan`
//!    (`plan_from_findings` → `--skip-reason` narrowing).
//! 4. Serialize the plan as `serde_json::Value` into `RepairOutput`.
//!
//! **Read-only guarantee:** this tool builds the `MigrationPlan` in memory and
//! never calls `fs::write`, `repair_apply`, or `run_apply`. The plan is the
//! output.
//!
//! **Envelope shape:** `MigrationPlan` carries `String`-typed path fields, so
//! it COULD derive `schemars::JsonSchema` — but the `serde_json::Value`
//! wrapper is still correct and future-proof (the schema can evolve without
//! breaking the MCP wire format).  We follow the same pattern as `validate.rs`
//! for consistency: one typed envelope with a `Value` payload.
//!
//! The returned `plan` JSON is byte-for-byte identical to what
//! `norn repair --plan --format json` emits: `serde_json::to_value(&plan)`.
//! `vault.apply` (Task 12) can consume it directly.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::{ConfidenceArg, RepairArgs};
use crate::env::{RequestScope, VaultEnv};
use crate::standards::{ConfidenceFilter, RepairPlanFilters};
use crate::validate_filter::ValidateFilterOptions;

/// Parameters for `vault.repair`.
///
/// Mirrors the agent-relevant knobs from `norn repair --plan`:
/// triage filters narrow which findings are fed to the planner, and
/// `confidence` lets agents request only high-confidence rewrites.
///
/// All fields are optional; omitting them matches `norn repair --plan`
/// bare defaults (all bands, no triage filters, no skip-reason filter).
///
/// This is also the surface-neutral REQUEST vocabulary (NRN-291): the CLI's
/// `RepairArgs` convert INTO it via [`RepairParams::from_args`], `Serialize`
/// carries it over the daemon socket, and `Deserialize` receives it MCP-side —
/// one params type, three surfaces. CLI-only knobs (`--plan`, `--format`,
/// `--out`) are deliberately absent: they never cross the wire and are handled
/// entirely adapter-side (see `from_args`).
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Default)]
pub struct RepairParams {
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
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone, Copy)]
pub enum ConfidenceBand {
    /// Include only High-confidence proposals (drop Medium).
    #[serde(rename = "high")]
    High,
}

/// Structured output for `vault.repair`.
///
/// `plan` is the `MigrationPlan` serialized to `serde_json::Value`.  It is
/// structurally identical to the JSON written by `norn repair --plan --out plan.json`
/// and can be fed to `vault.apply` (Task 12) unchanged.
///
/// `Deserialize` is implemented so the routed CLI client can rebuild this exact
/// report from the daemon's `structuredContent` envelope (NRN-291): the wire
/// value IS a serialized `RepairOutput`, so reconstruction is plain serde.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepairOutput {
    /// The full `MigrationPlan` as a JSON object.  Fields:
    /// `schema_version` (u32), `vault_root` (string), `operations` (array),
    /// `skipped` (array, omitted when empty).
    /// Feed this value to `vault.apply` to execute the rewrites.
    pub plan: serde_json::Value,

    /// Whether the vault carries any error-severity diagnostic — the SAME
    /// predicate `norn repair --plan`'s exit code derives from
    /// (`crate::graph::has_errors(&index)`, checked over the FULL loaded
    /// index, independent of the triage filters applied to `plan`). Carried
    /// so a routed `repair --plan` reproduces the direct path's exit code
    /// (NRN-231, mirroring `vault.find`'s `has_diagnostic_errors` bit from
    /// NRN-222).
    pub has_diagnostic_errors: bool,
}

/// The `#[tool]` wrapper's entry point for `vault.repair`, delegating to the
/// single implementation [`<RepairParams as Request>::execute`](crate::dispatch::Request::execute).
/// Kept as a free `fn` so the rmcp wrapper in `server.rs` and the test shim call
/// it by name; all logic lives in `execute`.
pub fn handle(ctx: &VaultEnv, scope: &RequestScope, p: RepairParams) -> Result<RepairOutput> {
    // The `#[tool]` wrapper's entry point. The single implementation lives in
    // `<RepairParams as Request>::execute` (NRN-291), which the cold CLI
    // dispatch path also calls — so the daemon, the routed client's local
    // fall-back, and the stdio `norn mcp` server all run one body.
    crate::dispatch::Request::execute(&p, ctx, scope)
}

impl crate::dispatch::Request for RepairParams {
    const TOOL: &'static str = crate::mcp::server::tool_names::REPAIR;
    type Report = RepairOutput;

    /// The single `vault.repair` implementation. Mirrors `repair::run_plan`
    /// exactly up to the in-memory `MigrationPlan` — with NO filesystem writes
    /// (no `fs::write`, no apply). Loads the graph index via
    /// `VaultEnv::load_graph_index` (warm-connection reuse under the daemon,
    /// fresh open in cold mode; `files.ignore` enforced at cache-build time so
    /// the index arrives already filtered — NRN-130), then runs the shared
    /// finding→plan orchestration keyed on these params.
    fn execute(&self, ctx: &VaultEnv, scope: &RequestScope) -> Result<RepairOutput> {
        // Use the request's bound config (`scope.config()`; hot-swapped in warm
        // mode, bound at the request boundary — NRN-253).
        let config = scope.config();
        let index = ctx.load_graph_index(scope)?;

        // The CLI's exit-code signal (any error-severity diagnostic anywhere in
        // the vault), computed off the FULL index BEFORE triage filtering — same
        // as `repair::run_plan`'s `crate::exit_code_for(&index)`. Carried so a
        // routed `repair --plan` reproduces the direct exit code (NRN-231).
        let has_diagnostic_errors = crate::graph::has_errors(&index);

        // Findings → plan run through the ONE surface-neutral orchestration in
        // `repair::mod` (NRN-291) — the SAME functions the CLI-local summary path
        // calls, keyed on this request's `RepairParams`.
        let findings = crate::repair::filtered_findings(self, &index, &config)?;
        let plan =
            crate::repair::build_plan(self, ctx.vault_root.clone(), findings, &config, &index);

        // Serialize the MigrationPlan to a serde_json::Value. Byte-equivalent to
        // `serde_json::to_string_pretty(&plan)` — the same bytes
        // `norn repair --plan --format json` writes.
        let plan_value = serde_json::to_value(&plan)?;

        Ok(RepairOutput {
            plan: plan_value,
            has_diagnostic_errors,
        })
    }

    /// Rebuild [`RepairOutput`] from a `vault.repair` `structuredContent` object.
    ///
    /// The wire value IS a serialized `RepairOutput`, so reconstruction is plain
    /// serde. The plan is additionally validated to deserialize as a
    /// [`MigrationPlan`](crate::migration_plan::MigrationPlan) — a malformed or
    /// pre-field envelope fails here and falls back to a verified direct run,
    /// preserving the pre-NRN-291 `repair::route::reconstruct` contract (the
    /// exit code and rendered plan both derive from these fields, so guessing
    /// them would silently break the routed↔direct isomorphism).
    fn reconstruct(structured: &serde_json::Value) -> Result<RepairOutput> {
        let output: RepairOutput = serde_json::from_value(structured.clone()).map_err(|error| {
            anyhow::anyhow!("vault.repair envelope failed to deserialize: {error}")
        })?;
        serde_json::from_value::<crate::migration_plan::MigrationPlan>(output.plan.clone())
            .map_err(|error| {
                anyhow::anyhow!(
                    "vault.repair envelope: `plan` failed to deserialize as MigrationPlan: {error}"
                )
            })?;
        Ok(output)
    }
}

impl RepairParams {
    /// Build the request vocabulary from parsed `norn repair` CLI args (NRN-291).
    ///
    /// Adapter direction is ALWAYS args → params, never params → fake args. The
    /// CLI-only knobs (`--plan`, `--format`, `--out`) have no `RepairParams`
    /// field, so their exclusion from the wire lives here by construction. The
    /// `ValidateTriageArgs` destructure is exhaustive so a new triage flag is a
    /// compile error here, not a silently-dropped field.
    pub(crate) fn from_args(args: &RepairArgs) -> Self {
        let crate::cli::ValidateTriageArgs {
            code,
            severity,
            field,
            rule,
            path,
            target,
            reason,
        } = &args.triage;
        Self {
            code: code.clone(),
            severity: severity.clone(),
            field: field.clone(),
            rule: rule.clone(),
            path: path.clone(),
            target: target.clone(),
            reason: reason.clone(),
            confidence: args.confidence.map(|c| match c {
                ConfidenceArg::High => ConfidenceBand::High,
            }),
            skip_reason: args.skip_reason.clone(),
        }
    }

    /// The triage-filter view over these params, borrowed for `filter_findings`.
    pub(crate) fn validate_filter_options(&self) -> ValidateFilterOptions<'_> {
        ValidateFilterOptions {
            codes: &self.code,
            severities: &self.severity,
            fields: &self.field,
            rules: &self.rule,
            paths: &self.path,
            targets: &self.target,
            reasons: &self.reason,
        }
    }

    /// The planner's `RepairPlanFilters` derived from these params.
    pub(crate) fn plan_filters(&self) -> RepairPlanFilters {
        RepairPlanFilters {
            code: normalized_filter_values(&self.code),
            severity: normalized_filter_values(&self.severity),
            field: normalized_filter_values(&self.field),
            rule: normalized_filter_values(&self.rule),
            path: normalized_filter_values(&self.path),
            target: normalized_filter_values(&self.target),
            reason: normalized_filter_values(&self.reason),
            skip_reason: normalized_filter_values(&self.skip_reason),
            confidence: self.confidence.map(|c| match c {
                ConfidenceBand::High => ConfidenceFilter::High,
            }),
        }
    }

    /// Normalized `--skip-reason` patterns (comma-split, trimmed, non-empty).
    pub(crate) fn skip_patterns(&self) -> Vec<String> {
        normalized_filter_values(&self.skip_reason)
    }
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

    crate::mcp::tools::scoped_shim! {
        fn handle(RepairParams) -> RepairOutput;
    }
    use camino::Utf8PathBuf;
    use serde_json::json;
    use tempfile::TempDir;

    // ---- Request seam: args → params conversion + wire round-trip (NRN-291) ----
    // Migrated from the deleted `repair::route` unit tests: the coverage those
    // held (args→wire mapping, CLI-only-flag exclusion, envelope reconstruction)
    // now lives on `RepairParams::from_args` + serde + `Request::reconstruct`.

    fn base_args() -> RepairArgs {
        RepairArgs {
            plan: true,
            format: None,
            out: None,
            confidence: None,
            skip_reason: vec![],
            triage: crate::cli::ValidateTriageArgs {
                code: vec![],
                severity: vec![],
                field: vec![],
                rule: vec![],
                path: vec![],
                target: vec![],
                reason: vec![],
            },
        }
    }

    /// The wire arguments a routed `repair` sends: the params serialized to JSON.
    fn wire_arguments(args: &RepairArgs) -> serde_json::Value {
        serde_json::to_value(RepairParams::from_args(args)).unwrap()
    }

    #[test]
    fn from_args_maps_triage_filters() {
        let mut args = base_args();
        args.triage.code = vec!["broken-link".into()];
        args.triage.severity = vec!["error".into()];
        args.triage.field = vec!["status".into()];
        args.triage.rule = vec!["allowed-values".into()];
        args.triage.path = vec!["tasks/*".into()];
        args.triage.target = vec!["missing-note".into()];
        args.triage.reason = vec!["target-not-found".into()];

        let v = wire_arguments(&args);
        assert_eq!(v["code"], json!(["broken-link"]));
        assert_eq!(v["severity"], json!(["error"]));
        assert_eq!(v["field"], json!(["status"]));
        assert_eq!(v["rule"], json!(["allowed-values"]));
        assert_eq!(v["path"], json!(["tasks/*"]));
        assert_eq!(v["target"], json!(["missing-note"]));
        assert_eq!(v["reason"], json!(["target-not-found"]));
    }

    #[test]
    fn from_args_maps_confidence_and_skip_reason() {
        let mut args = base_args();
        args.confidence = Some(ConfidenceArg::High);
        args.skip_reason = vec!["low-confidence".into(), "no-match".into()];

        let v = wire_arguments(&args);
        assert_eq!(v["confidence"], "high");
        assert_eq!(v["skip_reason"], json!(["low-confidence", "no-match"]));
    }

    /// `--plan` / `--format` / `--out` are CLI-only knobs with no `RepairParams`
    /// field, so they can never ride the wire regardless of value — the exclusion
    /// is structural, not a hand-maintained drop list.
    #[test]
    fn wire_omits_cli_only_flags() {
        let mut args = base_args();
        args.format = Some(crate::cli::RepairPlanFormat::Json);
        args.out = Some(Utf8PathBuf::from("plan.json"));

        let v = wire_arguments(&args);
        let obj = v.as_object().expect("params serialize to an object");
        assert!(!obj.contains_key("plan"), "wire must not carry `plan`");
        assert!(!obj.contains_key("format"), "wire must not carry `format`");
        assert!(!obj.contains_key("out"), "wire must not carry `out`");
    }

    fn sample_plan_value() -> serde_json::Value {
        let plan = crate::migration_plan::MigrationPlan {
            schema_version: crate::migration_plan::MIGRATION_PLAN_SCHEMA_VERSION,
            vault_root: "/vault".into(),
            generator: None,
            generated_at: None,
            preconditions: Vec::new(),
            operations: vec![],
            skipped: vec![],
            plan_footnote: None,
        };
        serde_json::to_value(&plan).unwrap()
    }

    fn wire_envelope(has_diagnostic_errors: bool) -> serde_json::Value {
        json!({
            "plan": sample_plan_value(),
            "has_diagnostic_errors": has_diagnostic_errors,
        })
    }

    fn reconstruct(structured: &serde_json::Value) -> Result<RepairOutput> {
        <RepairParams as crate::dispatch::Request>::reconstruct(structured)
    }

    #[test]
    fn reconstruct_happy_path() {
        let out = reconstruct(&wire_envelope(false)).unwrap();
        assert_eq!(out.plan["schema_version"], 2);
        assert_eq!(out.plan["vault_root"], "/vault");
        assert!(!out.has_diagnostic_errors);
    }

    #[test]
    fn reconstruct_carries_diagnostics_bit_true() {
        let out = reconstruct(&wire_envelope(true)).unwrap();
        assert!(out.has_diagnostic_errors);
    }

    #[test]
    fn reconstruct_missing_diagnostics_bit_is_error() {
        let mut structured = wire_envelope(false);
        structured
            .as_object_mut()
            .unwrap()
            .remove("has_diagnostic_errors");
        let err = reconstruct(&structured).unwrap_err();
        assert!(
            err.to_string().contains("has_diagnostic_errors"),
            "got: {err}"
        );
    }

    #[test]
    fn reconstruct_missing_plan_is_error() {
        let mut structured = wire_envelope(false);
        structured.as_object_mut().unwrap().remove("plan");
        let err = reconstruct(&structured).unwrap_err();
        assert!(err.to_string().contains("plan"), "got: {err}");
    }

    #[test]
    fn reconstruct_malformed_plan_is_error() {
        let mut structured = wire_envelope(false);
        // `operations` must be an array; a string is a malformed MigrationPlan.
        structured["plan"]["operations"] = json!("not-an-array");
        let err = reconstruct(&structured).unwrap_err();
        assert!(err.to_string().contains("MigrationPlan"), "got: {err}");
    }

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
        let ctx = VaultEnv::open(&root, None).expect("open ctx");

        let out = handle(&ctx, RepairParams::default()).expect("handle should succeed");

        // The plan must be a JSON object (MigrationPlan shape).
        assert!(
            out.plan.is_object(),
            "plan must be a JSON object (MigrationPlan), got: {:?}",
            out.plan
        );

        // Must carry `schema_version`.
        assert_eq!(
            out.plan["schema_version"], 2,
            "plan must have schema_version=2, got: {:?}",
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

        let ctx = VaultEnv::open(&root, None).expect("open ctx");
        let out = handle(&ctx, RepairParams::default()).expect("handle should succeed");

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
        assert!(
            !out.has_diagnostic_errors,
            "a clean vault must not carry an error-severity diagnostic"
        );
    }

    /// NRN-231: `has_diagnostic_errors` mirrors `norn repair --plan`'s exit-code
    /// signal — true whenever ANY document in the vault carries an
    /// error-severity diagnostic, independent of the triage filters applied to
    /// the plan itself (an unreadable file need not have any repairable finding
    /// to still flip this bit).
    #[test]
    fn handle_diagnostic_error_vault_sets_bit() {
        let tmp = tempfile::Builder::new()
            .prefix("norn-mcp-repair-plan-diag-")
            .tempdir()
            .unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        std::fs::write(
            root.join("good.md"),
            "---\ntype: note\ntitle: Good\n---\nbody\n",
        )
        .unwrap();
        // Invalid UTF-8 with a .md extension trips read_to_string, surfaced as a
        // Severity::Error diagnostic (code "read-failed") — same fixture shape as
        // `find::route`'s exit-code isomorphism test.
        std::fs::write(
            root.join("bad-utf8.md").as_std_path(),
            b"\xff\xfe\xfd\xfc invalid utf-8 here",
        )
        .unwrap();

        let ctx = VaultEnv::open(&root, None).expect("open ctx");
        let out = handle(&ctx, RepairParams::default()).expect("handle should succeed");
        assert!(
            out.has_diagnostic_errors,
            "a vault with an error-severity diagnostic must set the bit"
        );
    }

    #[test]
    fn handle_returns_read_only_plan_not_applied() {
        // Verify the tool is read-only: the source file must be unchanged after
        // handle() runs.  If anything were applied the wikilink text would change.
        let (_tmp, root) = vault_with_fixable_link();
        let source_before =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md before");

        let ctx = VaultEnv::open(&root, None).expect("open ctx");
        handle(&ctx, RepairParams::default()).expect("handle should succeed");

        let source_after =
            std::fs::read_to_string(root.join("source.md")).expect("read source.md after");
        assert_eq!(
            source_before, source_after,
            "source.md must be unmodified after repair (read-only tool)"
        );
    }
}
