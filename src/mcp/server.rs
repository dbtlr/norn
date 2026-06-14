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
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, tool_router, ServerHandler};

use super::context::VaultContext;

#[derive(Clone)]
pub struct McpServer {
    /// Warm vault context: config held for the server lifetime; cache opened
    /// fresh per tool call via `self.ctx.query_cache()`.
    #[allow(dead_code)] // used by tool implementations added in later tasks
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

// No `#[tool]` methods yet → the generated `tool_router()` builds an empty
// router, so `tools/list` returns zero tools.
#[tool_router]
impl McpServer {}

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
