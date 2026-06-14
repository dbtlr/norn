//! `norn mcp` — a Model Context Protocol stdio server over the vault.
//!
//! Task 1 is the scaffold: build a tokio runtime, construct the (toolless)
//! [`McpServer`], serve it over stdio, and wait. Later tasks add the actual
//! read/mutation tools and wire `--read-only` to the exposed tool set.

pub mod server;

use camino::Utf8PathBuf;
use rmcp::transport::io::stdio;
use rmcp::ServiceExt;

use self::server::McpServer;

/// Run the MCP stdio server. Owns its own multi-thread tokio runtime and blocks
/// until the client disconnects.
pub fn run(
    _args: &crate::cli::McpArgs,
    _cwd: &Utf8PathBuf,
    _config_path: Option<&Utf8PathBuf>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve())
}

async fn serve() -> anyhow::Result<()> {
    let service = McpServer::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
