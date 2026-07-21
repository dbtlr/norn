#![forbid(unsafe_code)]
//! Thin MCP adapter: stdio framing to Params, typed Reports back out.
//!
//! `norn mcp` speaks newline-delimited JSON-RPC (the Model Context Protocol) over
//! stdio. This crate is the adapter that turns each MCP tool call into a norn-wire
//! `Params` request against the vault's warm owner (via a held [`OwnerSession`])
//! and maps the returned `Report` back into the MCP output envelope. Every tool
//! handler is a thin wrapper — parse MCP args → the verb's wire `Params` → the
//! same routed owner the CLI uses → map the `Report` into the MCP result. There is
//! no second verb implementation here: the semantics live behind the wire.
//!
//! May never: Contain verb logic; handlers' semantics live behind the wire.

use std::sync::{Arc, Mutex};

use norn_client::OwnerSession;

mod mutation_result;
mod server;
mod tools;

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-mcp: thin MCP adapter — parse and present only";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_client::CONTRACT, norn_wire::CONTRACT];

/// Serve the MCP stdio protocol over an already-resolved, ready [`OwnerSession`].
///
/// The caller (the CLI's `mcp` dispatch arm) owns vault resolution and summon —
/// it hands a live session here, exactly as it does for a read verb. This owns a
/// small multi-thread tokio runtime (rmcp's server is async) and blocks until the
/// client closes stdin, at which point the stdio transport reaches EOF and the
/// service completes with a clean exit.
pub fn serve_stdio(session: OwnerSession) -> anyhow::Result<()> {
    let session = Arc::new(Mutex::new(session));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(session))
}

async fn serve(session: Arc<Mutex<OwnerSession>>) -> anyhow::Result<()> {
    use rmcp::transport::io::stdio;
    use rmcp::ServiceExt;

    let service = server::McpServer::new(session).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
