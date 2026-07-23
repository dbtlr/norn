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
//!
//! # Refusal fact vs advisory channel (mutation-report consumption rule)
//!
//! A refused mutation carries the FULL report envelope — `outcome` plus the
//! coded `{code, message, path?}` error — and, on the read-shaped verbs, the
//! typed `{severity, code, message}` notes/warnings channel (NRN-407). Those two
//! surfaces are related but distinct, so this crate — the first cross-surface
//! consumer — fixes the rule: **the report `outcome` (and its coded error) is
//! the AUTHORITATIVE refusal fact; notes/warnings are ADVISORY context.** The
//! `isError` bit is therefore derived from `outcome` / the outcome→exit mapping
//! alone (see [`MutationResult::from_outcome`] / [`from_apply_report`]), never
//! from the presence of a warning. Preview detection stays a per-family read of
//! the report's own fields (`set`/`new`/`edit` forecast vs the cascade verbs'
//! `dry_run` flag); the wrapper surfaces no separately-derived preview flag.
//!
//! [`from_apply_report`]: MutationResult::from_apply_report

use std::sync::Arc;

use norn_wire::{ApplyReport, MutationOutcome};
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

/// A report-shaped read output whose whole `serde_json::Value` IS the tool's
/// `structuredContent` — emitted FLAT (not wrapped under a `report` key), the
/// shape the read verbs use. The report types carry `Value` fields (a
/// `describe` schema, a `repair` plan) with no `JsonSchema` impl, so a derived
/// struct schema is impossible; this newtype serializes transparently and
/// publishes a minimal `type: object` schema by hand (rmcp REQUIRES the output
/// schema root to be `type: object` — a bare `Value` schema is rejected).
pub struct FlatReport(pub serde_json::Value);

impl Serialize for FlatReport {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl JsonSchema for FlatReport {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("FlatReport")
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // The report shape is verb-specific and carries opaque `Value` subtrees;
        // publish the minimal valid root (`type: object`) rmcp demands.
        schemars::json_schema!({ "type": "object" })
    }
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

    /// Build from a verb-report [`MutationOutcome`] (`set` / `new` / `edit`): a
    /// CONFIRMED apply whose outcome is `refused` is `isError: true`; a dry-run
    /// forecast never is (NRN-220 — a preview must not throw in an SDK that
    /// raises on `isError`), so `confirm` gates the flag.
    pub fn from_outcome(value: T, confirm: bool, outcome: MutationOutcome) -> Self {
        Self {
            value,
            is_error: confirm && matches!(outcome, MutationOutcome::Refused),
        }
    }

    /// Build from a cascade-verb [`ApplyReport`] (`move` / `delete` /
    /// `rewrite_wikilink` / `apply`): a report that did NOT cleanly apply — a
    /// pre-flight refusal (exit 2) or a partial-apply failure (exit 1) — is
    /// `isError: true`. A `dry_run` forecast is a preview and never flags an
    /// error even when its outcome is a forecasted refusal (same NRN-220 rule as
    /// [`from_outcome`](Self::from_outcome)).
    pub fn from_apply_report(value: T, report: &ApplyReport) -> Self {
        Self {
            value,
            is_error: !report.dry_run && report.exit_code() != 0,
        }
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
