//! CLI→service routing translation for `norn repair --plan` (NRN-231).
//!
//! `repair --plan` is routable byte-identically because the `vault.repair` MCP
//! tool's [`RepairOutput`](crate::mcp::tools::repair::RepairOutput) carries
//! everything the CLI needs to reproduce the direct path: the full
//! `MigrationPlan` — byte-equal to `serde_json::to_value(&plan)`, the exact
//! value `norn repair --plan --format json` serializes — plus the
//! `has_diagnostic_errors` bit the exit code derives from (mirroring
//! `vault.find`'s NRN-222 bit). The client rebuilds a [`MigrationPlan`] from
//! the wire and renders it through the SAME `repair::emit_plan` seam the
//! direct path uses (report / json / paths renderers, `--out` write, exit-code
//! derivation), so routed and direct output are byte-for-byte equal.
//!
//! Unlike `find`/`get`, `--out` is NOT a routed-away rendering knob: the
//! reconstructed `MigrationPlan` is written client-side via the SAME
//! `emit_plan` the direct path calls, so a routed `--out` write is
//! byte-identical to the direct `fs::write` of `serde_json::to_string_pretty`.
//!
//! Only `repair --plan` shapes route. Bare `norn repair` (summary mode) has no
//! wire analogue and is gated to Direct at the routing seam (`lib.rs`), not
//! here.
//!
//! Both functions here are pure so they unit-test without a live daemon; the
//! probe + wire round-trip live in the routing seam (`src/lib.rs`).

use anyhow::Result;
use serde_json::{Map, Value};

use crate::cli::{ConfidenceArg, RepairArgs, ValidateTriageArgs};
use crate::migration_plan::MigrationPlan;
use crate::route_wire::{get_bool, insert_list, json_type};

/// The reconstructed routed repair result: the `MigrationPlan` plus the
/// vault-level diagnostics bit the exit code derives from.
#[derive(Debug)]
pub struct RoutedRepair {
    pub plan: MigrationPlan,
    /// Whether the vault carries any error-severity diagnostic — the
    /// daemon-side `crate::graph::has_errors(&index)`, crossing the wire so
    /// the routed path reproduces direct `repair --plan`'s exit-1 contract.
    pub has_diagnostic_errors: bool,
}

/// Translate parsed `norn repair --plan` args into the `vault.repair` tool's
/// parameter object (the `RepairParams` shape in `src/mcp/tools/repair.rs`).
///
/// `--plan` / `--format` / `--out` are deliberately absent: `--plan` is the
/// CLI-only routing gate (checked before this is called), and `--format` /
/// `--out` are CLI-only rendering / write knobs — the client renders and
/// writes the reconstructed plan itself (see `repair::emit_plan`).
///
/// The `ValidateTriageArgs` destructure is exhaustive so a new triage flag is
/// a compile error here, not a silently-dropped wire field.
pub fn to_mcp_arguments(args: &RepairArgs) -> Value {
    let mut map = Map::new();

    let ValidateTriageArgs {
        code,
        severity,
        field,
        rule,
        path,
        target,
        reason,
    } = &args.triage;
    insert_list(&mut map, "code", code);
    insert_list(&mut map, "severity", severity);
    insert_list(&mut map, "field", field);
    insert_list(&mut map, "rule", rule);
    insert_list(&mut map, "path", path);
    insert_list(&mut map, "target", target);
    insert_list(&mut map, "reason", reason);

    insert_list(&mut map, "skip_reason", &args.skip_reason);

    if let Some(ConfidenceArg::High) = args.confidence {
        map.insert("confidence".into(), Value::String("high".into()));
    }

    Value::Object(map)
}

/// Rebuild a [`RoutedRepair`] from a `vault.repair` `structuredContent` object.
///
/// `plan` deserializes straight into [`MigrationPlan`] (it derives
/// `Deserialize` and the wire value is exactly `serde_json::to_value(&plan)`,
/// the direct path's own serialization). `has_diagnostic_errors` is required,
/// not defaulted (see `find::route::reconstruct`'s identical rationale): the
/// exit code derives from this bit, and guessing it for a pre-field daemon
/// would silently break the routed/direct exit-1 isomorphism — better to fall
/// back to Direct.
pub fn reconstruct(structured: &Value) -> Result<RoutedRepair> {
    let has_diagnostic_errors = get_bool(structured, "vault.repair", "has_diagnostic_errors")?;
    let plan_value = structured.get("plan").ok_or_else(|| {
        anyhow::anyhow!(
            "vault.repair envelope: `plan` must be present, got {}",
            json_type(structured.get("plan"))
        )
    })?;
    let plan: MigrationPlan = serde_json::from_value(plan_value.clone()).map_err(|error| {
        anyhow::anyhow!(
            "vault.repair envelope: `plan` failed to deserialize as MigrationPlan: {error}"
        )
    })?;

    Ok(RoutedRepair {
        plan,
        has_diagnostic_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RepairPlanFormat;
    use camino::Utf8PathBuf;
    use serde_json::json;

    fn base_args() -> RepairArgs {
        RepairArgs {
            plan: true,
            format: None,
            out: None,
            confidence: None,
            skip_reason: vec![],
            triage: ValidateTriageArgs {
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

    #[test]
    fn to_mcp_arguments_omits_defaults() {
        let v = to_mcp_arguments(&base_args());
        assert_eq!(v, json!({}), "an all-default RepairArgs sends nothing");
    }

    #[test]
    fn to_mcp_arguments_maps_triage_filters() {
        let mut args = base_args();
        args.triage.code = vec!["broken-link".into()];
        args.triage.severity = vec!["error".into()];
        args.triage.field = vec!["status".into()];
        args.triage.rule = vec!["allowed-values".into()];
        args.triage.path = vec!["tasks/*".into()];
        args.triage.target = vec!["missing-note".into()];
        args.triage.reason = vec!["target-not-found".into()];

        let v = to_mcp_arguments(&args);
        assert_eq!(v["code"], json!(["broken-link"]));
        assert_eq!(v["severity"], json!(["error"]));
        assert_eq!(v["field"], json!(["status"]));
        assert_eq!(v["rule"], json!(["allowed-values"]));
        assert_eq!(v["path"], json!(["tasks/*"]));
        assert_eq!(v["target"], json!(["missing-note"]));
        assert_eq!(v["reason"], json!(["target-not-found"]));
    }

    #[test]
    fn to_mcp_arguments_maps_confidence_and_skip_reason() {
        let mut args = base_args();
        args.confidence = Some(ConfidenceArg::High);
        args.skip_reason = vec!["low-confidence".into(), "no-match".into()];

        let v = to_mcp_arguments(&args);
        assert_eq!(v["confidence"], "high");
        assert_eq!(v["skip_reason"], json!(["low-confidence", "no-match"]));
    }

    /// `--plan` / `--format` / `--out` are CLI-only knobs; they never ride the
    /// wire regardless of value.
    #[test]
    fn to_mcp_arguments_omits_cli_only_flags() {
        let mut args = base_args();
        args.format = Some(RepairPlanFormat::Json);
        args.out = Some(Utf8PathBuf::from("plan.json"));

        let v = to_mcp_arguments(&args);
        assert_eq!(v, json!({}));
    }

    fn sample_plan() -> MigrationPlan {
        MigrationPlan {
            schema_version: 1,
            vault_root: "/vault".into(),
            generator: None,
            generated_at: None,
            operations: vec![],
            skipped: vec![],
            plan_footnote: None,
        }
    }

    fn wire(plan: &MigrationPlan, has_diagnostic_errors: bool) -> Value {
        json!({
            "plan": serde_json::to_value(plan).unwrap(),
            "has_diagnostic_errors": has_diagnostic_errors,
        })
    }

    #[test]
    fn reconstruct_happy_path() {
        let plan = sample_plan();
        let structured = wire(&plan, false);
        let routed = reconstruct(&structured).unwrap();
        assert_eq!(routed.plan.schema_version, plan.schema_version);
        assert_eq!(routed.plan.vault_root, plan.vault_root);
        assert!(!routed.has_diagnostic_errors);
    }

    #[test]
    fn reconstruct_carries_diagnostics_bit_true() {
        let plan = sample_plan();
        let structured = wire(&plan, true);
        let routed = reconstruct(&structured).unwrap();
        assert!(routed.has_diagnostic_errors);
    }

    #[test]
    fn reconstruct_missing_diagnostics_bit_is_error() {
        let plan = sample_plan();
        let mut structured = wire(&plan, false);
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
        let mut structured = wire(&sample_plan(), false);
        structured.as_object_mut().unwrap().remove("plan");
        let err = reconstruct(&structured).unwrap_err();
        assert!(err.to_string().contains("`plan`"), "got: {err}");
    }

    #[test]
    fn reconstruct_malformed_plan_is_error() {
        let mut structured = wire(&sample_plan(), false);
        // `operations` must be an array; a string is a malformed MigrationPlan.
        structured["plan"]["operations"] = json!("not-an-array");
        let err = reconstruct(&structured).unwrap_err();
        assert!(err.to_string().contains("MigrationPlan"), "got: {err}");
    }
}
