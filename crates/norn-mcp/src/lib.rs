#![forbid(unsafe_code)]
//! Thin MCP adapter: stdio framing to Params, typed Reports back out.
//!
//! May never: Contain verb logic; handlers' semantics live behind the wire.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str = "norn-mcp: thin MCP adapter — parse and present only";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_client::CONTRACT, norn_wire::CONTRACT];
