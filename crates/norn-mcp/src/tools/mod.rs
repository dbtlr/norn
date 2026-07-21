//! MCP tool surfaces — one module per tool.
//!
//! Each module owns two things: the tool's MCP-facing **param struct** (a
//! `schemars::JsonSchema` deriver whose generated schema IS the `tools/list`
//! `inputSchema` — the published MCP contract; today's tools are pinned by the
//! per-tool `mcp` parity cases, and a full `tools/list` case lands once the
//! catalog completes, per the adjudicated ruling on NRN-400) and, where the MCP
//! output envelope differs from the verb's raw
//! wire `Report`, the **output struct** plus the pure `to_wire` / `envelope`
//! mappers. The `#[tool]` methods themselves live on `McpServer` in `server.rs`
//! (rmcp requires them on the router impl block); each is a thin wrapper that
//! builds the wire `Params` here, runs the routed owner request, and maps the
//! `Report` back through here.

pub mod count;
pub mod get;
pub mod set;
pub mod validate;
