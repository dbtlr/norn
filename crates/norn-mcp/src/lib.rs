#![forbid(unsafe_code)]
//! Thin MCP adapter: stdio framing to Params, typed Reports back out.
//!
//! May never: Contain verb logic; handlers' semantics live behind the wire.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str =
    "norn-mcp: Thin MCP adapter: stdio framing to Params, typed Reports back out.";
