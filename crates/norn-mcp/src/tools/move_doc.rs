//! `vault.move` — relocate a document/folder and cascade-rewrite backlinks.
//!
//! The param struct mirrors `norn move`'s flags; the handler routes to the owner
//! and wraps the shared wire [`ApplyReport`] in the `{ report: … }` envelope,
//! deriving `isError` from the report's outcome (a confirmed refusal / partial
//! failure is `isError: true`; a `dry_run` forecast never is).

use norn_wire::{ApplyReport, MoveParams as WireMoveParams};
use serde::{Deserialize, Serialize};

use crate::mutation_result::MutationResult;

/// Parameters for `vault.move`.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct MoveParams {
    /// Source: a stem, an exact vault-relative `.md` path, or (with `recursive`)
    /// a directory. Resolved owner-side against the warm index.
    pub from: String,
    /// Destination: a stem, an exact vault-relative `.md` path, or a directory.
    pub to: String,
    /// Move a folder and everything under it (`--recursive`).
    #[serde(default)]
    pub recursive: bool,
    /// Auto-create missing parent directories at the destination (`--parents`).
    #[serde(default)]
    pub parents: bool,
    /// Overwrite an existing destination (`--force`).
    #[serde(default)]
    pub force: bool,
    /// Skip the backlink cascade-rewrite (`--no-link-rewrite`).
    #[serde(default)]
    pub no_link_rewrite: bool,
    /// Apply the move. **Defaults to `false` (dry-run): the call returns the
    /// planned move + cascade with `dry_run = true` and writes nothing.** Pass
    /// `true` to write.
    #[serde(default)]
    pub confirm: bool,
}

/// Structured output for `vault.move` — the shared apply report wrapped as a
/// generic `Value` under `report`, giving rmcp the required `type: object` root.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MoveOutput {
    /// The full apply report: per-op status, the backlink cascade summary,
    /// outcome, and any coded refusal / warnings.
    pub report: serde_json::Value,
}

/// Build the wire request from the MCP params.
pub(crate) fn to_wire(p: MoveParams) -> WireMoveParams {
    WireMoveParams {
        from: p.from,
        to: p.to,
        recursive: p.recursive,
        parents: p.parents,
        force: p.force,
        no_link_rewrite: p.no_link_rewrite,
        confirm: p.confirm,
    }
}

/// Wrap the shared apply report in the MCP envelope. The report's own `outcome`
/// (and `dry_run`) is the authoritative refusal fact driving `isError`.
pub(crate) fn envelope(report: ApplyReport) -> MutationResult<MoveOutput> {
    let value = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    MutationResult::from_apply_report(MoveOutput { report: value }, &report)
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
            "move_document",
            ApplyError {
                code: "target-not-found".into(),
                message: "no doc".into(),
                path: None,
            },
        );
        let env = envelope(report);
        let result = env.into_call_tool_result().unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["report"]["outcome"],
            "refused"
        );
    }

    #[test]
    fn forecast_refusal_is_not_error() {
        // A dry-run forecast never flags an error, even on a forecasted refusal.
        let report = ApplyReport::refused(
            "/v".into(),
            true,
            "move_document",
            ApplyError {
                code: "target-not-found".into(),
                message: "no doc".into(),
                path: None,
            },
        );
        assert_eq!(
            envelope(report).into_call_tool_result().unwrap().is_error,
            Some(false)
        );
    }

    #[test]
    fn flags_map_onto_the_wire() {
        let wire = to_wire(MoveParams {
            from: "a".into(),
            to: "b".into(),
            recursive: true,
            confirm: true,
            ..Default::default()
        });
        assert_eq!(wire.from, "a");
        assert!(wire.recursive);
        assert!(wire.confirm);
    }

    #[test]
    fn unknown_param_key_is_rejected() {
        let err = serde_json::from_value::<MoveParams>(json!({ "from": "a", "to": "b", "x": 1 }))
            .unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains('x'));
    }
}
