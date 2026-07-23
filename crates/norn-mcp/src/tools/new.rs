//! `vault.new` — create a document (explicit path, by rule, or inbox).
//!
//! The param struct mirrors `norn new`'s creation modes and overrides; the
//! handler routes to the owner and wraps the wire [`NewReport`] in the
//! `{ report: … }` envelope, deriving the MCP `isError` bit from the report's
//! outcome (a confirmed refusal is `isError: true`; a dry-run forecast never is).

use norn_wire::{NewParams as WireNewParams, NewReport};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.new`. Exactly one of `path` (explicit path) or `rule`
/// (by named creatable rule) selects the mode; neither → inbox (requires
/// `title`).
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct NewParams {
    /// Explicit vault-relative destination path (Mode A). Mutually exclusive
    /// with `rule`.
    #[serde(default)]
    pub path: Option<String>,
    /// Named creatable rule to instantiate (Mode B) — as `norn new --as`.
    /// Mutually exclusive with `path`.
    #[serde(default)]
    pub rule: Option<String>,
    /// Document title. Required for the inbox mode (neither `path` nor `rule`);
    /// otherwise an override.
    #[serde(default)]
    pub title: Option<String>,
    /// `KEY=VALUE` template variables, repeatable — fed to `norn new --var`.
    #[serde(default)]
    pub var: Vec<String>,
    /// Frontmatter overrides in `KEY=VALUE` format, repeatable — string-coerced
    /// against the schema, as `norn new --field`.
    #[serde(default)]
    pub field: Vec<String>,
    /// Frontmatter overrides as raw `KEY=JSON` tokens, repeatable — JSON-parsed
    /// and schema-validated, as `norn new --field-json`.
    #[serde(default)]
    pub field_json: Vec<String>,
    /// Document body — the MCP analogue of `norn new --body-from-stdin`.
    #[serde(default)]
    pub body: Option<String>,
    /// Auto-create missing parent directories (`--parents`).
    #[serde(default)]
    pub parents: bool,
    /// Overwrite an existing destination and skip coercion (`--force`).
    #[serde(default)]
    pub force: bool,
    /// Apply the creation. **Defaults to `false` (dry-run): the call returns the
    /// planned document with `applied = false` and writes nothing.** Pass `true`
    /// to write.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.new` — the wire report wrapped as a generic
/// `Value` under `report` (the report carries a path type with no `JsonSchema`
/// impl), giving rmcp the required `type: object` root.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct NewOutput {
    /// The full `norn new` report: the applied flag, created path, created
    /// frontmatter, outcome, and any coded refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: NewParams) -> WireNewParams {
    WireNewParams {
        path: p.path,
        as_rule: p.rule,
        title: p.title,
        vars: p.var,
        fields: p.field,
        field_json: p.field_json,
        body: p.body,
        parents: p.parents,
        force: p.force,
        confirm: p.confirm,
    }
}

/// Wrap the wire report in the MCP envelope. A CONFIRMED create whose outcome is
/// `refused` is `isError: true`; a dry-run forecast never is (NRN-220).
pub(crate) fn envelope(confirm: bool, report: NewReport) -> MutationResult<NewOutput> {
    let outcome = report.outcome;
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_outcome(NewOutput { report: value }, confirm, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::MutationOutcome;
    use rmcp::handler::server::tool::IntoCallToolResult;
    use serde_json::json;

    fn report(outcome: MutationOutcome, applied: bool) -> NewReport {
        NewReport {
            schema_version: 2,
            trace_id: String::new(),
            telemetry_degraded: false,
            operation: "new".into(),
            path: Some("notes/a.md".into()),
            applied,
            outcome,
            frontmatter_created: vec![],
            body_bytes: 0,
            warnings: vec![],
            predicted_path: None,
            error: None,
        }
    }

    #[test]
    fn dry_run_refusal_is_not_error() {
        let env = envelope(false, report(MutationOutcome::Refused, false));
        assert_eq!(env.into_call_tool_result().unwrap().is_error, Some(false));
    }

    #[test]
    fn confirmed_refusal_is_error_and_preserves_the_report() {
        let env = envelope(true, report(MutationOutcome::Refused, false));
        let result = env.into_call_tool_result().unwrap();
        assert_eq!(result.is_error, Some(true));
        let sc = result.structured_content.unwrap();
        assert_eq!(sc["report"]["outcome"], "refused");
    }

    #[test]
    fn confirmed_apply_is_not_error() {
        let env = envelope(true, report(MutationOutcome::Applied, true));
        assert_eq!(env.into_call_tool_result().unwrap().is_error, Some(false));
    }

    #[test]
    fn rule_and_var_map_onto_the_wire() {
        let wire = to_wire(NewParams {
            rule: Some("task".into()),
            var: vec!["slug:x".into()],
            confirm: true,
            ..Default::default()
        });
        assert_eq!(wire.as_rule, Some("task".to_string()));
        assert_eq!(wire.vars, vec!["slug:x".to_string()]);
        assert!(wire.confirm);
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<NewParams>(json!({ "bogus": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("bogus"));
    }
}
