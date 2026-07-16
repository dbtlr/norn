//! `Noted<R>` — the general operator-note envelope facility (NRN-215).
//!
//! Some daemon-side operations produce operator notes that the DIRECT (non-daemon)
//! path would print to the CLI's stderr — the canonical case is the write-lock
//! contention note (`crate::cache::LOCK_CONTENTION_NOTE`) that
//! `VaultContext::query_cache_warm` emits when the implicit refresh times out and
//! the read proceeds against the current cache state. When a read is ROUTED
//! through the warm `norn serve` daemon, that note would otherwise land on the
//! daemon's own stderr — invisible to the caller — so the operator loses the
//! possibly-stale-read signal exactly when routing (the NRN-94-review bug this
//! fixes).
//!
//! [`Noted<R>`] carries those notes alongside ANY tool's structured output. It is
//! produced in one place — the shared tool funnel
//! [`McpServer::run_wrapped`](crate::mcp::server) — which drains the request's
//! notes off its own [`RequestScope`](crate::env::RequestScope) (NRN-253:
//! a fresh per-request note buffer, so notes are structurally bound to the request
//! that produced them and cannot leak across concurrent requests — no shared
//! context buffer to clear) and wraps the tool's own result. On serialization it
//! adds an `operator_notes` array as a SIBLING key inside the tool's
//! `structuredContent` object — additive, so a tool whose run produced no notes
//! serializes byte-for-byte as before (the key is omitted entirely). The routed
//! CLI reads that sibling generically (`route_read` in `src/lib.rs`) and re-emits
//! each note on its own stderr, reproducing the direct path's note byte-identically.
//!
//! `operator_notes` is deliberately NOT part of any tool's advertised
//! `outputSchema`: it is a cross-cutting envelope sibling, not tool-specific
//! payload, and the schemas the parity gate pins
//! (`non_json_tools_advertise_their_payload_schema`) describe the payload type
//! only. Schemars-derived schemas do not set `additionalProperties: false`, so the
//! extra key is schema-valid.

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::CallToolResult;
use serde_json::Value;

/// The `structuredContent` key operator notes are injected under.
pub(crate) const OPERATOR_NOTES_KEY: &str = "operator_notes";

/// A tool result `R` plus the operator notes accumulated while serving the
/// request. `R` is whatever the tool already returned (`Json<T>` for a plain
/// read, `MutationResult<T>` for a tool that also sets `isError`); this wrapper
/// only adds the notes, so it composes over every tool uniformly.
pub struct Noted<R> {
    inner: R,
    notes: Vec<String>,
}

impl<R> Noted<R> {
    /// Pair a tool result with the request's operator notes. An empty `notes`
    /// leaves the serialized envelope byte-identical to the bare `R`.
    pub(crate) fn new(inner: R, notes: Vec<String>) -> Self {
        Self { inner, notes }
    }

    /// The wrapped tool result. Test-only: production consumes the wrapper
    /// through [`IntoCallToolResult`], which serializes it (plus any notes) into
    /// `structuredContent`.
    #[cfg(test)]
    pub(crate) fn inner(&self) -> &R {
        &self.inner
    }

    /// The captured operator notes. Test-only.
    #[cfg(test)]
    pub(crate) fn notes(&self) -> &[String] {
        &self.notes
    }
}

impl<R: IntoCallToolResult> IntoCallToolResult for Noted<R> {
    fn into_call_tool_result(self) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut result = self.inner.into_call_tool_result()?;
        if !self.notes.is_empty() {
            inject_operator_notes(&mut result, self.notes);
        }
        Ok(result)
    }
}

/// Insert the `operator_notes` array into a result's `structuredContent` object.
///
/// A no-op (notes dropped) when the result carries no object-shaped
/// `structuredContent` — every norn tool returns a `type: object` envelope, so
/// this branch is unreachable in practice, but a note is a diagnostic aid, never
/// load-bearing state, so silently dropping it is preferable to failing the call.
fn inject_operator_notes(result: &mut CallToolResult, notes: Vec<String>) {
    if let Some(Value::Object(obj)) = result.structured_content.as_mut() {
        obj.insert(
            OPERATOR_NOTES_KEY.to_string(),
            Value::Array(notes.into_iter().map(Value::String).collect()),
        );
    }
}

/// Read the `operator_notes` array out of a routed tool's `structuredContent`.
///
/// Tolerant by design (NRN-215 additive contract): a wire missing the key — an
/// older daemon, or any run that produced no notes — yields an empty vector, so a
/// happy-path routed read stays byte-identical to direct (no stderr lines). The
/// version gate on the routed request connection means a routed daemon is always
/// this exact build, so "missing key" only ever means "this run had no notes",
/// never "an incompatible daemon dropped a note this build would have forwarded"
/// — the additive default is safe. Non-string array entries are skipped rather
/// than failing the read (a diagnostic aid must never turn a good read into a
/// fallback).
#[cfg(unix)]
pub(crate) fn operator_notes_from_structured(structured: &Value) -> Vec<String> {
    structured
        .get(OPERATOR_NOTES_KEY)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::wrapper::Json;
    use serde_json::json;

    /// A non-empty note set is injected as an `operator_notes` sibling in the
    /// tool's `structuredContent`, leaving the tool's own fields untouched.
    #[test]
    fn injects_operator_notes_sibling() {
        let noted = Noted::new(Json(json!({ "total": 3 })), vec!["note one".to_string()]);
        let result = noted.into_call_tool_result().expect("serialize");
        let sc = result
            .structured_content
            .expect("structured content present");
        assert_eq!(sc["total"], 3, "the tool's own fields survive");
        assert_eq!(
            sc[OPERATOR_NOTES_KEY],
            json!(["note one"]),
            "operator_notes injected as a sibling array"
        );
    }

    /// An empty note set adds NOTHING — the envelope is byte-identical to the bare
    /// inner result, so a note-free run cannot regress the happy path.
    #[test]
    fn empty_notes_leave_envelope_untouched() {
        let bare = Json(json!({ "total": 3 }))
            .into_call_tool_result()
            .expect("serialize bare");
        let noted = Noted::new(Json(json!({ "total": 3 })), Vec::new())
            .into_call_tool_result()
            .expect("serialize noted");
        assert_eq!(
            noted.structured_content, bare.structured_content,
            "empty notes must not add the operator_notes key"
        );
        assert!(
            noted
                .structured_content
                .expect("present")
                .get(OPERATOR_NOTES_KEY)
                .is_none(),
            "no operator_notes key when there are no notes"
        );
    }

    /// The in-band tool-failure envelope (NRN-219/220: `MutationResult` with
    /// `isError: true`, which crosses as `structured_error` PRESERVING the
    /// structured report) carries the notes exactly like a success — it flows
    /// through `run_wrapped`'s Ok arm, so a contended request that ends in a
    /// coded refusal / not-found still forwards its operator note.
    #[test]
    fn injects_notes_into_is_error_envelope() {
        use crate::mcp::mutation_result::MutationResult;
        let noted = Noted::new(
            MutationResult::from_flag(json!({ "report": { "outcome": "refused" } }), true),
            vec!["note one".to_string()],
        );
        let result = noted.into_call_tool_result().expect("serialize");
        assert_eq!(result.is_error, Some(true), "the error bit survives Noted");
        let sc = result
            .structured_content
            .expect("structured report preserved on the error path");
        assert_eq!(sc["report"]["outcome"], "refused");
        assert_eq!(
            sc[OPERATOR_NOTES_KEY],
            json!(["note one"]),
            "operator_notes must ride the isError envelope too"
        );
    }

    /// The reader inverts the injection: notes injected server-side are recovered
    /// verbatim from the serialized `structuredContent`.
    #[cfg(unix)]
    #[test]
    fn reader_recovers_injected_notes() {
        let notes = vec!["first".to_string(), "second".to_string()];
        let result = Noted::new(Json(json!({ "total": 0 })), notes.clone())
            .into_call_tool_result()
            .expect("serialize");
        let structured = result.structured_content.expect("present");
        assert_eq!(operator_notes_from_structured(&structured), notes);
    }

    /// The reader treats a missing key as no-notes (additive tolerance).
    #[cfg(unix)]
    #[test]
    fn reader_missing_key_is_empty() {
        assert!(operator_notes_from_structured(&json!({ "total": 0 })).is_empty());
    }
}
