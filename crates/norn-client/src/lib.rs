#![forbid(unsafe_code)]
//! Summoner, connector, warm-up progress surface, send-commit boundary. The only crate that spawns owners or dials sockets client-side.
//!
//! May never: Open caches, or depend on norn-core.

/// One-line boundary contract, referenced by the bin so every edge in the
/// crate map is a real, compiler-checked dependency.
pub const CONTRACT: &str =
    "norn-client: Summoner, connector, warm-up progress surface, send-commit boundary.";
