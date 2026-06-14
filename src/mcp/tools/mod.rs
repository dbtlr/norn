//! MCP tool implementations.
//!
//! Each tool is split into two layers:
//!
//! 1. A **pure handler** (`handle`) plus its param struct, here in `tools/`.
//!    The handler takes `&VaultContext` and the deserialized params, runs the
//!    same underlying norn code path the CLI uses, and returns the report type
//!    directly (`anyhow::Result<…>`). It carries no rmcp machinery, so it is
//!    unit-testable against a seeded temp vault with the `pub(crate)` cache
//!    types in scope.
//! 2. A **thin `#[tool]` wrapper** on the `#[tool_router] impl McpServer` block
//!    in `server.rs`, which only deserializes params, calls `handle`, wraps the
//!    result in [`rmcp::handler::server::wrapper::Json`], and maps the error via
//!    `to_mcp_error`.
//!
//! `vault.get` (this module) establishes the pattern the later read tools copy.

pub mod count;
pub mod describe;
pub mod find;
pub mod get;
pub mod move_doc;
pub mod new;
pub mod repair_plan;
pub mod set;
pub mod validate;
