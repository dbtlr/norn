//! MCP tool surfaces — one module per tool.
//!
//! Each module owns two things: the tool's MCP-facing **param struct** (a
//! `schemars::JsonSchema` deriver whose generated schema IS the `tools/list`
//! `inputSchema` — the published MCP contract) and, where the MCP
//! output envelope differs from the verb's raw
//! wire `Report`, the **output struct** plus the pure `to_wire` / `envelope`
//! mappers. The `#[tool]` methods themselves live on `McpServer` in `server.rs`
//! (rmcp requires them on the router impl block); each is a thin wrapper that
//! builds the wire `Params` here, runs the routed owner request, and maps the
//! `Report` back through here.

pub mod apply;
pub mod count;
pub mod delete;
pub mod describe;
pub mod edit;
pub mod find;
pub mod get;
pub mod move_doc;
pub mod new;
pub mod repair;
pub mod rewrite_wikilink;
pub mod set;
pub mod validate;
