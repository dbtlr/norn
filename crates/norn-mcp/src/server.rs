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
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::mutation_result::{output_schema_for, MutationResult};

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

/// Map a summoner/owner [`ClientError`] onto an rmcp error. A tool that cannot
/// even reach the owner (or whose owner rejected/failed the request) surfaces as a
/// JSON-RPC error, not a tool result. A structured owner `Rejected` carries its
/// message; every other variant renders its `Display`.
pub(crate) fn client_error_to_mcp(e: ClientError) -> rmcp::ErrorData {
    match e {
        ClientError::Rejected { message, .. } => rmcp::ErrorData::internal_error(message, None),
        other => rmcp::ErrorData::internal_error(other.to_string(), None),
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
    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T, rmcp::ErrorData>
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
            Ok(Err(err)) => Err(client_error_to_mcp(err)),
            Err(join_err) => Err(rmcp::ErrorData::internal_error(
                format!("tool task failed: {join_err}"),
                None,
            )),
        }
    }
}

#[tool_router(router = read_router, vis = "pub(crate)")]
impl McpServer {
    /// `vault.count` — count documents in the vault, total or grouped.
    #[tool(
        name = "vault.count",
        description = "Count documents in the vault — total, or grouped by a frontmatter field — with the find filter surface."
    )]
    async fn count(
        &self,
        Parameters(p): Parameters<crate::tools::count::CountParams>,
    ) -> Result<Json<crate::tools::count::CountEnvelope>, rmcp::ErrorData> {
        let wire = crate::tools::count::to_wire(p);
        let report = self.call(move |s| s.count(wire.clone())).await?;
        Ok(Json(crate::tools::count::envelope(report)))
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
    ) -> Result<MutationResult<crate::tools::get::GetOutput>, rmcp::ErrorData> {
        let wire = crate::tools::get::to_wire(&p);
        let report = self.call(move |s| s.get(wire.clone())).await?;
        Ok(crate::tools::get::envelope(&p, report))
    }

    /// `vault.validate` — validate vault graph facts and configured rules.
    #[tool(
        name = "vault.validate",
        description = "Validate vault graph facts and configured frontmatter/link rules; returns structured findings."
    )]
    async fn validate(
        &self,
        Parameters(p): Parameters<crate::tools::validate::ValidateParams>,
    ) -> Result<Json<crate::tools::validate::ValidateOutput>, rmcp::ErrorData> {
        let summary_requested = p.summary;
        let wire = crate::tools::validate::to_wire(p);
        let report = self.call(move |s| s.validate(wire.clone())).await?;
        let out = crate::tools::validate::envelope(report, summary_requested)
            .map_err(|e| rmcp::ErrorData::internal_error(e, None))?;
        Ok(Json(out))
    }
}

#[tool_router(router = mutate_router, vis = "pub(crate)")]
impl McpServer {
    /// `vault.set` — update one document's frontmatter (and optionally its body).
    #[tool(
        name = "vault.set",
        description = "Update one document's frontmatter (and optionally replace its body), schema-aware. DRY-RUN by default — returns the planned change without writing. Pass confirm:true to apply.",
        output_schema = output_schema_for::<crate::tools::set::SetOutput>()
    )]
    async fn set(
        &self,
        Parameters(p): Parameters<crate::tools::set::SetParams>,
    ) -> Result<MutationResult<crate::tools::set::SetOutput>, rmcp::ErrorData> {
        let confirm = p.confirm;
        let wire = crate::tools::set::to_wire(p);
        let report = self.call(move |s| s.set(wire.clone())).await?;
        Ok(crate::tools::set::envelope(confirm, report))
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
