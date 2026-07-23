//! `vault.edit` — apply structured section/text edit ops to one document's body.
//!
//! The param struct mirrors `norn edit`'s resolved-ops surface; the handler
//! serializes the ops array onto the wire [`EditParams`] (the owner runs the
//! transform on the SAME resolved array), routes to the owner, and wraps the
//! wire [`EditReport`] in the `{ report: … }` envelope, deriving `isError` from
//! the report's outcome (a confirmed refusal is `isError: true`; a forecast is
//! never).

use norn_wire::{EditParams as WireEditParams, EditReport};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.edit`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct EditParams {
    /// Target document (stem or path), as `norn edit` accepts.
    pub target: String,
    /// The section/text edit ops as a JSON array — one object per op, in the
    /// `norn edit --edits-json` vocabulary (e.g.
    /// `[{"op":"str_replace","old":"a","new":"b"}]`,
    /// `[{"op":"append_to_section","section":"Log","content":"…"}]`). Applied in
    /// order; an empty array is a no-op.
    #[serde(default)]
    pub edits: serde_json::Value,
    /// Opt-in compare-and-swap precondition: the document's full-content blake3
    /// hex (from a read's `document_hash`). The edit refuses if the on-disk
    /// content has drifted from this hash.
    #[serde(default)]
    pub expected_hash: Option<String>,
    /// Apply the edit. **Defaults to `false` (dry-run): the call returns the
    /// planned edit with `applied = false` and writes nothing.** Pass `true` to
    /// write.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.edit` — the wire report wrapped as a generic
/// `Value` under `report`, giving rmcp the required `type: object` root.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EditOutput {
    /// The full `norn edit` report: the applied flag, the per-op edit changes,
    /// the body-change summary, outcome, and any coded refusal.
    pub report: serde_json::Value,
}

/// Serialize the ops array onto the wire `edits` text. A null / empty value is
/// sent as the empty string (the owner treats it as no ops).
fn edits_text(edits: &serde_json::Value) -> String {
    if edits.is_null() {
        return String::new();
    }
    if let Some(arr) = edits.as_array() {
        if arr.is_empty() {
            return String::new();
        }
    }
    serde_json::to_string(edits).unwrap_or_default()
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: EditParams) -> WireEditParams {
    WireEditParams {
        target: p.target,
        edits: edits_text(&p.edits),
        expected_hash: p.expected_hash,
        confirm: p.confirm,
    }
}

/// Wrap the wire report in the MCP envelope. A CONFIRMED edit whose outcome is
/// `refused` is `isError: true`; a dry-run forecast never is (NRN-220).
pub(crate) fn envelope(confirm: bool, report: EditReport) -> MutationResult<EditOutput> {
    let outcome = report.outcome;
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_outcome(EditOutput { report: value }, confirm, outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::MutationOutcome;
    use rmcp::handler::server::tool::IntoCallToolResult;
    use serde_json::json;

    fn report(outcome: MutationOutcome) -> EditReport {
        EditReport {
            schema_version: norn_wire::EDIT_REPORT_SCHEMA_VERSION,
            trace_id: String::new(),
            operation: "edit".into(),
            target: "notes/a.md".into(),
            edits: vec![],
            body_changed: false,
            body_bytes_old: None,
            body_bytes_new: None,
            applied: false,
            outcome,
            error: None,
        }
    }

    #[test]
    fn edits_array_serializes_onto_the_wire_text() {
        let wire = to_wire(EditParams {
            target: "a".into(),
            edits: json!([{"op":"str_replace","old":"x","new":"y"}]),
            confirm: true,
            ..Default::default()
        });
        assert!(wire.edits.contains("str_replace"));
        assert!(wire.confirm);
    }

    #[test]
    fn empty_edits_is_the_empty_string() {
        assert_eq!(edits_text(&json!([])), "");
        assert_eq!(edits_text(&serde_json::Value::Null), "");
    }

    #[test]
    fn confirmed_refusal_is_error() {
        let env = envelope(true, report(MutationOutcome::Refused));
        assert_eq!(env.into_call_tool_result().unwrap().is_error, Some(true));
    }

    #[test]
    fn forecast_refusal_is_not_error() {
        let env = envelope(false, report(MutationOutcome::Refused));
        assert_eq!(env.into_call_tool_result().unwrap().is_error, Some(false));
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err =
            serde_json::from_value::<EditParams>(json!({ "target": "a", "bogus": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("bogus"));
    }
}
