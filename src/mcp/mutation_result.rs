//! `MutationResult<T>` — a structured tool output paired with the MCP `isError`
//! bit (NRN-219 / BUG-3).
//!
//! Mirrors rmcp's [`Json<T>`](rmcp::handler::server::wrapper::Json): it delegates
//! `JsonSchema` to `T`, so a tool's `outputSchema` is byte-identical whether it
//! returns `Json<T>` or `MutationResult<T>` (the CLI↔MCP parity gate stays
//! green). Unlike `Json<T>` — which always renders `isError: false` via
//! [`CallToolResult::structured`] — this wrapper carries a bit and renders
//! [`CallToolResult::structured_error`] (`isError: true`) when the mutation did
//! not fully apply.
//!
//! The invariant (BUG-3): the protocol-native `isError` bit MUST agree with the
//! report's own `outcome`. A refused (byte-identical, nothing written) or a
//! partially-failed apply is `isError: true`; a clean apply or a successful
//! dry-run forecast is `isError: false`. In BOTH cases the structured report is
//! preserved in `structuredContent`, so a consumer still branches on
//! `operations[].error.code` (retryable CAS drift vs terminal refusal) — this
//! never re-launders the machine-readable code back into prose (which would undo
//! the B7 / NRN-150 delivery).
//!
//! The one place the bit is chosen is each mutation handler's `handle_output`,
//! from `report.exit_code() != 0` — the same `outcome → exit` mapping the CLI
//! exits on (`apply_report::ApplyReport::exit_code`), so a dry-run that forecasts
//! a refusal is `isError: true` on both surfaces (CLI exit 2 / MCP isError). Only
//! the four `refuse_as_report` handlers (apply/move/delete/rewrite-wikilink) can
//! return an in-band not-applied outcome; the single-op mutators (new/set/edit)
//! `?`-propagate their apply error and wrap with [`MutationResult::ok`].

use std::borrow::Cow;

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::Serialize;

/// A structured mutation output `T` plus whether the call ended in an error.
pub struct MutationResult<T> {
    value: T,
    is_error: bool,
}

impl<T> MutationResult<T> {
    /// Pair `value` with an explicit error bit. `is_error` must equal
    /// `report.exit_code() != 0` (i.e. `outcome ∈ {refused, failed}`); that is
    /// the whole point — `isError` and the report's `outcome` agree.
    pub fn new(value: T, is_error: bool) -> Self {
        Self { value, is_error }
    }

    /// A mutation with no in-band not-applied outcome: it either applied cleanly
    /// or already propagated an `Err` (mapped by `run_mutation`). Always
    /// `isError: false`.
    pub fn ok(value: T) -> Self {
        Self {
            value,
            is_error: false,
        }
    }

    /// The MCP `isError` bit this result will render with. Test-only: production
    /// consumes the wrapper through [`IntoCallToolResult`] rather than reading the
    /// bit directly, so the accessor exists for assertions.
    #[cfg(test)]
    pub fn is_error(&self) -> bool {
        self.is_error
    }
}

// Delegate the schema to `T` so `outputSchema` is identical to the `Json<T>` a
// tool would otherwise return — the parity gate reads this via `list_all()`.
impl<T: JsonSchema> JsonSchema for MutationResult<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        T::json_schema(generator)
    }
}

// Render to a `CallToolResult`, choosing the error vs success constructor by the
// carried bit while ALWAYS placing the serialized value in `structuredContent`.
impl<T: Serialize + JsonSchema + 'static> IntoCallToolResult for MutationResult<T> {
    fn into_call_tool_result(self) -> Result<CallToolResult, rmcp::ErrorData> {
        let value = serde_json::to_value(self.value).map_err(|e| {
            rmcp::ErrorData::internal_error(
                format!("Failed to serialize structured content: {e}"),
                None,
            )
        })?;
        Ok(if self.is_error {
            CallToolResult::structured_error(value)
        } else {
            CallToolResult::structured(value)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, JsonSchema)]
    struct SampleReport {
        outcome: String,
    }

    #[derive(Serialize, JsonSchema)]
    struct Sample {
        report: SampleReport,
    }

    fn sample(outcome: &str) -> Sample {
        Sample {
            report: SampleReport {
                outcome: outcome.to_string(),
            },
        }
    }

    /// The error path sets `isError: true` AND preserves the structured report,
    /// so a consumer can still read `report.outcome` / an op's `error.code`. This
    /// is the BUG-3 invariant: `isError` reflects the not-applied outcome without
    /// laundering the code back to prose.
    #[test]
    fn errored_renders_structured_error_preserving_content() {
        let result = MutationResult::new(sample("refused"), true)
            .into_call_tool_result()
            .expect("serialize");

        assert_eq!(result.is_error, Some(true));
        let sc = result
            .structured_content
            .expect("structured report must survive on the error path");
        assert_eq!(sc["report"]["outcome"], "refused");
    }

    /// The success path is byte-identical to what `Json<T>` produced before:
    /// `isError: false` with the structured content present.
    #[test]
    fn ok_renders_structured_success() {
        let wrapped = MutationResult::ok(sample("applied"));
        assert!(!wrapped.is_error());

        let result = wrapped.into_call_tool_result().expect("serialize");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result
                .structured_content
                .expect("structured content present")["report"]["outcome"],
            "applied"
        );
    }

    /// `new(_, false)` and `ok(_)` are the same result — the success bit.
    #[test]
    fn new_false_equals_ok() {
        assert!(!MutationResult::new(sample("applied"), false).is_error());
        assert!(MutationResult::new(sample("failed"), true).is_error());
    }
}
