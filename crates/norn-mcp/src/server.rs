//! The MCP server handler — the thin rmcp adapter over a held [`OwnerSession`].
//!
//! Each `#[tool]` method deserializes MCP args, builds the verb's norn-wire
//! `Params`, runs the SAME routed owner request the CLI runs (via [`call`]), and
//! maps the returned `Report` into the MCP output envelope. No verb logic lives
//! here.
//!
//! The tools are split into two `#[tool_router]` blocks — `read_router` (the read
//! tools) and `mutate_router` (the mutation tools) — merged by [`McpServer::new`]
//! into one served surface, mirroring the donor's structure.

use std::sync::{Arc, Mutex};

use norn_client::{ClientError, OwnerSession};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::mutation_result::{output_schema_for, MutationResult};

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
    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T, rmcp::ErrorData>
    where
        T: Send + 'static,
        F: FnOnce(&mut OwnerSession) -> Result<T, ClientError> + Send + 'static,
    {
        let session = Arc::clone(&self.session);
        let joined = tokio::task::spawn_blocking(move || {
            let mut guard = session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&mut guard)
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
        let report = self.call(move |s| s.count(wire)).await?;
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
        let report = self.call(move |s| s.get(wire)).await?;
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
        let report = self.call(move |s| s.validate(wire)).await?;
        Ok(Json(crate::tools::validate::envelope(report, summary_requested)))
    }
}

#[tool_router(router = mutate_router, vis = "pub(crate)")]
impl McpServer {
    #[allow(unused)]
    fn _mutate_router_anchor(&self) {}
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
