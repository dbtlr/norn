//! `MutationResult<T>` — a structured tool output paired with the MCP `isError`
//! bit.
//!
//! The four cascade mutation tools (`vault.apply` / `move` / `delete` /
//! `rewrite_wikilink`) return an in-band `ApplyReport` even when the mutation did
//! NOT apply — a no-op pre-flight refusal (a CAS / stale-hash mismatch)
//! or a partial-apply failure. rmcp's `Json<T>` wrapper always renders
//! `isError: false`, so before this wrapper those not-applied outcomes crossed MCP
//! looking like success. This wrapper carries an `isError` bit derived from the
//! report's own outcome and renders [`CallToolResult::structured_error`] when the
//! mutation did not apply, while ALWAYS preserving the structured report in
//! `structuredContent`.
//!
//! **Schema note:** rmcp's `#[tool]` macro auto-derives a tool's `outputSchema`
//! only when the return type is the literal `Json` identifier. A tool returning
//! `MutationResult<T>` must therefore pass an explicit `output_schema = …`
//! attribute via [`output_schema_for`], or its schema silently drops from
//! `tools/list`.

use std::sync::Arc;

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResult, JsonObject};
use schemars::JsonSchema;
use serde::Serialize;

/// The `outputSchema` a `MutationResult<T>`-returning tool must publish via its
/// explicit `output_schema = …` attribute.
///
/// rmcp's `#[tool]` macro auto-derives `outputSchema` only for the literal
/// `Json<T>` return type (it name-matches `Json`), so a `MutationResult<T>` tool
/// would otherwise advertise no schema at all. This reproduces exactly what the
/// macro generates for `Json<T>` — the same `schema_for_output::<T>()` call — so
/// the published schema is exactly what the macro emits for `Json<T>`.
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
    /// Build from a tool's own already-derived error flag (NRN-214) — the
    /// `vault.get` READ tool uses it to map its not-found / all-missed-section
    /// signal to `isError: true` while still returning its records.
    pub fn from_flag(value: T, is_error: bool) -> Self {
        Self { value, is_error }
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
