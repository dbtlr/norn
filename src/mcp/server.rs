//! The MCP server handler.
//!
//! Task 1 is the scaffold: an empty tool router with zero `#[tool]` methods, so
//! `tools/list` answers with an empty array. Later tasks add `#[tool]` methods to
//! the `#[tool_router]` impl block below.
//!
//! Task 2 wires in a warm [`VaultContext`] so tool implementations can call
//! `self.ctx.query_cache()` to open a fresh cache handle on each invocation —
//! getting the CLI's per-invocation freshness check without a filesystem watcher.
//!
//! We use the explicit `#[tool_handler(router = self.tool_router)]` form (rather
//! than `#[tool_router(server_handler)]`) so the generated `ServerHandler`
//! dispatches through the *instance* `tool_router` field. The `server_handler`
//! convenience variant instead routes through a fresh `Self::tool_router()` each
//! call, which would leave the field unread and trip `-D dead_code`.

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use super::context::VaultContext;
use super::to_mcp_error;
use crate::mcp::tools::count::CountEnvelope;
use crate::mcp::tools::find::FindOutput;
use crate::mcp::tools::get::GetOutput;
use crate::mcp::tools::repair_plan::RepairPlanOutput;
use crate::mcp::tools::validate::ValidateOutput;

#[derive(Clone)]
pub struct McpServer {
    /// Warm vault context: config held for the server lifetime; cache opened
    /// fresh per tool call via `self.ctx.query_cache()`.
    pub(crate) ctx: Arc<VaultContext>,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    pub fn new(ctx: Arc<VaultContext>) -> Self {
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl McpServer {
    /// `vault.get` — fetch one or more documents with full connection context.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::get`; this only bridges rmcp ↔ `anyhow`. The
    /// returned [`GetOutput`] is a typed envelope whose root schema is `object`
    /// (rmcp rejects a non-object `outputSchema`); see `tools::get` for why the
    /// per-record payload stays generic JSON rather than a full `JsonSchema`
    /// derive across the core types. Later read tools copy this thin shape.
    #[tool(
        name = "vault.get",
        description = "Fetch one or more documents: frontmatter, headings, outgoing/incoming/unresolved links, optionally body."
    )]
    async fn get(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::get::GetParams>,
    ) -> Result<Json<GetOutput>, rmcp::ErrorData> {
        crate::mcp::tools::get::handle_output(&self.ctx, p)
            .map(Json)
            .map_err(to_mcp_error)
    }

    /// `vault.count` — count documents in the vault, total or grouped.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::count`; this only bridges rmcp ↔ `anyhow`. The
    /// returned [`CountEnvelope`] is a typed flat object whose root schema is
    /// `type: object` (rmcp rejects non-object `outputSchema`). See `tools::count`
    /// for why `CountOutput`'s untagged enum is projected into the envelope.
    #[tool(
        name = "vault.count",
        description = "Count documents in the vault — total, or grouped by a frontmatter field — with the find filter surface."
    )]
    async fn count(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::count::CountParams>,
    ) -> Result<Json<CountEnvelope>, rmcp::ErrorData> {
        crate::mcp::tools::count::handle(&self.ctx, p)
            .map(Json)
            .map_err(to_mcp_error)
    }

    /// `vault.find` — full-text + metadata document search.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::find`, which runs the shared `find::query`
    /// seam (the same selection/JSON path behind `norn find --format json`), so
    /// the MCP tool and the CLI can't drift on filtering, sort, limit, or `--col`.
    /// The returned [`FindOutput`] is a typed envelope with a `type: object` root
    /// (rmcp rejects a non-object `outputSchema`); the per-document payload stays
    /// generic JSON, matching the `vault.get` shape.
    #[tool(
        name = "vault.find",
        description = "Find documents in the vault — full-text + metadata filters with sort, limit, and paging."
    )]
    async fn find(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::find::FindParams>,
    ) -> Result<Json<FindOutput>, rmcp::ErrorData> {
        crate::mcp::tools::find::handle(&self.ctx, p)
            .map(Json)
            .map_err(to_mcp_error)
    }

    /// `vault.validate` — validate vault graph facts and configured frontmatter/link rules.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::validate`, which drives the same pipeline as
    /// `norn validate` (cache → graph index → `validate_with_compiled` →
    /// `filter_findings`) and returns findings as serialized JSON values in the
    /// [`ValidateOutput`] envelope. The envelope root is `type: object` (rmcp
    /// rejects a non-object `outputSchema`); per-finding payload stays generic
    /// JSON because `Finding` carries `Utf8PathBuf` which has no `JsonSchema` impl.
    #[tool(
        name = "vault.validate",
        description = "Validate vault graph facts and configured frontmatter/link rules; returns structured findings."
    )]
    async fn validate(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::validate::ValidateParams>,
    ) -> Result<Json<ValidateOutput>, rmcp::ErrorData> {
        crate::mcp::tools::validate::handle(&self.ctx, p)
            .map(Json)
            .map_err(to_mcp_error)
    }

    /// `vault.repair_plan` — produce a deterministic MigrationPlan without applying it.
    ///
    /// Thin wrapper: deserialize params, call the pure handler, map the result.
    /// All logic lives in `tools::repair_plan`, which drives the same pipeline as
    /// `norn repair --plan` (cache → graph index → findings → `plan_from_findings`)
    /// and returns the in-memory `MigrationPlan` serialized as `serde_json::Value`
    /// in the [`RepairPlanOutput`] envelope. The plan JSON is identical to what
    /// `norn repair --plan --format json` emits — `vault.apply_plan` (Task 12)
    /// can consume it unchanged. The tool is READ-ONLY: it never writes files,
    /// never calls the applier, and never mutates the vault.
    #[tool(
        name = "vault.repair_plan",
        description = "Produce a deterministic repair MigrationPlan (closest-match link rewrites, frontmatter fixes) without applying it. Feed the plan to vault.apply_plan to execute."
    )]
    async fn repair_plan(
        &self,
        Parameters(p): Parameters<crate::mcp::tools::repair_plan::RepairPlanParams>,
    ) -> Result<Json<RepairPlanOutput>, rmcp::ErrorData> {
        crate::mcp::tools::repair_plan::handle(&self.ctx, p)
            .map(Json)
            .map_err(to_mcp_error)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` (alias for `InitializeResult`) is `#[non_exhaustive]` in
        // rmcp 1.7.0, so the struct-literal form from the plan snippet does not
        // compile — start from `Default` and override the tools capability.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}
