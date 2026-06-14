//! `norn mcp` — a Model Context Protocol stdio server over the vault.
//!
//! Task 1 is the scaffold: build a tokio runtime, construct the (toolless)
//! [`McpServer`], serve it over stdio, and wait. Later tasks add the actual
//! read/mutation tools and wire `--read-only` to the exposed tool set.
//!
//! Task 2 adds a warm [`VaultContext`] that the server holds across tool calls:
//! config is parsed once at startup; the cache is re-opened per tool call so
//! each call gets the CLI's per-invocation freshness check without a filesystem
//! watcher.

pub mod context;
pub mod server;

use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

use self::context::VaultContext;
use self::server::McpServer;

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
