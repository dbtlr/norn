//! The MCP server handler — the thin rmcp adapter over a held [`OwnerSession`].
//!
//! Each `#[tool]` method deserializes MCP args, builds the verb's norn-wire
//! `Params`, runs the SAME routed owner request the CLI runs (via [`call`]), and
//! maps the returned `Report` into the MCP output envelope. No verb logic lives
//! here.
//!
//! The tools are split into two `#[tool_router]` blocks — `read_router` (the read
//! tools) and `mutate_router` (the mutation tools) — merged by [`McpServer::new`]
//! into one served surface.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use norn_client::{ClientError, OwnerSession};
use rmcp::handler::server::tool::{IntoCallToolResult, ToolRouter};
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::mutation_result::output_schema_for;

/// How long the resummon recovery waits for a freshly-summoned owner to warm up
/// to Ready — the same generous ceiling the CLI's routed read uses (warm-up is
/// size-linear; the owner answers pings throughout, so a truly hung owner is
/// caught by the per-request stall budget long before this).
const RESUMMON_MAX_WAIT: Duration = Duration::from_secs(120);

/// The recovery policy for a held session, factored out so it can be pinned in
/// isolation: run `attempt` against `session`; on a PRE-SEND owner-gone (the
/// request was provably never written — the held owner idle-reaped between
/// calls), `resummon` a fresh owner and run `attempt` exactly ONCE more. Every
/// other outcome propagates WITHOUT retry — success, an unrelated error, and
/// crucially a POST-SEND owner-gone ([`ClientError::OwnerGone`]: the request WAS
/// written but went unanswered, so a mutation may have applied — ADR 0011
/// post-send uncertainty — and must never be blind-retried). Generic over the
/// session type so the retry/no-retry decision is unit-testable without a live
/// owner.
fn retry_once_on_pre_send<S, T>(
    session: &mut S,
    attempt: impl Fn(&mut S) -> Result<T, ClientError>,
    resummon: impl FnOnce(&mut S) -> Result<(), ClientError>,
) -> Result<T, ClientError> {
    match attempt(session) {
        Err(e) if e.is_owner_gone_pre_send() => {
            resummon(session)?;
            attempt(session)
        }
        other => other,
    }
}

/// Why a routed tool call did not return its verb report. Splits the owner's
/// user-facing REJECTION (a bad predicate, an unresolvable target — the owner is
/// healthy) from a transport/owner FAULT (owner gone, protocol skew, health):
///
/// - [`CallError::Rejected`] becomes a STRUCTURED tool result (`isError: true`,
///   `{code, message, hints}`) — the request was well-formed and reached a
///   healthy owner, so it is a tool-level outcome an MCP client branches on, not
///   a JSON-RPC transport error (B2). The owner's soft-landing `hints` (NRN-361)
///   pass straight through.
/// - [`CallError::Fault`] becomes a JSON-RPC `internal_error` — the tool could
///   not run at all, so there is no tool result to return.
pub(crate) enum CallError {
    Rejected { message: String, hints: Vec<String> },
    Fault(rmcp::ErrorData),
}

/// Classify a summoner/owner [`ClientError`]: a healthy-owner `Rejected` rides
/// the structured tool-result path; every other variant is a transport/owner
/// fault rendered as a JSON-RPC `internal_error`.
fn classify_client_error(e: ClientError) -> CallError {
    match e {
        ClientError::Rejected { message, hints } => CallError::Rejected { message, hints },
        other => CallError::Fault(rmcp::ErrorData::internal_error(other.to_string(), None)),
    }
}

/// The structured tool result an owner `Rejected` renders to (B2): `isError:
/// true` with a `{code, message, hints}` body. `code` is the stable `rejected`
/// discriminant an MCP client keys on; `hints` passes the owner's soft-landing
/// lines through verbatim (empty in the common case).
pub(crate) fn rejected_result(message: String, hints: Vec<String>) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": "rejected",
        "message": message,
        "hints": hints,
    }))
}

/// Fold a routed call's outcome into the rmcp handler return: a produced reply
/// renders through [`IntoCallToolResult`]; a [`CallError::Rejected`] becomes the
/// structured error tool result; a [`CallError::Fault`] propagates as a JSON-RPC
/// error. The single seam every tool handler funnels through, so the Rejected /
/// Fault split (B2) is decided in exactly one place.
pub(crate) fn finish<R: IntoCallToolResult>(
    outcome: Result<R, CallError>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    match outcome {
        Ok(reply) => reply.into_call_tool_result(),
        Err(CallError::Rejected { message, hints }) => Ok(rejected_result(message, hints)),
        Err(CallError::Fault(err)) => Err(err),
    }
}

#[derive(Clone)]
pub struct McpServer {
    /// The held session to the vault's warm owner. Behind a mutex because rmcp's
    /// handler must be `Clone + Send + Sync`; the stdio transport is single-client
    /// and each tool call runs the blocking socket round-trip under the lock on a
    /// `spawn_blocking` thread, so calls serialize without ever blocking an async
    /// worker.
    session: Arc<Mutex<OwnerSession>>,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    /// Build the server: merge the read + mutate routers into one served surface.
    pub fn new(session: Arc<Mutex<OwnerSession>>) -> Self {
        let mut routers = Self::routers().into_iter();
        let mut router = routers
            .next()
            .expect("routers() always yields the read router");
        for extra in routers {
            router.merge(extra);
        }
        Self {
            session,
            tool_router: router,
        }
    }

    /// The tool routers that compose the served surface, in merge order. Single
    /// source of truth for which routers exist.
    pub(crate) fn routers() -> Vec<ToolRouter<Self>> {
        vec![Self::read_router(), Self::mutate_router()]
    }

    /// Run a blocking owner round-trip on a `spawn_blocking` thread, holding the
    /// session lock only for that thread's duration. The socket IO is blocking, so
    /// it must never run inline on an async worker.
    ///
    /// A held session outlives the owner's idle TTL (default 120s): once the owner
    /// idle-reaps, the next call fails PRE-SEND (the first write hits the closed
    /// socket). This seam recovers exactly that shape — resummon a fresh owner and
    /// retry ONCE (see [`retry_once_on_pre_send`]) — so a long-lived MCP server is
    /// never bricked by an idle reap. `f` must be `Fn` (it can run twice, on a
    /// freshly-summoned owner), so a tool handler clones its wire `Params` into the
    /// closure rather than moving them.
    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T, CallError>
    where
        T: Send + 'static,
        F: Fn(&mut OwnerSession) -> Result<T, ClientError> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
        let joined = tokio::task::spawn_blocking(move || {
            let mut guard = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            retry_once_on_pre_send(&mut *guard, |s| f(s), |s| s.resummon(RESUMMON_MAX_WAIT))
        })
        .await;
        match joined {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(classify_client_error(err)),
            Err(join_err) => Err(CallError::Fault(rmcp::ErrorData::internal_error(
                format!("tool task failed: {join_err}"),
                None,
            ))),
        }
    }
}

#[tool_router(router = read_router, vis = "pub(crate)")]
impl McpServer {
    /// `vault.count` — count documents in the vault, total or grouped.
    #[tool(
        name = "vault.count",
        description = "Count documents in the vault — total, or grouped by a frontmatter field — with the find filter surface.",
        output_schema = output_schema_for::<crate::tools::count::CountEnvelope>()
    )]
    async fn count(
        &self,
        Parameters(p): Parameters<crate::tools::count::CountParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::count::to_wire(p);
        let outcome = self
            .call(move |s| s.count(wire.clone()))
            .await
            .map(|report| Json(crate::tools::count::envelope(report)));
        finish(outcome)
    }

    /// `vault.find` — filter, sort, and page the vault's documents.
    #[tool(
        name = "vault.find",
        description = "Filter, sort, and page vault documents by frontmatter/body/link predicates; returns structured document records. Paging is ZERO-indexed via starts_at.",
        output_schema = output_schema_for::<crate::tools::find::FindOutput>()
    )]
    async fn find(
        &self,
        Parameters(p): Parameters<crate::tools::find::FindParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::find::to_wire(&p);
        let outcome = self
            .call(move |s| s.find(wire.clone()))
            .await
            .map(|report| Json(crate::tools::find::envelope(&p, report)));
        finish(outcome)
    }

    /// `vault.get` — fetch one or more documents with full connection context.
    #[tool(
        name = "vault.get",
        description = "Fetch structured documents, or one exact on-disk source document with format=markdown.",
        output_schema = output_schema_for::<crate::tools::get::GetOutput>()
    )]
    async fn get(
        &self,
        Parameters(p): Parameters<crate::tools::get::GetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::get::to_wire(&p);
        let outcome = self
            .call(move |s| s.get(wire.clone()))
            .await
            .map(|report| crate::tools::get::envelope(&p, report));
        finish(outcome)
    }

    /// `vault.describe` — report the vault's structure and configured schema.
    #[tool(
        name = "vault.describe",
        description = "Describe the vault's folders, path/creatable rules, inbox, and frontmatter schema; add data=true (or a `by` grouping) for the contents summary.",
        output_schema = output_schema_for::<crate::tools::describe::DescribeOutput>()
    )]
    async fn describe(
        &self,
        Parameters(p): Parameters<crate::tools::describe::DescribeParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::describe::to_wire(p);
        let outcome = self
            .call(move |s| s.describe(wire.clone()))
            .await
            .map(|report| Json(crate::tools::describe::envelope(report)));
        finish(outcome)
    }

    /// `vault.validate` — validate vault graph facts and configured rules.
    #[tool(
        name = "vault.validate",
        description = "Validate vault graph facts and configured frontmatter/link rules; returns structured findings.",
        output_schema = output_schema_for::<crate::tools::validate::ValidateOutput>()
    )]
    async fn validate(
        &self,
        Parameters(p): Parameters<crate::tools::validate::ValidateParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let summary_requested = p.summary;
        let wire = crate::tools::validate::to_wire(p);
        let outcome = self
            .call(move |s| s.validate(wire.clone()))
            .await
            .and_then(
                |report| match crate::tools::validate::envelope(report, summary_requested) {
                    Ok(out) => Ok(Json(out)),
                    Err(e) => Err(CallError::Fault(rmcp::ErrorData::internal_error(e, None))),
                },
            );
        finish(outcome)
    }

    /// `vault.repair` — build the standards-repair plan without applying it.
    #[tool(
        name = "vault.repair",
        description = "Build the deterministic standards-repair MigrationPlan from validation findings WITHOUT applying it (read-only). Feed the returned plan to vault.apply to execute it.",
        output_schema = output_schema_for::<crate::tools::repair::RepairOutput>()
    )]
    async fn repair(
        &self,
        Parameters(p): Parameters<crate::tools::repair::RepairParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::repair::to_wire(p);
        let outcome = self
            .call(move |s| s.repair(wire.clone()))
            .await
            .map(|report| Json(crate::tools::repair::envelope(report)));
        finish(outcome)
    }
}

#[tool_router(router = mutate_router, vis = "pub(crate)")]
impl McpServer {
    /// `vault.set` — update one document's frontmatter (and optionally its body).
    #[tool(
        name = "vault.set",
        description = "Update one document's frontmatter (and optionally replace its body), schema-aware. DRY-RUN by default (confirm:false): reports outcome:\"forecast\" and writes nothing. Pass confirm:true to apply and get outcome:\"applied\"/\"refused\".",
        output_schema = output_schema_for::<crate::tools::set::SetOutput>()
    )]
    async fn set(
        &self,
        Parameters(p): Parameters<crate::tools::set::SetParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let confirm = p.confirm;
        let wire = crate::tools::set::to_wire(p);
        let outcome = self
            .call(move |s| s.set(wire.clone()))
            .await
            .map(|report| crate::tools::set::envelope(confirm, report));
        finish(outcome)
    }

    /// `vault.new` — create a document (explicit path, by rule, or inbox).
    #[tool(
        name = "vault.new",
        description = "Create a document — explicit path, by named rule, or inbox — with schema defaults and {{seq}} allocation. DRY-RUN by default (confirm:false): reports outcome:\"forecast\" and writes nothing. Pass confirm:true to apply and get outcome:\"applied\"/\"refused\".",
        output_schema = output_schema_for::<crate::tools::new::NewOutput>()
    )]
    async fn new_document(
        &self,
        Parameters(p): Parameters<crate::tools::new::NewParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let confirm = p.confirm;
        let wire = crate::tools::new::to_wire(p);
        let outcome = self
            .call(move |s| s.new_document(wire.clone()))
            .await
            .map(|report| crate::tools::new::envelope(confirm, report));
        finish(outcome)
    }

    /// `vault.edit` — apply section/text edit ops to one document's body.
    #[tool(
        name = "vault.edit",
        description = "Apply structured section/text edit ops to one document's body, with an optional expected_hash compare-and-swap. DRY-RUN by default (confirm:false): reports outcome:\"forecast\" and writes nothing. Pass confirm:true to apply and get outcome:\"applied\"/\"refused\".",
        output_schema = output_schema_for::<crate::tools::edit::EditOutput>()
    )]
    async fn edit(
        &self,
        Parameters(p): Parameters<crate::tools::edit::EditParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let confirm = p.confirm;
        let wire = crate::tools::edit::to_wire(p);
        let outcome = self
            .call(move |s| s.edit(wire.clone()))
            .await
            .map(|report| crate::tools::edit::envelope(confirm, report));
        finish(outcome)
    }

    /// `vault.move` — relocate a document/folder and cascade-rewrite backlinks.
    #[tool(
        name = "vault.move",
        description = "Relocate a document (or folder, recursive:true) and cascade-rewrite its backlinks. DRY-RUN by default (confirm:false): reports dry_run:true (with outcome:\"applied\" pending NRN-161) and writes nothing. Pass confirm:true to apply.",
        output_schema = output_schema_for::<crate::tools::move_doc::MoveOutput>()
    )]
    async fn move_document(
        &self,
        Parameters(p): Parameters<crate::tools::move_doc::MoveParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::move_doc::to_wire(p);
        let outcome = self
            .call(move |s| s.move_document(wire.clone()))
            .await
            .map(crate::tools::move_doc::envelope);
        finish(outcome)
    }

    /// `vault.delete` — remove a document, leaving or redirecting its backlinks.
    #[tool(
        name = "vault.delete",
        description = "Delete a document, either leaving its incoming links broken (allow_broken_links:true) or redirecting them (rewrite_to). The owner stamps the target's current content hash as a required plan-time precondition (protects against owner-index-vs-disk skew); a client-chosen CAS instead goes through vault.apply with a plan-carried document_hash. DRY-RUN by default (confirm:false): reports dry_run:true (with outcome:\"applied\" pending NRN-161) and writes nothing. Pass confirm:true to apply.",
        output_schema = output_schema_for::<crate::tools::delete::DeleteOutput>()
    )]
    async fn delete(
        &self,
        Parameters(p): Parameters<crate::tools::delete::DeleteParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::delete::to_wire(p);
        let outcome = self
            .call(move |s| s.delete(wire.clone()))
            .await
            .map(crate::tools::delete::envelope);
        finish(outcome)
    }

    /// `vault.rewrite_wikilink` — rewrite every `[[old]]` reference to `[[new]]`.
    #[tool(
        name = "vault.rewrite_wikilink",
        description = "Rewrite every [[old]] wikilink reference (body + frontmatter) to [[new]] across the vault. DRY-RUN by default (confirm:false): reports dry_run:true (with outcome:\"applied\" pending NRN-161) and writes nothing. Pass confirm:true to apply.",
        output_schema = output_schema_for::<crate::tools::rewrite_wikilink::RewriteWikilinkOutput>()
    )]
    async fn rewrite_wikilink(
        &self,
        Parameters(p): Parameters<crate::tools::rewrite_wikilink::RewriteWikilinkParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = crate::tools::rewrite_wikilink::to_wire(p);
        let outcome = self
            .call(move |s| s.rewrite_wikilink(wire.clone()))
            .await
            .map(crate::tools::rewrite_wikilink::envelope);
        finish(outcome)
    }

    /// `vault.apply` — execute an already-reviewed MigrationPlan.
    #[tool(
        name = "vault.apply",
        description = "Execute an already-reviewed MigrationPlan (e.g. one returned by vault.repair). DRY-RUN by default (confirm:false): reports dry_run:true (with outcome:\"applied\" pending NRN-161) and writes nothing. Pass confirm:true to apply.",
        output_schema = output_schema_for::<crate::tools::apply::ApplyOutput>()
    )]
    async fn apply(
        &self,
        Parameters(p): Parameters<crate::tools::apply::ApplyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let wire = match crate::tools::apply::to_wire(p) {
            Ok(wire) => wire,
            // A malformed plan argument never reaches the owner — it is a
            // structured rejection (a well-formed tool call with bad contents).
            Err(message) => return Ok(rejected_result(message, Vec::new())),
        };
        let outcome = self
            .call(move |s| s.apply(wire.clone()))
            .await
            .map(crate::tools::apply::envelope);
        finish(outcome)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (alias for `InitializeResult`) is `#[non_exhaustive]` in
        // rmcp, so start from `Default` and override the fields we care about.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        // Identify as "norn" at the crate version — `Default` inherits rmcp's own
        // build env (name="rmcp"), so set it explicitly.
        info.server_info = Implementation::new("norn", env!("CARGO_PKG_VERSION"));
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_renders_a_structured_error_tool_result_with_code_and_hints() {
        // B2: an owner Rejected becomes a STRUCTURED tool result (isError:true,
        // `{code, message, hints}`), not a JSON-RPC transport error — hints pass
        // through verbatim.
        let outcome: Result<Json<()>, CallError> = Err(CallError::Rejected {
            message: "bad predicate".into(),
            hints: vec!["did you mean `status`?".into()],
        });
        let result = finish(outcome).expect("Rejected is a tool RESULT, never an Err(ErrorData)");
        assert_eq!(result.is_error, Some(true));
        let sc = result
            .structured_content
            .expect("the structured refusal body is present");
        assert_eq!(sc["code"], "rejected");
        assert_eq!(sc["message"], "bad predicate");
        assert_eq!(sc["hints"][0], "did you mean `status`?");
    }

    #[test]
    fn fault_propagates_as_a_jsonrpc_error() {
        // A transport/owner fault is a JSON-RPC error, never a tool result — the
        // tool could not run, so there is nothing to return in-band.
        let outcome: Result<Json<()>, CallError> = Err(CallError::Fault(
            rmcp::ErrorData::internal_error("owner went away", None),
        ));
        assert!(
            finish(outcome).is_err(),
            "a Fault must propagate as a JSON-RPC error"
        );
    }

    #[test]
    fn classify_maps_rejected_to_the_structured_path_and_others_to_fault() {
        assert!(matches!(
            classify_client_error(ClientError::Rejected {
                message: "m".into(),
                hints: vec![],
            }),
            CallError::Rejected { .. }
        ));
        assert!(matches!(
            classify_client_error(ClientError::OwnerGone("gone".into())),
            CallError::Fault(_)
        ));
    }

    /// A stand-in "session" recording how many times the attempt + resummon ran.
    #[derive(Default)]
    struct FakeSession {
        attempts: u32,
        resummons: u32,
    }

    /// Drive [`retry_once_on_pre_send`] with a scripted first-attempt error.
    /// `first_err` is the error the FIRST attempt returns; the second attempt
    /// (if reached) succeeds. Returns (result, attempts, resummons).
    fn drive(
        first_err: fn() -> ClientError,
        second_ok: bool,
    ) -> (Result<u8, ClientError>, u32, u32) {
        let mut s = FakeSession::default();
        let r = retry_once_on_pre_send(
            &mut s,
            |s: &mut FakeSession| {
                s.attempts += 1;
                if s.attempts == 1 {
                    Err(first_err())
                } else if second_ok {
                    Ok(42u8)
                } else {
                    Err(first_err())
                }
            },
            |s: &mut FakeSession| {
                s.resummons += 1;
                Ok(())
            },
        );
        (r, s.attempts, s.resummons)
    }

    #[test]
    fn pre_send_owner_gone_resummons_and_retries_once() {
        let (r, attempts, resummons) =
            drive(|| ClientError::OwnerGonePreSend("reaped".into()), true);
        assert_eq!(r.unwrap(), 42, "the retry after resummon succeeds");
        assert_eq!(attempts, 2, "the request is attempted exactly twice");
        assert_eq!(resummons, 1, "the owner is resummoned exactly once");
    }

    #[test]
    fn post_send_owner_gone_is_never_retried() {
        // ADR 0011: a written-but-unanswered request may have applied — it must
        // propagate WITHOUT a resummon/retry (a mutation must never double-apply).
        let (r, attempts, resummons) = drive(|| ClientError::OwnerGone("mid-reply".into()), true);
        assert!(
            matches!(r, Err(ClientError::OwnerGone(_))),
            "a post-send owner-gone propagates as-is"
        );
        assert_eq!(attempts, 1, "the request is attempted exactly once");
        assert_eq!(resummons, 0, "a post-send failure NEVER resummons");
    }

    #[test]
    fn unrelated_error_is_never_retried() {
        let (r, attempts, resummons) = drive(
            || ClientError::Rejected {
                message: "bad predicate".into(),
                hints: vec![],
            },
            true,
        );
        assert!(matches!(r, Err(ClientError::Rejected { .. })));
        assert_eq!(attempts, 1);
        assert_eq!(resummons, 0);
    }

    #[test]
    fn pre_send_twice_retries_only_once_then_propagates() {
        let (r, attempts, resummons) =
            drive(|| ClientError::OwnerGonePreSend("still gone".into()), false);
        assert!(
            matches!(r, Err(ClientError::OwnerGonePreSend(_))),
            "the second pre-send failure propagates — retry is bounded to once"
        );
        assert_eq!(attempts, 2, "attempted twice, never a third time");
        assert_eq!(resummons, 1, "resummoned once");
    }

    #[test]
    fn success_on_first_try_never_resummons() {
        let mut s = FakeSession::default();
        let r = retry_once_on_pre_send(
            &mut s,
            |s: &mut FakeSession| {
                s.attempts += 1;
                Ok(7u8)
            },
            |s: &mut FakeSession| {
                s.resummons += 1;
                Ok(())
            },
        );
        assert_eq!(r.unwrap(), 7);
        assert_eq!(s.attempts, 1);
        assert_eq!(s.resummons, 0);
    }
}
