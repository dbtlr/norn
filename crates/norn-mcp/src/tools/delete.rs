//! `vault.delete` — remove a document, leaving or redirecting its backlinks.
//!
//! The param struct mirrors `norn delete`'s flags; the handler routes to the
//! owner and wraps the shared wire [`ApplyReport`] in the `{ report: … }`
//! envelope, deriving `isError` from the report's outcome.
//!
//! # Delete-hash contract (read → delete)
//!
//! The delete plan carries a REQUIRED plan-time compare-and-swap: the current
//! document content hash (ADR 0024 / NRN-151), so a delete refuses if the target
//! drifted since it was inspected. That hash is stamped OWNER-SIDE from the warm
//! index at plan time — this verb takes no client-supplied hash. A client
//! obtains a document's hash from a read (`vault.get` / `vault.find` with
//! `col: ".document_hash"`) to detect drift up front; to stamp a client-chosen
//! hash into a delete precondition explicitly, build the plan and run it through
//! `vault.apply` (whose typed plan carries the op's `document_hash`).

use norn_wire::{ApplyReport, DeleteParams as WireDeleteParams};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.delete`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct DeleteParams {
    /// Target document (stem or path), as `norn delete` accepts.
    pub target: String,
    /// Redirect the deleted document's incoming links to this alternate target
    /// (a stem or path). Mutually exclusive with `allow_broken_links`.
    #[serde(default)]
    pub rewrite_to: Option<String>,
    /// Delete even though incoming links will be left dangling (`--allow-broken-links`).
    #[serde(default)]
    pub allow_broken_links: bool,
    /// Apply the delete. **Defaults to `false` (dry-run): the call returns the
    /// planned delete + link impact with `dry_run = true` and writes nothing.**
    /// Pass `true` to write. The current content hash is stamped as a required
    /// compare-and-swap precondition, so a drifted target refuses.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.delete` — the shared apply report wrapped as a
/// generic `Value` under `report`, giving rmcp the required `type: object` root.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeleteOutput {
    /// The full apply report: per-op status, the incoming-link impact, outcome,
    /// and any coded refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: DeleteParams) -> WireDeleteParams {
    WireDeleteParams {
        target: p.target,
        rewrite_to: p.rewrite_to,
        allow_broken_links: p.allow_broken_links,
        confirm: p.confirm,
    }
}

/// Wrap the shared apply report in the MCP envelope.
pub(crate) fn envelope(report: ApplyReport) -> MutationResult<DeleteOutput> {
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_apply_report(DeleteOutput { report: value }, &report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use norn_wire::ApplyError;
    use rmcp::handler::server::tool::IntoCallToolResult;
    use serde_json::json;

    #[test]
    fn confirmed_refusal_is_error() {
        let report = ApplyReport::refused(
            "/v".into(),
            false,
            "delete_document",
            ApplyError {
                code: "stale-document-hash".into(),
                message: "drifted".into(),
                path: Some("a.md".into()),
            },
        );
        let env = envelope(report);
        let result = env.into_call_tool_result().unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["report"]["operations"][0]["error"]["code"],
            "stale-document-hash"
        );
    }

    #[test]
    fn rewrite_to_maps_onto_the_wire() {
        let wire = to_wire(DeleteParams {
            target: "a".into(),
            rewrite_to: Some("b".into()),
            confirm: true,
            ..Default::default()
        });
        assert_eq!(wire.rewrite_to, Some("b".to_string()));
        assert!(wire.confirm);
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err =
            serde_json::from_value::<DeleteParams>(json!({ "target": "a", "x": 1 })).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains('x'));
    }
}
