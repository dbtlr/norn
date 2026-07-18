//! MCP tool implementations.
//!
//! Each tool is split into two layers:
//!
//! 1. A **pure handler** (`handle`) plus its param struct, here in `tools/`.
//!    The handler takes `&VaultEnv` and the deserialized params, runs the
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

/// NRN-253 test-shim seam: generate scope-threading wrappers for a tool's
/// handlers, so existing two-arg test call sites (`handle(&ctx, p)`) compile
/// unchanged while production threads the request's [`RequestScope`] from
/// `run_wrapped`. Each generated `fn $name(ctx, p)` opens a fresh single-use
/// scope via `begin_request` and forwards to the module's real `super::$name(ctx,
/// &scope, p)`. ONE definition instead of a hand-rolled copy per tool test
/// module (the `query_cache_unscoped` precedent, applied to handlers).
///
/// [`RequestScope`]: crate::env::RequestScope
#[cfg(test)]
macro_rules! scoped_shim {
    ($(fn $name:ident($params:ty) -> $ret:ty;)+) => {
        $(
            fn $name(
                ctx: &$crate::env::VaultEnv,
                p: $params,
            ) -> anyhow::Result<$ret> {
                let scope = ctx.begin_request()?;
                super::$name(ctx, &scope, p)
            }
        )+
    };
}
#[cfg(test)]
pub(crate) use scoped_shim;

pub mod apply;
pub mod audit;
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
