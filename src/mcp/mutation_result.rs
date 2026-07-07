//! `MutationResult<T>` — a structured tool output paired with the MCP `isError`
//! bit (NRN-219 / BUG-3).
//!
//! The four cascade mutation tools (`vault.apply` / `move` / `delete` /
//! `rewrite_wikilink`) return an in-band `ApplyReport` even when the mutation did
//! NOT apply — a byte-identical pre-flight refusal (a CAS / stale-hash mismatch)
//! or a partial-apply failure. rmcp's [`Json<T>`](rmcp::handler::server::wrapper::Json)
//! wrapper always renders `isError: false`, so before this wrapper those
//! not-applied outcomes crossed MCP looking like success — a consumer trusting
//! the protocol-native `isError` bit read a no-write or a half-write as a
//! completed mutation.
//!
//! This wrapper carries an `isError` bit derived from the report's own outcome and
//! renders [`CallToolResult::structured_error`] when the mutation did not apply,
//! while ALWAYS preserving the structured report in `structuredContent` — a
//! consumer still branches on `operations[].error.code` (retryable CAS drift vs
//! terminal refusal); the machine-readable code is never laundered back to prose
//! (which would undo the B7 / NRN-150 delivery).
//!
//! **Schema note:** rmcp's `#[tool]` macro auto-derives a tool's `outputSchema`
//! only when the return type is the literal `Json` identifier (it name-matches
//! `Json`, it does not inspect the `JsonSchema` trait). A tool returning
//! `MutationResult<T>` must therefore pass an explicit `output_schema = …`
//! attribute, or its schema silently drops from `tools/list`. The always-success
//! mutators (`new` / `set` / `edit`, which surface an apply failure as a plain MCP
//! `Err`) keep returning `Json<T>` and its auto-derived schema. A guard test
//! (`parity_gate::every_tool_advertises_an_output_schema`) fails the build if any
//! tool loses its schema.

use std::sync::Arc;

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResult, JsonObject};
use schemars::JsonSchema;
use serde::Serialize;

use crate::apply_report::ApplyReport;

/// The `outputSchema` a `MutationResult<T>`-returning tool must publish via its
/// explicit `output_schema = …` attribute.
///
/// rmcp's `#[tool]` macro auto-derives `outputSchema` only for the literal
/// `Json<T>` return type (it name-matches `Json`), so a `MutationResult<T>` tool
/// would otherwise advertise no schema at all (NRN-219). This reproduces exactly
/// what the macro generates for `Json<T>` — the same `schema_for_output::<T>()`
/// call — so the published schema is byte-for-byte what the tool advertised
/// before the wrapper. Panics only if `T`'s schema is not a valid output schema,
/// which for a `#[derive(JsonSchema)]` struct cannot happen at runtime.
pub fn output_schema_for<T: JsonSchema + std::any::Any>() -> Arc<JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<T>().unwrap_or_else(|e| {
        panic!(
            "invalid outputSchema for {}: {e}",
            std::any::type_name::<T>()
        )
    })
}

/// A structured mutation output `T` plus whether the call ended in an error.
pub struct MutationResult<T> {
    value: T,
    is_error: bool,
}

impl<T> MutationResult<T> {
    /// Build from an apply outcome. `isError` is derived from the report itself —
    /// this is the ONE site that owns the `outcome → isError` mapping, so no call
    /// site can pass a bit that disagrees with the report (BUG-3 / NRN-219):
    ///
    /// - A **confirm** apply that did not fully apply (`refused` or `failed`,
    ///   i.e. `exit_code() != 0`) → `isError: true`. The write-signal a consumer
    ///   trusts now reflects that nothing — or only part — was written.
    /// - A **dry-run** forecast → always `isError: false`. A dry-run attempts no
    ///   write, so it cannot misreport one; a *forecasted* refusal is carried as
    ///   `outcome: refused` inside the (still-preserved) structured report, not as
    ///   a failed tool call — so a `confirm: false` preview never throws in an SDK
    ///   that raises on `isError`.
    pub fn from_apply_report(value: T, report: &ApplyReport) -> Self {
        Self {
            value,
            is_error: !report.dry_run && report.exit_code() != 0,
        }
    }

    /// The MCP `isError` bit this result will render with. Test-only: production
    /// consumes the wrapper through [`IntoCallToolResult`].
    #[cfg(test)]
    pub fn is_error(&self) -> bool {
        self.is_error
    }
}

impl<T: Serialize + 'static> IntoCallToolResult for MutationResult<T> {
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
    use serde::Serialize;

    #[derive(Serialize)]
    struct Sample {
        report: SampleReport,
    }

    #[derive(Serialize)]
    struct SampleReport {
        outcome: String,
    }

    fn sample(outcome: &str) -> Sample {
        Sample {
            report: SampleReport {
                outcome: outcome.to_string(),
            },
        }
    }

    /// The error bit sets `isError: true` AND preserves the structured report, so
    /// a consumer can still read `report.outcome` / an op's `error.code`. This is
    /// the BUG-3 invariant: `isError` reflects the not-applied outcome without
    /// laundering the code back to prose. (`mod tests` is a descendant module, so
    /// it constructs the wrapper via its private fields directly — the only public
    /// constructor is the outcome-deriving `from_apply_report`.)
    #[test]
    fn error_bit_renders_structured_error_preserving_content() {
        let result = MutationResult {
            value: sample("refused"),
            is_error: true,
        }
        .into_call_tool_result()
        .expect("serialize");

        assert_eq!(result.is_error, Some(true));
        let sc = result
            .structured_content
            .expect("structured report must survive on the error path");
        assert_eq!(sc["report"]["outcome"], "refused");
    }

    /// The success bit renders exactly what `Json<T>` produced before:
    /// `isError: false` with the structured content present.
    #[test]
    fn success_bit_renders_structured_success() {
        let result = MutationResult {
            value: sample("applied"),
            is_error: false,
        }
        .into_call_tool_result()
        .expect("serialize");

        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            result
                .structured_content
                .expect("structured content present")["report"]["outcome"],
            "applied"
        );
    }
}
