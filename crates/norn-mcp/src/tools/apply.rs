//! `vault.apply` — execute an already-reviewed `MigrationPlan`.
//!
//! Unlike the other mutators, `apply` does not synthesize its plan from a few
//! arguments: the client supplies the whole plan (typically one returned by
//! `vault.repair`). The handler parses it into a typed [`MigrationPlan`] and
//! validates its `schema_version` BEFORE the wire — a malformed plan or a schema
//! mismatch is a STRUCTURED rejection (a well-formed tool call with bad
//! contents), never reaching the owner. The parsed plan then crosses TYPED, so a
//! repair→apply composition round-trips (ADR 0011: the plan bytes reviewed are
//! the plan bytes applied). The wire [`ApplyReport`] is wrapped in the
//! `{ report: … }` envelope with `isError` derived from the report's outcome.

use norn_wire::{ApplyParams as WireApplyParams, ApplyReport, MigrationPlan};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.apply`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ApplyParams {
    /// The `MigrationPlan` to execute, as a JSON object — exactly the `plan`
    /// returned by `vault.repair` (or a hand-authored plan). Its `schema_version`
    /// is validated before the wire; a malformed or wrong-version plan is
    /// rejected without touching the vault.
    pub plan: serde_json::Value,
    /// Apply the plan. **Defaults to `false` (dry-run): the call returns the
    /// planned execution with `dry_run = true` and writes nothing.** Pass `true`
    /// to write.
    #[serde(default)]
    pub confirm: bool,
    /// Auto-create missing parent directories for `create_document` ops
    /// (`--parents`).
    #[serde(default)]
    pub parents: bool,
}

/// Structured output for `vault.apply` — the shared apply report wrapped as a
/// generic `Value` under `report`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ApplyOutput {
    /// The full apply report: per-op status, preconditions, cascade summaries,
    /// outcome, and any coded refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params, parsing + schema-validating the
/// plan CLIENT-SIDE (before the wire). Returns `Err(message)` — a structured
/// rejection — for a plan that is not a valid `MigrationPlan` or carries a
/// version other than the current schema.
pub(crate) fn to_wire(p: ApplyParams) -> Result<WireApplyParams, String> {
    let plan: MigrationPlan = serde_json::from_value(p.plan)
        .map_err(|e| format!("plan is not a valid MigrationPlan: {e}"))?;
    if plan.schema_version != norn_wire::MIGRATION_PLAN_SCHEMA_VERSION {
        return Err(format!(
            "plan schema_version {} is not the supported version {}",
            plan.schema_version,
            norn_wire::MIGRATION_PLAN_SCHEMA_VERSION
        ));
    }
    Ok(WireApplyParams {
        plan,
        confirm: p.confirm,
        parents: p.parents,
    })
}

/// Wrap the shared apply report in the MCP envelope.
pub(crate) fn envelope(report: ApplyReport) -> MutationResult<ApplyOutput> {
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_apply_report(ApplyOutput { report: value }, &report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_plan_parses_and_carries_flags() {
        let p = ApplyParams {
            plan: json!({
                "schema_version": norn_wire::MIGRATION_PLAN_SCHEMA_VERSION,
                "vault_root": "/v",
                "operations": []
            }),
            confirm: true,
            parents: true,
        };
        let wire = to_wire(p).unwrap();
        assert_eq!(wire.plan.vault_root, "/v");
        assert!(wire.confirm);
        assert!(wire.parents);
    }

    #[test]
    fn wrong_schema_version_is_rejected() {
        let p = ApplyParams {
            plan: json!({ "schema_version": 999, "vault_root": "/v", "operations": [] }),
            ..Default::default()
        };
        let err = to_wire(p).unwrap_err();
        assert!(err.contains("schema_version"), "got: {err}");
    }

    #[test]
    fn malformed_plan_is_rejected() {
        let p = ApplyParams {
            plan: json!("not a plan"),
            ..Default::default()
        };
        let err = to_wire(p).unwrap_err();
        assert!(err.contains("not a valid MigrationPlan"), "got: {err}");
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<ApplyParams>(json!({ "plan": {}, "x": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains('x'));
    }
}
