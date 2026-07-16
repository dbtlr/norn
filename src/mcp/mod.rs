//! `norn mcp` — a Model Context Protocol stdio server over the vault.
//!
//! Task 1 is the scaffold: build a tokio runtime, construct the (toolless)
//! [`McpServer`], serve it over stdio, and wait. Later tasks add the actual
//! read/mutation tools.
//!
//! Task 2 adds a warm [`VaultContext`] that the server holds across tool calls:
//! config is parsed once at startup; the cache is re-opened per tool call so
//! each call gets the CLI's per-invocation freshness check without a filesystem
//! watcher.
//!
//! The vault env seam itself — [`VaultContext`](crate::env::VaultContext) and
//! [`RequestScope`](crate::env::RequestScope) — lives in [`crate::env`], not
//! here. It is surface-neutral (the CLI direct path will adopt it too); this
//! module keeps only the MCP adapter concerns (server, tools, writer queue).

pub mod mutate;
pub mod mutation_result;
pub mod notes;
pub mod server;
pub mod tools;
/// Per-vault writer queue (ADR 0013 Phase 2, NRN-252). All warm-mode write work
/// routes through it: generation opens and the per-request freshness refresh as
/// LIVENESS ops (latency-critical, a reader is blocked on them), and the
/// post-apply cache-increment commit as a chunked BULK op.
pub mod writer_queue;

/// CLI↔MCP surface-parity forcing function (NRN-178). A `#[cfg(test)]` gate that
/// fails the build when a CLI flag has no MCP twin (or vice versa) without a
/// justified carve-out. Lives here (in-crate) so it can read the exact tool
/// schemas the server serves via the crate-visible `server::McpServer` routers.
#[cfg(test)]
mod parity_gate;

use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

/// Map an `anyhow::Error` to an rmcp internal-error response carrying BOTH the
/// human message and a structured `data` envelope (NRN-150).
///
/// Shared by every `#[tool]` wrapper: the pure handlers return `anyhow::Result`,
/// and this is the single seam that turns a failure into a JSON-RPC error. The
/// `data` payload is the `{ code, message, path? }` error envelope — recovered by
/// downcasting through the known failure types (`ApplyError`, `ContainmentError`,
/// `CacheError`), falling back to `internal-error`. A consumer branches on
/// `error.data.code`, never on the prose. (Validation-phase precondition refusals
/// on the mutation tools never reach here — those return a report-on-refusal with
/// the offending op `failed`; see `ApplyContext::refuse_as_report`.)
pub(crate) fn to_mcp_error(e: anyhow::Error) -> rmcp::ErrorData {
    let envelope = crate::apply_report::ApplyError::from_anyhow(&e);
    let data = serde_json::to_value(&envelope).ok();
    rmcp::ErrorData::internal_error(e.to_string(), data)
}

use self::server::McpServer;
use crate::env::VaultContext;

/// Run the MCP stdio server. Owns its own multi-thread tokio runtime and blocks
/// until the client disconnects. Fails fast with a non-zero exit if the vault
/// context cannot be opened.
pub fn run(
    _args: &crate::cli::McpArgs,
    cwd: &Utf8PathBuf,
    config_path: Option<&Utf8PathBuf>,
) -> anyhow::Result<()> {
    // Build vault context before entering the async runtime — any config error
    // surfaces here as a clean anyhow chain before the server starts listening.
    let ctx = VaultContext::open(cwd, config_path)
        .with_context(|| format!("failed to open vault at {cwd}"))?;
    let ctx = Arc::new(ctx);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(ctx))
}

async fn serve(ctx: Arc<VaultContext>) -> anyhow::Result<()> {
    let service = McpServer::new(ctx).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
