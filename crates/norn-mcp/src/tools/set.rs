//! `vault.set` — update one document's frontmatter (and optionally its body),
//! schema-aware. DRY-RUN by default; `confirm: true` writes.
//!
//! The param struct mirrors `norn set`'s mutation flags; the handler routes to the
//! owner and wraps the wire [`SetReport`] in the `{ report: … }` envelope,
//! deriving the MCP `isError` bit from the report's outcome (a confirmed refusal /
//! failure is `isError: true`; a dry-run forecast never is).

use norn_wire::{MutationOutcome, SetParams as WireSetParams, SetReport};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.set`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
pub struct SetParams {
    /// Target document (stem or path), as `norn set` accepts.
    pub target: String,

    /// Frontmatter fields to set, as raw `KEY=JSON` tokens, repeatable. Applied
    /// in order and fed verbatim into `norn set --field-json KEY=JSON`: each value
    /// is JSON-parsed and schema-validated. A key repeated across tokens
    /// accumulates into an array. Empty list = no frontmatter change.
    #[serde(default)]
    pub field_json: Vec<String>,

    /// Frontmatter field overrides in `KEY=VALUE` format, repeatable. The value is
    /// string-coerced against the schema exactly like `norn set --field KEY=VALUE`.
    #[serde(default)]
    pub field: Vec<String>,

    /// Append to a list-typed frontmatter field, as raw `KEY=VALUE` tokens,
    /// repeatable — fed verbatim into `norn set --push KEY=VALUE`.
    #[serde(default)]
    pub push: Vec<String>,

    /// Remove from a list-typed frontmatter field, as raw `KEY=VALUE` tokens,
    /// repeatable — fed verbatim into `norn set --pop KEY=VALUE`.
    #[serde(default)]
    pub pop: Vec<String>,

    /// Frontmatter keys to remove entirely. Silent no-op for missing keys.
    #[serde(default)]
    pub remove: Vec<String>,

    /// Wholesale body replacement — the MCP analogue of `norn set
    /// --body-from-stdin`. Absent = body unchanged.
    #[serde(default)]
    pub body: Option<String>,

    /// Bypass schema enforcement (type validation + required-field protection),
    /// mirroring `norn set --force`.
    #[serde(default)]
    pub force: bool,

    /// Apply the mutation. **Defaults to `false` (dry-run): the call returns the
    /// planned change with `applied = false` and writes nothing.** Pass `true` to
    /// acquire the vault mutation lock and write the change to disk.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.set` — the wire report wrapped as a generic
/// `Value` under `report` (the report carries a path type with no `JsonSchema`
/// impl), giving rmcp the required `type: object` root.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SetOutput {
    /// The full `norn set` report: the applied flag, frontmatter changes, body
    /// change, outcome, and any coded refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: SetParams) -> WireSetParams {
    WireSetParams {
        target: p.target,
        fields: p.field,
        field_json: p.field_json,
        push: p.push,
        pop: p.pop,
        remove: p.remove,
        body: p.body,
        force: p.force,
        confirm: p.confirm,
    }
}

/// Wrap the wire report in the MCP envelope. `isError` is derived from the
/// outcome→exit vocabulary: a CONFIRM apply whose outcome is not
/// clean (`refused`) is `isError: true`; a dry-run forecast never is.
pub(crate) fn envelope(confirm: bool, report: SetReport) -> MutationResult<SetOutput> {
    let is_error = confirm && matches!(report.outcome, MutationOutcome::Refused);
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_flag(SetOutput { report: value }, is_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::tool::IntoCallToolResult;

    fn report(outcome: MutationOutcome, applied: bool) -> SetReport {
        SetReport {
            schema_version: 2,
            trace_id: String::new(),
            operation: "set".into(),
            target: "notes/alpha.md".into(),
            frontmatter_changes: vec![],
            body_changed: false,
            body_bytes_new: None,
            body_bytes_old: None,
            applied,
            outcome,
            error: None,
            warnings: vec![],
        }
    }

    #[test]
    fn dry_run_refusal_is_not_error() {
        // A confirm:false forecast never sets isError, even on a forecasted refusal
        // — an SDK that raises on isError must not throw on a preview (NRN-220).
        let env = envelope(false, report(MutationOutcome::Refused, false));
        let result = env.into_call_tool_result().unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn confirmed_refusal_is_error_and_preserves_the_report() {
        let env = envelope(true, report(MutationOutcome::Refused, false));
        let result = env.into_call_tool_result().unwrap();
        assert_eq!(
            result.is_error,
            Some(true),
            "a confirmed refusal is isError"
        );
        let sc = result
            .structured_content
            .expect("the structured report survives the error path");
        assert_eq!(sc["report"]["outcome"], "refused");
    }

    #[test]
    fn confirmed_apply_is_not_error() {
        let env = envelope(true, report(MutationOutcome::Applied, true));
        let result = env.into_call_tool_result().unwrap();
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn field_maps_to_wire_fields_and_confirm_carries() {
        let wire = to_wire(SetParams {
            target: "alpha".into(),
            field: vec!["status:done".into()],
            confirm: true,
            ..SetParams::default()
        });
        assert_eq!(wire.target, "alpha");
        assert_eq!(wire.fields, vec!["status:done".to_string()]);
        assert!(wire.confirm);
    }
}
