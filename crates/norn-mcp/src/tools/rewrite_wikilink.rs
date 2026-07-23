//! `vault.rewrite_wikilink` — rewrite every `[[old]]` reference to `[[new]]`.
//!
//! The param struct mirrors `norn rewrite-wikilink`; the handler routes to the
//! owner and wraps the shared wire [`ApplyReport`] in the `{ report: … }`
//! envelope, deriving `isError` from the report's outcome.

use norn_wire::{ApplyReport, RewriteWikilinkParams as WireRewriteWikilinkParams};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.rewrite_wikilink`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct RewriteWikilinkParams {
    /// The existing wikilink target to rewrite FROM (e.g. `old-note`).
    pub old: String,
    /// The wikilink target to rewrite TO (e.g. `new-note`).
    pub new: String,
    /// Apply the rewrite. **Defaults to `false` (dry-run): the call returns the
    /// planned rewrites with `dry_run = true` and writes nothing.** Pass `true`
    /// to write.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.rewrite_wikilink` — the shared apply report
/// wrapped as a generic `Value` under `report`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RewriteWikilinkOutput {
    /// The full apply report: the per-file rewrites, outcome, and any coded
    /// refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: RewriteWikilinkParams) -> WireRewriteWikilinkParams {
    WireRewriteWikilinkParams {
        old: p.old,
        new: p.new,
        confirm: p.confirm,
    }
}

/// Wrap the shared apply report in the MCP envelope.
pub(crate) fn envelope(report: ApplyReport) -> MutationResult<RewriteWikilinkOutput> {
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_apply_report(RewriteWikilinkOutput { report: value }, &report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::tool::IntoCallToolResult;
    use serde_json::json;

    #[test]
    fn maps_onto_the_wire() {
        let wire = to_wire(RewriteWikilinkParams {
            old: "a".into(),
            new: "b".into(),
            confirm: true,
        });
        assert_eq!(wire.old, "a");
        assert_eq!(wire.new, "b");
        assert!(wire.confirm);
    }

    #[test]
    fn applied_report_is_not_error() {
        let report = ApplyReport {
            schema_version: norn_wire::APPLY_REPORT_SCHEMA_VERSION,
            trace_id: String::new(),
            plan_hash: "h".into(),
            vault_root: "/v".into(),
            dry_run: false,
            applied: 1,
            skipped: 0,
            failed: 0,
            remaining: 0,
            preconditions: vec![],
            operations: vec![],
            warnings: vec![],
            outcome: norn_wire::ApplyOutcome::Applied,
            touched_paths: vec![],
        };
        assert_eq!(
            envelope(report).into_call_tool_result().unwrap().is_error,
            Some(false)
        );
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<RewriteWikilinkParams>(
            json!({ "old": "a", "new": "b", "x": 1 }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains('x'));
    }
}
