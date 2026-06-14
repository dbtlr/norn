//! Smoke test for the `norn mcp` server scaffold (NRN-33, Task 1).
//!
//! Drives the `McpServer` with the rmcp client over an in-process
//! `tokio::io::duplex` transport (no child process — faster and hermetic).
//! `ServiceExt::serve` performs the MCP `initialize` handshake; we then call
//! `list_all_tools` (the `tools/list` RPC) and assert an EMPTY tool list, which
//! is the whole contract of the Task 1 scaffold.

use rmcp::{serve_client, ServiceExt};

// The MCP server lives in the binary crate; pull the module in directly so the
// test does not depend on a `lib` target.
#[path = "../src/mcp/server.rs"]
mod server;

use server::McpServer;

#[tokio::test]
async fn server_initializes_and_lists_zero_tools() -> anyhow::Result<()> {
    let (server_transport, client_transport) = tokio::io::duplex(4096);

    // Server: serve the scaffold over the in-process transport, then wait.
    let server_handle = tokio::spawn(async move {
        let running = McpServer::new().serve(server_transport).await?;
        running.waiting().await?;
        anyhow::Ok(())
    });

    // Client: `()` is a no-op ClientHandler. `serve_client` runs the initialize
    // handshake; a successful return means we got a well-formed InitializeResult.
    // (`serve_client` over `().serve()` to disambiguate the client role — `()`
    // alone leaves `ServiceExt::serve` unable to pick RoleClient vs RoleServer.)
    let client = serve_client((), client_transport).await?;

    // The handshake gave us the server's advertised info; tools capability is on.
    let peer_info = client
        .peer_info()
        .expect("server must send InitializeResult");
    assert!(
        peer_info.capabilities.tools.is_some(),
        "scaffold should advertise the tools capability, got {:?}",
        peer_info.capabilities
    );

    // tools/list — the heart of the scaffold contract.
    let tools = client.list_all_tools().await?;
    assert!(
        tools.is_empty(),
        "Task 1 scaffold must expose zero tools, got {} ({:?})",
        tools.len(),
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>()
    );

    // Clean shutdown so the spawned server task can exit.
    client.cancel().await?;
    server_handle.await??;
    Ok(())
}
