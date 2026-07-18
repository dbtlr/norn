#![forbid(unsafe_code)]
//! Summoner, connector, warm-up progress surface, send-commit boundary. The only crate that spawns owners or dials sockets client-side.
//!
//! May never: Open caches, or depend on norn-core.

/// One-line boundary contract, referenced by every dependent so each
/// declared edge in the crate map is compiler-load-bearing.
pub const CONTRACT: &str =
    "norn-client: summoner and connector — the only crate that spawns owners or dials sockets";

/// Direct-dependency contracts — the code reference that makes this
/// crate's declared edges load-bearing rather than manifest-only.
pub const DEP_CONTRACTS: &[&str] = &[norn_wire::CONTRACT, norn_config::CONTRACT];
